use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use crypto_box::SecretKey;
use sha2::{Digest as _, Sha256};

use crate::ApiError;

/// The seed of every [`SealKey`] the server hands out. It is public: hubdouble is a test double,
/// and a fixed seed keeps the keys of a seeded owner the same across runs.
pub const SEAL_SEED: &[u8] = b"hubdouble sealed box keys";

/// A family of secrets that GitHub keys apart: each has its own `public-key` operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretFamily {
    Actions,
    Dependabot,
    Codespaces,
    Agents,
}

/// The account, repository or environment a secret belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretOwner<'a> {
    Repo(i64),
    Org(i64),
    Environment { repo: i64, name: &'a str },
    User(i64),
}

/// The libsodium key pair of one owner's secrets, as GitHub's `public-key` operations answer it.
pub struct SealKey {
    key_id: String,
    key: String,
    secret: SecretKey,
}

impl SealKey {
    /// The key of `owner` in `family`, the same for the same `seed`, family and owner.
    #[must_use]
    pub fn derive(seed: &[u8], family: SecretFamily, owner: SecretOwner<'_>) -> SealKey {
        let mut h = Sha256::new();
        h.update(seed);
        h.update(format!("\0{family:?}\0{owner:?}"));
        let secret = SecretKey::from_bytes(h.finalize().into());
        let public = secret.public_key();
        let digest = Sha256::digest(public.as_bytes());
        let mut first = [0; 8];
        first.copy_from_slice(&digest[..8]);
        let id = u64::from_be_bytes(first) % 900_000_000_000_000_000 + 100_000_000_000_000_000;
        SealKey {
            key_id: id.to_string(),
            key: STANDARD.encode(public.as_bytes()),
            secret,
        }
    }

    /// The `key_id` a client names when it sends a secret.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// The base64 Curve25519 public key, `key` in the `public-key` answer.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Checks the `key_id` and `encrypted_value` of a secret write and returns the sealed box
    /// bytes, which is what a secret keeps. The value is opened only to check it, never kept.
    ///
    /// # Errors
    ///
    /// `422` when `encrypted_value` does not match the base64 pattern of GitHub's request
    /// schema, when `key_id` names another key, or when the box does not open with this key.
    pub fn check(&self, key_id: &str, encrypted_value: &str) -> Result<Vec<u8>, ApiError> {
        let sealed = STANDARD
            .decode(encrypted_value)
            .ok()
            .filter(|b| !b.is_empty())
            .ok_or_else(|| {
                ApiError::unprocessable(format!(
                    "Invalid request.\n\nInvalid property /encrypted_value: `{encrypted_value}` is not base64."
                ))
            })?;
        if key_id != self.key_id || self.secret.unseal(&sealed).is_err() {
            return Err(ApiError::unprocessable(
                "Bad request - could not decrypt the secret with key_id.",
            ));
        }
        Ok(sealed)
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use crypto_box::PublicKey;
    use rand::SeedableRng as _;
    use rand::rngs::StdRng;

    use super::{SEAL_SEED, SealKey, SecretFamily, SecretOwner};

    const REPO: SecretOwner<'static> = SecretOwner::Repo(7);

    fn seal(key: &SealKey, plaintext: &[u8]) -> String {
        let bytes: [u8; 32] = STANDARD.decode(key.key()).unwrap().try_into().unwrap();
        let sealed = PublicKey::from(bytes)
            .seal(&mut StdRng::seed_from_u64(1), plaintext)
            .unwrap();
        STANDARD.encode(sealed)
    }

    #[test]
    fn a_box_sealed_by_libsodium_to_the_public_key_opens_and_keeps_only_the_box() {
        // Given the repository key, and a box PyNaCl's SealedBox sealed to its public key
        let key = SealKey::derive(SEAL_SEED, SecretFamily::Actions, REPO);
        assert_eq!(key.key(), LIBSODIUM_KEY);
        let sent = LIBSODIUM_BOX;

        // When the box is checked
        let kept = key.check(key.key_id(), sent).unwrap();

        // Then the secret keeps the box as sent
        assert_eq!(STANDARD.encode(&kept), sent);
    }

    #[test]
    fn each_owner_and_family_has_its_own_stable_key() {
        // Given keys of one owner in two families, two owners, and two environments
        let keys = [
            SealKey::derive(SEAL_SEED, SecretFamily::Actions, REPO),
            SealKey::derive(SEAL_SEED, SecretFamily::Dependabot, REPO),
            SealKey::derive(SEAL_SEED, SecretFamily::Actions, SecretOwner::Org(7)),
            SealKey::derive(
                SEAL_SEED,
                SecretFamily::Actions,
                SecretOwner::Environment {
                    repo: 7,
                    name: "prod",
                },
            ),
            SealKey::derive(
                SEAL_SEED,
                SecretFamily::Actions,
                SecretOwner::Environment {
                    repo: 7,
                    name: "test",
                },
            ),
        ];

        // Then the same inputs give the same key, and each key and key ID is distinct
        let again = SealKey::derive(SEAL_SEED, SecretFamily::Actions, REPO);
        assert_eq!(
            (again.key(), again.key_id()),
            (keys[0].key(), keys[0].key_id())
        );
        for (i, a) in keys.iter().enumerate() {
            assert_eq!(a.key_id().len(), 18, "{}", a.key_id());
            assert!(a.key_id().bytes().all(|b| b.is_ascii_digit()));
            for b in &keys[i + 1..] {
                assert_ne!(a.key(), b.key());
                assert_ne!(a.key_id(), b.key_id());
            }
        }
    }

    #[test]
    fn a_value_that_is_not_base64_or_does_not_open_is_422() {
        // Given the repository key and the key of another repository
        let key = SealKey::derive(SEAL_SEED, SecretFamily::Actions, REPO);
        let other = SealKey::derive(SEAL_SEED, SecretFamily::Actions, SecretOwner::Repo(8));
        let sealed = seal(&key, b"s3cret");

        // Then
        for (key_id, value) in [
            (key.key_id(), "not base64!"),
            (key.key_id(), ""),
            (key.key_id(), &*seal(&other, b"s3cret")),
            (
                key.key_id(),
                &*STANDARD.encode(b"too short for a sealed box"),
            ),
            (other.key_id(), &*sealed),
        ] {
            assert_eq!(
                key.check(key_id, value).map_err(|e| e.status),
                Err(422),
                "{value}"
            );
        }
        assert!(key.check(key.key_id(), &sealed).is_ok());
    }

    const LIBSODIUM_KEY: &str = "XeBxTXvJOM2icN2hMj7vudRNaqolBpaaItpXYtzYjQY=";
    const LIBSODIUM_BOX: &str =
        "H2rNgEXF8NtCKyuxWojl/u6vKqVIdG6UcAzQf0O86nd0C47Tn1N5SEcsfTsrviL0bfJFeS/9";
}
