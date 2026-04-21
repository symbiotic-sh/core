//! Phone-local escrow blob operations.
//!
//! Encrypts arbitrary secret data with a user-chosen passphrase using
//! Argon2id key derivation and XChaCha20-Poly1305 authenticated encryption.
//! The blob is stored at `{data_dir}/escrow.blob` and is safe to store on
//! any untrusted medium.
//!
//! ## Blob format (SYMESC2)
//!
//! ```text
//! "SYMESC2" (7 bytes) || fingerprint (8 bytes) || salt (32 bytes) || nonce (24 bytes) || ciphertext+tag (variable)
//! ```
//!
//! This mirrors the format used by `symbiotic-vault-store::escrow` so blobs
//! created on the phone can be synced to the daemon and vice-versa.

use argon2::Argon2;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

/// Magic bytes identifying the escrow blob format.
const MAGIC_V2: &[u8; 7] = b"SYMESC2";
const MAGIC_LEN: usize = 7;
const FINGERPRINT_LEN: usize = 8;
const SALT_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const HEADER_V2_LEN: usize = MAGIC_LEN + FINGERPRINT_LEN + SALT_LEN + NONCE_LEN;

/// Argon2id parameters tuned for mobile (lower memory than server defaults).
struct MobileKdfParams {
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
}

impl MobileKdfParams {
    /// Production parameters: 64 MiB memory, 3 iterations, 2 threads.
    /// This is lower than the server default (256 MiB) to keep mobile devices
    /// responsive while still providing strong key derivation.
    fn production() -> Self {
        Self {
            memory_kib: 65_536, // 64 MiB
            iterations: 3,
            parallelism: 2,
        }
    }

    #[cfg(test)]
    fn test() -> Self {
        Self {
            memory_kib: 1024, // 1 MiB (fast for tests)
            iterations: 1,
            parallelism: 1,
        }
    }
}

/// Errors from escrow operations.
#[derive(Debug)]
pub enum EscrowError {
    InvalidFormat(String),
    DecryptionFailed,
    PassphraseTooWeak(String),
    Kdf(String),
    Encrypt(String),
    Io(std::io::Error),
}

impl std::fmt::Display for EscrowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFormat(msg) => write!(f, "invalid escrow blob format: {msg}"),
            Self::DecryptionFailed => {
                write!(f, "decryption failed (wrong passphrase or corrupted blob)")
            }
            Self::PassphraseTooWeak(msg) => write!(f, "passphrase too weak: {msg}"),
            Self::Kdf(msg) => write!(f, "key derivation error: {msg}"),
            Self::Encrypt(msg) => write!(f, "encryption error: {msg}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl From<std::io::Error> for EscrowError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Escrow blob filename.
const ESCROW_FILENAME: &str = "escrow.blob";

/// Validate passphrase strength.
///
/// Requires at least 12 characters or 4 space-separated words.
pub fn validate_passphrase(passphrase: &str) -> Result<(), EscrowError> {
    let trimmed = passphrase.trim();

    if trimmed.is_empty() {
        return Err(EscrowError::PassphraseTooWeak(
            "passphrase cannot be empty".to_string(),
        ));
    }

    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.len() >= 4 {
        return Ok(());
    }

    if trimmed.len() >= 12 {
        return Ok(());
    }

    Err(EscrowError::PassphraseTooWeak(
        "passphrase must be at least 12 characters or 4+ words".to_string(),
    ))
}

/// Create an escrow blob from secret data and a passphrase.
///
/// Returns the SYMESC2 blob bytes.
pub fn create_blob(secret_data: &[u8], passphrase: &str) -> Result<Vec<u8>, EscrowError> {
    create_blob_with_params(secret_data, passphrase, &MobileKdfParams::production())
}

fn create_blob_with_params(
    secret_data: &[u8],
    passphrase: &str,
    params: &MobileKdfParams,
) -> Result<Vec<u8>, EscrowError> {
    validate_passphrase(passphrase)?;

    // Compute fingerprint of the secret data for integrity checking.
    let fingerprint = compute_fingerprint(secret_data);

    // Generate random salt and nonce.
    let mut salt = [0u8; SALT_LEN];
    rand::rng().fill_bytes(&mut salt);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);

    // Derive encryption key from passphrase via Argon2id.
    let key = derive_key(passphrase, &salt, params)?;

    // Encrypt secret data with XChaCha20-Poly1305.
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|e| EscrowError::Encrypt(format!("cipher init: {e}")))?;
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, secret_data)
        .map_err(|e| EscrowError::Encrypt(format!("encrypt: {e}")))?;

    // Assemble blob: magic_v2 || fingerprint || salt || nonce || ciphertext+tag
    let mut blob = Vec::with_capacity(HEADER_V2_LEN + ciphertext.len());
    blob.extend_from_slice(MAGIC_V2);
    blob.extend_from_slice(&fingerprint);
    blob.extend_from_slice(&salt);
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);

    Ok(blob)
}

