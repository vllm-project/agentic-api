//! Sealed web search content for Messages clients.
//!
//! Anthropic returns each search result's page content as opaque
//! `encrypted_content` that the client sends back unchanged on later turns. The
//! gateway seals the content it shows the model the same way, with AES-256-GCM
//! under a key derived from the operator's secret, or from a secret generated
//! once per process when none is configured. Every envelope draws a random salt
//! from which its own content key is derived, so a long-lived secret never
//! repeats a key and nonce pair. The caller's associated data binds a result to
//! its call and position, so content cannot be altered, moved, or reordered
//! unnoticed.

use std::sync::OnceLock;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::error::Unspecified;
use ring::hkdf;
use ring::rand::{SecureRandom as _, SystemRandom};
use thiserror::Error;

/// Environment variable holding the operator's secret for sealing results.
pub const MESSAGES_RESULT_KEY_ENV: &str = "AGENTIC_MESSAGES_RESULT_KEY";

/// Minimum length of an operator secret, in non-whitespace bytes.
const MIN_SECRET_LEN: usize = 32;
const ENVELOPE_VERSION: u8 = 1;
const KEY_ID_LEN: usize = 8;
const SALT_LEN: usize = 16;
const TAG_LEN: usize = 16;
const HKDF_SALT: &[u8] = b"agentic-api/messages/result-key/v1";
const KEY_ID_INFO: &[u8] = b"key id";
const CONTENT_KEY_INFO: &[u8] = b"content key";

/// The key that seals and opens result content returned to Messages clients.
pub struct MessagesResultKey {
    prk: hkdf::Prk,
    id: [u8; KEY_ID_LEN],
}

impl std::fmt::Debug for MessagesResultKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MessagesResultKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// A result key that could not be created.
#[derive(Debug, Error)]
pub enum MessagesResultKeyError {
    #[error("{MESSAGES_RESULT_KEY_ENV} must be at least {MIN_SECRET_LEN} bytes of independently generated randomness")]
    TooShort,
    #[error("could not derive the Messages result key")]
    Unavailable,
}

/// Why sealed content did not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenError {
    /// Not an envelope this gateway produces.
    Malformed,
    /// Sealed under another key, such as one generated before a restart.
    OtherKey,
    /// The envelope or its associated data was altered.
    Tampered,
}

/// `hkdf` output length for the key id.
struct KeyIdLen;

impl hkdf::KeyType for KeyIdLen {
    fn len(&self) -> usize {
        KEY_ID_LEN
    }
}

impl MessagesResultKey {
    /// Derive the key from an operator secret.
    ///
    /// # Errors
    /// Returns [`MessagesResultKeyError::TooShort`] when the secret has fewer
    /// than 32 non-whitespace bytes.
    pub fn from_secret(secret: &[u8]) -> Result<Self, MessagesResultKeyError> {
        if secret.iter().filter(|byte| !byte.is_ascii_whitespace()).count() < MIN_SECRET_LEN {
            return Err(MessagesResultKeyError::TooShort);
        }
        Self::derive(secret)
    }

    /// The key configured through [`MESSAGES_RESULT_KEY_ENV`], if it is set.
    ///
    /// # Errors
    /// Returns [`MessagesResultKeyError::TooShort`] for a secret that is set but
    /// too short.
    pub fn from_env() -> Result<Option<Self>, MessagesResultKeyError> {
        Self::from_env_value(std::env::var(MESSAGES_RESULT_KEY_ENV).ok())
    }

    /// The key a value of [`MESSAGES_RESULT_KEY_ENV`] configures; surrounding
    /// whitespace is ignored, and a blank value configures none.
    fn from_env_value(value: Option<String>) -> Result<Option<Self>, MessagesResultKeyError> {
        value
            .map(|secret| secret.trim().to_owned())
            .filter(|secret| !secret.is_empty())
            .map(|secret| Self::from_secret(secret.as_bytes()))
            .transpose()
    }

    fn generate() -> Result<Self, MessagesResultKeyError> {
        let mut secret = [0; 32];
        SystemRandom::new()
            .fill(&mut secret)
            .map_err(|_| MessagesResultKeyError::Unavailable)?;
        Self::derive(&secret)
    }

