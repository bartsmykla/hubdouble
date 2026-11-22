use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rsa::pkcs1::EncodeRsaPrivateKey as _;
use rsa::pkcs8::{EncodePublicKey as _, LineEnding};
use rsa::{Generate as _, PrivateKey, PublicKey};
use serde_json::{Value, json};

use super::{Result, require};

pub(super) struct AppKey {
    signing: EncodingKey,
    pub public_pem: String,
}

impl AppKey {
    pub(super) fn generate() -> Result<Self> {
        Self::new(&PrivateKey::try_generate().map_err(|_| "Could not generate the App RSA key")?)
    }

    fn new(private: &PrivateKey) -> Result<Self> {
        let der = private.to_pkcs1_der()?;
        Ok(Self {
            signing: EncodingKey::from_rsa_der(der.as_bytes()),
            public_pem: PublicKey::from(private).to_public_key_pem(LineEnding::LF)?,
        })
    }

    pub(super) fn jwt(&self, now: u64) -> Result<String> {
        let issued = now.checked_sub(30).ok_or("App JWT clock is too early")?;
        let claims = json!({"iss": 1_000_001, "iat": issued, "exp": now + 300});
        Ok(jsonwebtoken::encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &self.signing,
        )?)
    }
}

pub(super) fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

pub(super) fn verify(token: &str, jwks: &Value, now: u64) -> Result<Value> {
    let header = jsonwebtoken::decode_header(token)?;
    require(header.alg == Algorithm::RS256, "OIDC token must use RS256")?;
    let kid = header.kid.ok_or("OIDC token has no kid")?;
    let keys = jwks["keys"].as_array().ok_or("JWKS has no keys")?;
    let mut matches = keys.iter().filter(|key| key["kid"] == kid);
    let key = matches.next().ok_or("JWKS has no key for the token kid")?;
    require(matches.next().is_none(), "JWKS has duplicate token keys")?;
    require(
        key["kty"] == "RSA" && key["alg"] == "RS256",
        "Unexpected JWK algorithm",
    )?;
    let n = key["n"].as_str().ok_or("JWK has no modulus")?;
    let e = key["e"].as_str().ok_or("JWK has no exponent")?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.validate_exp = false;
    validation.validate_aud = false;
    let claims = jsonwebtoken::decode::<Value>(
        token,
        &DecodingKey::from_rsa_components(n, e)?,
        &validation,
    )?
    .claims;
    let nbf = claims["nbf"]
        .as_u64()
        .ok_or("OIDC token has no numeric nbf")?;
    let exp = claims["exp"]
        .as_u64()
        .ok_or("OIDC token has no numeric exp")?;
    require(
        nbf <= now && now < exp,
        "OIDC token is outside its validity period",
    )?;
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use rsa::pkcs1::DecodeRsaPrivateKey as _;
    use rsa::pkcs8::DecodePrivateKey as _;
    use rsa::traits::PublicKeyParts as _;

    use super::*;

    const NOW: u64 = 1_767_225_600;

    fn private_key(pem: &str) -> PrivateKey {
        PrivateKey::from_pkcs8_pem(pem)
            .or_else(|_| PrivateKey::from_pkcs1_pem(pem))
            .unwrap()
    }

    #[test]
    fn app_jwt_has_the_expected_app_issuer_and_validity_period() {
        let private = private_key(include_str!(
            "../../../crates/testkit/fixture/app-private-key.pem"
        ));
        let key = AppKey::new(&private).unwrap();
        let token = key.jwt(NOW).unwrap();
        let mut validation = Validation::new(Algorithm::RS256);
        validation.validate_exp = false;
        let decoded = jsonwebtoken::decode::<Value>(
            &token,
            &DecodingKey::from_rsa_pem(key.public_pem.as_bytes()).unwrap(),
            &validation,
        )
        .unwrap();
        assert_eq!(decoded.header.alg, Algorithm::RS256);
        assert_eq!(
            decoded.claims,
            json!({"iss": 1_000_001, "iat": NOW - 30, "exp": NOW + 300}),
        );
    }

    #[test]
    fn oidc_verification_requires_the_matching_key_signature_and_time() {
        let private = private_key(include_str!(
            "../../../crates/testkit/fixture/app-private-key.pem"
        ));
        let other = private_key(include_str!("../../../crates/auth/testdata/other.key.pem"));
        let key = AppKey::new(&private).unwrap();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test".into());
        let claims = json!({"nbf": NOW, "exp": NOW + 300, "run_id": "1"});
        let token = jsonwebtoken::encode(&header, &claims, &key.signing).unwrap();
        let jwk = |private: &PrivateKey| {
            json!({
                "kid": "test", "alg": "RS256", "kty": "RSA",
                "n": URL_SAFE_NO_PAD.encode(private.n().to_be_bytes_trimmed_vartime()),
                "e": URL_SAFE_NO_PAD.encode(private.e().to_be_bytes_trimmed_vartime()),
            })
        };
        let valid = json!({"keys": [jwk(&private)]});
        assert_eq!(verify(&token, &valid, NOW).unwrap(), claims);
        assert!(verify(&token, &valid, NOW - 1).is_err());
        assert!(verify(&token, &valid, NOW + 300).is_err());
        assert!(verify(&token, &json!({"keys": [jwk(&other)]}), NOW).is_err());
        assert!(
            verify(
                &token,
                &json!({"keys": [jwk(&private), jwk(&private)]}),
                NOW
            )
            .is_err()
        );
        assert!(verify(&token, &json!({"keys": []}), NOW).is_err());
    }
}