/// Recover secret data from an escrow blob using a passphrase.
pub fn recover_blob(blob: &[u8], passphrase: &str) -> Result<Vec<u8>, EscrowError> {
    recover_blob_with_params(blob, passphrase, &MobileKdfParams::production())
}

fn recover_blob_with_params(
    blob: &[u8],
    passphrase: &str,
    params: &MobileKdfParams,
) -> Result<Vec<u8>, EscrowError> {
    let parts = parse_blob(blob)?;

    // Derive decryption key from passphrase.
    let key = derive_key(passphrase, parts.salt, params)?;

    // Decrypt.
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|e| EscrowError::Encrypt(format!("cipher init: {e}")))?;
    let nonce = XNonce::from_slice(parts.nonce);

    cipher
        .decrypt(nonce, parts.ciphertext)
        .map_err(|_| EscrowError::DecryptionFailed)
}

/// Create an escrow blob and write it to `{data_dir}/escrow.blob`.
///
/// The `secret_data` is the device's identity key material (or any secret
/// bytes to escrow).
pub fn create_escrow_file(
    data_dir: &str,
    secret_data: &[u8],
    passphrase: &str,
) -> Result<String, EscrowError> {
    let blob = create_blob(secret_data, passphrase)?;
    let path = Path::new(data_dir).join(ESCROW_FILENAME);

    // Ensure data directory exists.
    fs::create_dir_all(data_dir)?;
    fs::write(&path, &blob)?;

    Ok(path.to_string_lossy().into_owned())
}

/// Recover secret data from `{data_dir}/escrow.blob`.
pub fn recover_escrow_file(data_dir: &str, passphrase: &str) -> Result<Vec<u8>, EscrowError> {
    let path = Path::new(data_dir).join(ESCROW_FILENAME);
    let blob = fs::read(&path)?;
    recover_blob(&blob, passphrase)
}

/// Check whether `{data_dir}/escrow.blob` exists.
pub fn escrow_exists(data_dir: &str) -> bool {
    Path::new(data_dir).join(ESCROW_FILENAME).exists()
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Compute an 8-byte fingerprint from data (truncated SHA-256).
fn compute_fingerprint(data: &[u8]) -> [u8; FINGERPRINT_LEN] {
    let hash = Sha256::digest(data);
    let mut fp = [0u8; FINGERPRINT_LEN];
    fp.copy_from_slice(&hash[..FINGERPRINT_LEN]);
    fp
}

/// Parsed components of an escrow blob.
struct BlobParts<'a> {
    salt: &'a [u8],
    nonce: &'a [u8],
    ciphertext: &'a [u8],
}

/// Parse an escrow blob into its component parts.
fn parse_blob(blob: &[u8]) -> Result<BlobParts<'_>, EscrowError> {
    if blob.len() < MAGIC_LEN {
        return Err(EscrowError::InvalidFormat("blob too short".to_string()));
    }

    let magic = &blob[..MAGIC_LEN];

    if magic == MAGIC_V2.as_slice() {
        if blob.len() < HEADER_V2_LEN + 1 {
            return Err(EscrowError::InvalidFormat("V2 blob truncated".to_string()));
        }

        let salt_start = MAGIC_LEN + FINGERPRINT_LEN;
        let nonce_start = salt_start + SALT_LEN;
        let ciphertext_start = nonce_start + NONCE_LEN;

        Ok(BlobParts {
            salt: &blob[salt_start..nonce_start],
            nonce: &blob[nonce_start..ciphertext_start],
            ciphertext: &blob[ciphertext_start..],
        })
    } else if magic.starts_with(b"SYMESC") {
        let version = String::from_utf8_lossy(magic);
        Err(EscrowError::InvalidFormat(format!(
            "unsupported version: {version}"
        )))
    } else {
        Err(EscrowError::InvalidFormat(
            "unrecognized magic bytes".to_string(),
        ))
    }
}