    fn derive(secret: &[u8]) -> Result<Self, MessagesResultKeyError> {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, HKDF_SALT).extract(secret);
        let mut id = [0; KEY_ID_LEN];
        prk.expand(&[KEY_ID_INFO], KeyIdLen)
            .and_then(|okm| okm.fill(&mut id))
            .map_err(|_| MessagesResultKeyError::Unavailable)?;
        Ok(Self { prk, id })
    }

    fn content_key(&self, salt: &[u8]) -> Result<LessSafeKey, Unspecified> {
        let info = [CONTENT_KEY_INFO, salt];
        let okm = self.prk.expand(&info, &AES_256_GCM)?;
        Ok(LessSafeKey::new(UnboundKey::from(okm)))
    }

    /// Seal `content`, bound to `aad`, into a URL-safe envelope.
    pub(crate) fn seal(&self, aad: &[u8], mut content: Vec<u8>) -> Result<String, MessagesResultKeyError> {
        let mut salt = [0; SALT_LEN];
        SystemRandom::new()
            .fill(&mut salt)
            .map_err(|_| MessagesResultKeyError::Unavailable)?;
        // Each envelope has its own content key, so the fixed nonce never repeats under one key.
        self.content_key(&salt)
            .and_then(|key| {
                key.seal_in_place_append_tag(
                    Nonce::assume_unique_for_key([0; NONCE_LEN]),
                    Aad::from(aad),
                    &mut content,
                )
            })
            .map_err(|_| MessagesResultKeyError::Unavailable)?;
        let mut envelope = Vec::with_capacity(1 + KEY_ID_LEN + SALT_LEN + content.len());
        envelope.push(ENVELOPE_VERSION);
        envelope.extend_from_slice(&self.id);
        envelope.extend_from_slice(&salt);
        envelope.extend_from_slice(&content);
        Ok(URL_SAFE_NO_PAD.encode(envelope))
    }

    /// Open an envelope produced by [`seal`](Self::seal) with the same `aad`.
    pub(crate) fn open(&self, aad: &[u8], sealed: &str) -> Result<Vec<u8>, OpenError> {
        let envelope = URL_SAFE_NO_PAD.decode(sealed).map_err(|_| OpenError::Malformed)?;
        if envelope.len() < 1 + KEY_ID_LEN + SALT_LEN + TAG_LEN || envelope[0] != ENVELOPE_VERSION {
            return Err(OpenError::Malformed);
        }
        let (id, rest) = envelope[1..].split_at(KEY_ID_LEN);
        if id != self.id {
            return Err(OpenError::OtherKey);
        }
        let (salt, sealed_content) = rest.split_at(SALT_LEN);
        let mut content = sealed_content.to_vec();
        let key = self.content_key(salt).map_err(|_| OpenError::Tampered)?;
        let opened = key
            .open_in_place(
                Nonce::assume_unique_for_key([0; NONCE_LEN]),
                Aad::from(aad),
                &mut content,
            )
            .map_err(|_| OpenError::Tampered)?
            .len();
        content.truncate(opened);
        Ok(content)
    }
}

