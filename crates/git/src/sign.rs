use hubdouble_model::Timestamp;
use pgp::composed::{
    ArmorOptions, Deserializable, DetachedSignature, EncryptionCaps, KeyType,
    SecretKeyParamsBuilder, SignedPublicKey, SignedSecretKey, SubpacketConfig,
};
use pgp::crypto::hash::HashAlgorithm;
use pgp::packet::{Subpacket, SubpacketData};
use pgp::types::{KeyDetails, Password};
use rand::SeedableRng;
use rand::rngs::StdRng;

use crate::contract::{GitError, SignatureState};

const KEY_SEED: u64 = 0x6875_6264_6f75_626c;

fn pgp_error(e: impl std::fmt::Display) -> GitError {
    GitError::Invalid(format!("web-flow signing: {e}"))
}

pub(crate) struct WebFlow {
    secret: SignedSecretKey,
    public: SignedPublicKey,
}

fn timestamp(t: Timestamp) -> pgp::types::Timestamp {
    pgp::types::Timestamp::from_secs(u32::try_from(t.unix()).unwrap_or(0))
}

impl WebFlow {
    pub(crate) fn new() -> Result<WebFlow, GitError> {
        WebFlow::from_seed(KEY_SEED)
    }

    pub(crate) fn from_seed(seed: u64) -> Result<WebFlow, GitError> {
        let mut params = SecretKeyParamsBuilder::default();
        params
            .key_type(KeyType::Ed25519Legacy)
            .can_certify(true)
            .can_sign(true)
            .can_encrypt(EncryptionCaps::None)
            .created_at(timestamp(Timestamp::SEED))
            .primary_user_id("GitHub (web-flow commit signing) <noreply@github.com>".into());
        let secret = params
            .build()
            .map_err(pgp_error)?
            .generate(StdRng::seed_from_u64(seed))
            .map_err(pgp_error)?;
        let public = secret.to_public_key();
        Ok(WebFlow { secret, public })
    }

    pub(crate) fn sign(&self, payload: &[u8], time: Timestamp) -> Result<String, GitError> {
        let key = &self.secret.primary_key;
        let subpackets = SubpacketConfig::UserDefined {
            hashed: vec![
                Subpacket::regular(SubpacketData::IssuerFingerprint(key.fingerprint()))
                    .map_err(pgp_error)?,
                Subpacket::regular(SubpacketData::SignatureCreationTime(timestamp(time)))
                    .map_err(pgp_error)?,
            ],
            unhashed: vec![
                Subpacket::regular(SubpacketData::IssuerKeyId(key.legacy_key_id()))
                    .map_err(pgp_error)?,
            ],
        };
        let signature = DetachedSignature::sign_binary_data_with_subpackets(
            StdRng::seed_from_u64(KEY_SEED),
            key,
            &Password::empty(),
            HashAlgorithm::Sha256,
            payload,
            subpackets,
        )
        .map_err(pgp_error)?;
        let armored = signature
            .to_armored_string(ArmorOptions::default())
            .map_err(pgp_error)?;
        Ok(armored.trim_end().to_owned())
    }

    pub(crate) fn verify(&self, armored: &str, payload: &[u8]) -> SignatureState {
        let Ok((signature, _)) = DetachedSignature::from_string(armored) else {
            return SignatureState::Invalid;
        };
        let ours = self.public.fingerprint();
        let id = self.public.legacy_key_id();
        let config = &signature.signature;
        let by_us =
            config.issuer_fingerprint().contains(&&ours) || config.issuer_key_id().contains(&&id);
        if !by_us {
            return SignatureState::UnknownKey;
        }
        match signature.verify(&self.public, payload) {
            Ok(()) => SignatureState::Valid,
            Err(_) => SignatureState::Invalid,
        }
    }

    pub(crate) fn public(&self) -> Result<String, GitError> {
        self.public
            .to_armored_string(ArmorOptions::default())
            .map_err(pgp_error)
    }
}