/// Derive a 256-bit encryption key from a passphrase and salt using Argon2id.
fn derive_key(
    passphrase: &str,
    salt: &[u8],
    params: &MobileKdfParams,
) -> Result<[u8; 32], EscrowError> {
    let argon2_params = argon2::Params::new(
        params.memory_kib,
        params.iterations,
        params.parallelism,
        Some(32),
    )
    .map_err(|e| EscrowError::Kdf(format!("invalid params: {e}")))?;

    let argon2 = Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2_params,
    );

    let mut key = [0u8; 32];
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| EscrowError::Kdf(format!("hash failed: {e}")))?;

    Ok(key)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD_PASSPHRASE: &str = "correct horse battery staple";
    const WEAK_PASSPHRASE: &str = "short";

    fn test_params() -> MobileKdfParams {
        MobileKdfParams::test()
    }

    #[test]
    fn roundtrip_create_and_recover() {
        let secret = b"AGE-SECRET-KEY-1FAKE...";
        let params = test_params();

        let blob = create_blob_with_params(secret, GOOD_PASSPHRASE, &params).unwrap();
        let recovered = recover_blob_with_params(&blob, GOOD_PASSPHRASE, &params).unwrap();

        assert_eq!(secret.as_slice(), recovered.as_slice());
    }

    #[test]
    fn wrong_passphrase_fails() {
        let secret = b"some secret key material";
        let params = test_params();

        let blob = create_blob_with_params(secret, GOOD_PASSPHRASE, &params).unwrap();
        let result = recover_blob_with_params(&blob, "wrong passphrase here", &params);

        assert!(result.is_err());
        match result.unwrap_err() {
            EscrowError::DecryptionFailed => {}
            other => panic!("expected DecryptionFailed, got: {other}"),
        }
    }

    #[test]
    fn weak_passphrase_rejected() {
        let result = validate_passphrase(WEAK_PASSPHRASE);
        assert!(result.is_err());
        match result.unwrap_err() {
            EscrowError::PassphraseTooWeak(_) => {}
            other => panic!("expected PassphraseTooWeak, got: {other}"),
        }
    }

    #[test]
    fn empty_passphrase_rejected() {
        let result = validate_passphrase("");
        assert!(result.is_err());
    }

    #[test]
    fn passphrase_12_chars_accepted() {
        assert!(validate_passphrase("123456789012").is_ok());
    }

    #[test]
    fn passphrase_11_chars_rejected() {
        assert!(validate_passphrase("12345678901").is_err());
    }

    #[test]
    fn passphrase_4_words_accepted() {
        assert!(validate_passphrase("one two three four").is_ok());
    }

    #[test]
    fn passphrase_3_words_rejected_if_short() {
        assert!(validate_passphrase("a b c").is_err());
    }

    #[test]
    fn blob_starts_with_magic() {
        let secret = b"secret data here";
        let params = test_params();

        let blob = create_blob_with_params(secret, GOOD_PASSPHRASE, &params).unwrap();
        assert!(blob.starts_with(b"SYMESC2"));
    }

    #[test]
    fn corrupted_ciphertext_fails() {
        let secret = b"secret data";
        let params = test_params();

        let mut blob = create_blob_with_params(secret, GOOD_PASSPHRASE, &params).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;

        let result = recover_blob_with_params(&blob, GOOD_PASSPHRASE, &params);
        assert!(result.is_err());
    }

    #[test]
    fn empty_blob_fails() {
        let result = recover_blob_with_params(&[], "any passphrase", &test_params());
        assert!(result.is_err());
    }

    #[test]
    fn truncated_blob_fails() {
        let secret = b"secret data";
        let params = test_params();

        let blob = create_blob_with_params(secret, GOOD_PASSPHRASE, &params).unwrap();
        let truncated = &blob[..HEADER_V2_LEN];

        let result = recover_blob_with_params(truncated, GOOD_PASSPHRASE, &params);
        assert!(result.is_err());
    }

    #[test]
    fn multiple_blobs_differ_due_to_random_salt() {
        let secret = b"same secret";
        let params = test_params();

        let blob1 = create_blob_with_params(secret, GOOD_PASSPHRASE, &params).unwrap();
        let blob2 = create_blob_with_params(secret, GOOD_PASSPHRASE, &params).unwrap();

        // Different random salt/nonce means different ciphertext.
        assert_ne!(blob1, blob2);

        // Both should recover the same secret.
        let r1 = recover_blob_with_params(&blob1, GOOD_PASSPHRASE, &params).unwrap();
        let r2 = recover_blob_with_params(&blob2, GOOD_PASSPHRASE, &params).unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        let secret = b"device identity key material";

        // Should not exist initially.
        assert!(!escrow_exists(data_dir));

        // Create escrow file (using internal params for speed).
        let blob = create_blob_with_params(secret, GOOD_PASSPHRASE, &test_params()).unwrap();
        let path = Path::new(data_dir).join(ESCROW_FILENAME);
        fs::write(&path, &blob).unwrap();

        // Should exist now.
        assert!(escrow_exists(data_dir));

        // Recover.
        let blob_bytes = fs::read(&path).unwrap();
        let recovered =
            recover_blob_with_params(&blob_bytes, GOOD_PASSPHRASE, &test_params()).unwrap();
        assert_eq!(secret.as_slice(), recovered.as_slice());
    }

    #[test]
    fn blob_size_is_reasonable() {
        let secret = b"AGE-SECRET-KEY-1ABCDEFGHIJKLMNOPQRSTUVWXYZ234567890ABCDEFGHIJKLMNOPQ";
        let params = test_params();

        let blob = create_blob_with_params(secret, GOOD_PASSPHRASE, &params).unwrap();

        // Header (71 bytes) + secret (~66 bytes) + auth tag (16 bytes) = ~153.
        assert!(blob.len() > HEADER_V2_LEN + 16);
        assert!(blob.len() < 300);
    }
}