/// The key used when the operator configured none: generated on first use and
/// kept for the life of the process.
pub(crate) fn process_result_key() -> Result<&'static MessagesResultKey, MessagesResultKeyError> {
    static KEY: OnceLock<MessagesResultKey> = OnceLock::new();
    if let Some(key) = KEY.get() {
        return Ok(key);
    }
    let key = MessagesResultKey::generate()?;
    Ok(KEY.get_or_init(|| key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_from(secret: &str) -> MessagesResultKey {
        MessagesResultKey::from_secret(secret.as_bytes()).unwrap()
    }

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn sealed_content_opens_only_with_the_same_key_and_associated_data() {
        let key = key_from(SECRET);
        let sealed = key.seal(b"call 1, result 0 of 2", b"page text".to_vec()).unwrap();
        assert_eq!(key.open(b"call 1, result 0 of 2", &sealed).unwrap(), b"page text");
        assert!(!sealed.contains("page"), "the content is not readable in the envelope");
        // Another envelope of the same content differs (fresh salt).
        assert_ne!(
            sealed,
            key.seal(b"call 1, result 0 of 2", b"page text".to_vec()).unwrap()
        );

        assert_eq!(key.open(b"call 1, result 1 of 2", &sealed), Err(OpenError::Tampered));
        // The same secret derives the same key, so a restart with it keeps results.
        assert_eq!(
            MessagesResultKey::from_secret(SECRET.as_bytes())
                .unwrap()
                .open(b"call 1, result 0 of 2", &sealed)
                .unwrap(),
            b"page text"
        );
        let other = key_from("fedcba9876543210fedcba9876543210");
        assert_eq!(other.open(b"call 1, result 0 of 2", &sealed), Err(OpenError::OtherKey));
        let generated = MessagesResultKey::generate().unwrap();
        assert_eq!(
            generated.open(b"call 1, result 0 of 2", &sealed),
            Err(OpenError::OtherKey)
        );
    }

    #[test]
    fn every_altered_byte_is_detected() {
        let key = key_from(SECRET);
        let sealed = key.seal(b"aad", b"some page content".to_vec()).unwrap();
        let envelope = URL_SAFE_NO_PAD.decode(&sealed).unwrap();
        for index in 0..envelope.len() {
            let mut altered = envelope.clone();
            altered[index] ^= 0x01;
            let expected = match index {
                0 => OpenError::Malformed,
                1..=KEY_ID_LEN => OpenError::OtherKey,
                _ => OpenError::Tampered,
            };
            assert_eq!(
                key.open(b"aad", &URL_SAFE_NO_PAD.encode(&altered)),
                Err(expected),
                "byte {index}"
            );
        }
        let truncated = &envelope[..envelope.len() - 1];
        assert_eq!(
            key.open(b"aad", &URL_SAFE_NO_PAD.encode(truncated)),
            Err(OpenError::Tampered)
        );
        for malformed in ["", "not base64!", "AQ", &URL_SAFE_NO_PAD.encode([1; 40])] {
            assert_eq!(key.open(b"aad", malformed), Err(OpenError::Malformed), "{malformed}");
        }
    }

    #[test]
    fn a_short_secret_is_refused() {
        assert!(matches!(
            MessagesResultKey::from_secret(b"short"),
            Err(MessagesResultKeyError::TooShort)
        ));
        let padded = format!("{}{}", "a".repeat(MIN_SECRET_LEN - 1), " ".repeat(8));
        assert!(matches!(
            MessagesResultKey::from_secret(padded.as_bytes()),
            Err(MessagesResultKeyError::TooShort)
        ));
        assert!(MessagesResultKey::from_secret("a".repeat(MIN_SECRET_LEN).as_bytes()).is_ok());
    }

    #[test]
    fn the_environment_value_configures_the_key() {
        assert!(MessagesResultKey::from_env_value(None).unwrap().is_none());
        assert!(
            MessagesResultKey::from_env_value(Some(" \n".to_owned()))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            MessagesResultKey::from_env_value(Some("short".to_owned())),
            Err(MessagesResultKeyError::TooShort)
        ));
        let configured = MessagesResultKey::from_env_value(Some(format!(" {SECRET}\n")))
            .unwrap()
            .unwrap();
        let sealed = key_from(SECRET).seal(b"aad", b"x".to_vec()).unwrap();
        assert_eq!(
            configured.open(b"aad", &sealed).unwrap(),
            b"x",
            "the trimmed value is the secret"
        );
    }

    #[test]
    fn the_process_key_is_generated_once() {
        let first = process_result_key().unwrap();
        let second = process_result_key().unwrap();
        assert!(std::ptr::eq(first, second));
        let sealed = first.seal(b"aad", b"x".to_vec()).unwrap();
        assert_eq!(second.open(b"aad", &sealed).unwrap(), b"x");
    }

    #[test]
    fn debug_output_does_not_reveal_the_key() {
        let rendered = format!("{:?}", key_from(SECRET));
        assert!(rendered.starts_with("MessagesResultKey"));
        assert!(!rendered.contains("0123456789abcdef"));
    }
}
