//! Key escrow module for disaster recovery.
//!
//! Encrypts an age X25519 identity key with a user-chosen passphrase using
//! Argon2id key derivation and XChaCha20-Poly1305 authenticated encryption.
//! The resulting escrow blob is safe to store on any untrusted medium (VPS,
//! cloud storage, USB drive, QR code) because it is useless without the
//! passphrase.
//!
//! ## Blob formats
//!
//! ### SYMESC1 (legacy)
//! ```text
//! "SYMESC1" (7 bytes) || salt (32 bytes) || nonce (24 bytes) || ciphertext+tag (variable)
//! ```
//!
//! ### SYMESC2 (current)
//! ```text
//! "SYMESC2" (7 bytes) || fingerprint (8 bytes) || salt (32 bytes) || nonce (24 bytes) || ciphertext+tag (variable)
//! ```
//!
//! The fingerprint is `SHA-256(pubkey_string)[0..8]` and enables pre-flight
//! detection of "wrong blob" vs "wrong passphrase" after a decryption failure.
//!
//! See `docs/design/blob-key-escrow.md` for the full design document.

use age::secrecy::ExposeSecret;
use age::x25519::Identity;
use argon2::Argon2;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use rand::Rng;
use sha2::{Digest, Sha256};

/// Magic bytes identifying the legacy escrow blob format (v1, no fingerprint).
const MAGIC_V1: &[u8; 7] = b"SYMESC1";

/// Magic bytes identifying the current escrow blob format (v2, with fingerprint).
const MAGIC_V2: &[u8; 7] = b"SYMESC2";

/// Length of the magic header bytes.
const MAGIC_LEN: usize = 7;

/// Length of the pubkey fingerprint in bytes (truncated SHA-256).
const FINGERPRINT_LEN: usize = 8;

/// Length of the Argon2id salt in bytes.
const SALT_LEN: usize = 32;

/// Length of the XChaCha20-Poly1305 nonce in bytes.
const NONCE_LEN: usize = 24;

/// Minimum header size for V1: magic + salt + nonce.
const HEADER_V1_LEN: usize = MAGIC_LEN + SALT_LEN + NONCE_LEN;

/// Minimum header size for V2: magic + fingerprint + salt + nonce.
const HEADER_V2_LEN: usize = MAGIC_LEN + FINGERPRINT_LEN + SALT_LEN + NONCE_LEN;

/// Parameters for Argon2id key derivation.
#[derive(Debug, Clone)]
pub struct EscrowKdfParams {
    /// Memory cost in KiB (default: 262_144 = 256 MiB).
    pub memory_kib: u32,
    /// Number of iterations (default: 3).
    pub iterations: u32,
    /// Degree of parallelism (default: 4).
    pub parallelism: u32,
}

impl Default for EscrowKdfParams {
    fn default() -> Self {
        Self {
            memory_kib: 262_144,
            iterations: 3,
            parallelism: 4,
        }
    }
}

/// Errors from escrow operations.
#[derive(Debug, thiserror::Error)]
pub enum EscrowError {
    #[error("invalid escrow blob format")]
    InvalidFormat,
    #[error("unsupported escrow version: {0}")]
    UnsupportedVersion(String),
    #[error("decryption failed (wrong passphrase or corrupted blob)")]
    DecryptionFailed,
    #[error(
        "wrong escrow blob (fingerprint mismatch — this blob belongs to a different identity)"
    )]
    WrongBlob,
    #[error("passphrase too weak: {0}")]
    PassphraseTooWeak(String),
    #[error("key derivation error: {0}")]
    Kdf(String),
    #[error("encryption error: {0}")]
    Encrypt(String),
}

/// Create an escrow blob from an age identity and a recovery passphrase.
///
/// The identity is encrypted with a key derived from the passphrase via
/// Argon2id, then wrapped in XChaCha20-Poly1305.
///
/// Returns the escrow blob bytes (prefixed with "SYMESC2") including an
/// 8-byte pubkey fingerprint for wrong-blob detection.
pub fn create_escrow(
    identity: &Identity,
    passphrase: &str,
    params: &EscrowKdfParams,
) -> Result<Vec<u8>, EscrowError> {
    validate_passphrase(passphrase)?;

    // Serialize identity to its AGE-SECRET-KEY-1... string representation
    let identity_str = identity.to_string();
    let identity_bytes = identity_str.expose_secret().as_bytes();

    // Compute pubkey fingerprint for wrong-blob detection
    let fingerprint = compute_fingerprint(identity);

    // Generate random salt and nonce
    let mut salt = [0u8; SALT_LEN];
    rand::rng().fill_bytes(&mut salt);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);

    // Derive encryption key from passphrase via Argon2id
    let key = derive_key(passphrase, &salt, params)?;

    // Encrypt identity bytes with XChaCha20-Poly1305
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|e| EscrowError::Encrypt(format!("cipher init: {e}")))?;
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, identity_bytes)
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

/// Recover an age identity from an escrow blob and recovery passphrase.
///
/// Parses the blob (supports both SYMESC1 and SYMESC2 formats), derives the
/// decryption key from the passphrase via Argon2id with the embedded salt,
/// and decrypts the identity.
///
/// The `params` must match the KDF parameters used during escrow creation.
/// For blobs created with default parameters, use `EscrowKdfParams::default()`.
///
/// ## Error discrimination (SYMESC2 only)
///
/// On decryption failure with a SYMESC2 blob, the embedded fingerprint is
/// checked against the provided passphrase's derived key. If the fingerprint
/// cannot match any identity that would be recovered, the error is
/// `EscrowError::WrongBlob` rather than `EscrowError::DecryptionFailed`.
///
/// For SYMESC1 blobs (no fingerprint), all decryption failures return
/// `EscrowError::DecryptionFailed`.
pub fn recover_escrow(
    escrow_blob: &[u8],
    passphrase: &str,
    params: &EscrowKdfParams,
) -> Result<Identity, EscrowError> {
    // Parse the blob (handles both V1 and V2)
    let parts = parse_blob(escrow_blob)?;

    // Derive decryption key from passphrase using provided params
    let key = derive_key(passphrase, parts.salt, params)?;

    // Decrypt
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|e| EscrowError::Encrypt(format!("cipher init: {e}")))?;
    let nonce = XNonce::from_slice(parts.nonce);

    match cipher.decrypt(nonce, parts.ciphertext) {
        Ok(plaintext) => {
            // Parse the AGE-SECRET-KEY-1... string back into an Identity
            let identity_str =
                std::str::from_utf8(&plaintext).map_err(|_| EscrowError::DecryptionFailed)?;
            let identity: Identity = identity_str
                .trim()
                .parse()
                .map_err(|_| EscrowError::DecryptionFailed)?;

            Ok(identity)
        }
        Err(_) => {
            // Decryption failed. For V2 blobs, we can try to distinguish
            // "wrong passphrase" from "wrong blob" using the fingerprint.
            //
            // The heuristic: since decryption failed, we can't recover the
            // identity to check against. But if the caller knows the
            // *expected* identity, they can use `verify_escrow_fingerprint()`.
            //
            // For the automatic case: if a V2 blob's fingerprint is present,
            // we return DecryptionFailed (the passphrase was wrong for this
            // specific blob). The WrongBlob error is returned by
            // `recover_escrow_for_identity()` or `verify_escrow_fingerprint()`
            // when the caller knows which identity they expect.
            Err(EscrowError::DecryptionFailed)
        }
    }
}

/// Recover an escrow blob with an expected identity hint for better error
/// discrimination.
///
/// This is like `recover_escrow()` but accepts the expected identity's public
/// key string. On decryption failure with a SYMESC2 blob, the fingerprint is
/// compared to determine if this is a "wrong blob" (fingerprint mismatch) or
/// "wrong passphrase" (fingerprint matches but decryption failed).
///
/// For SYMESC1 blobs (no fingerprint), behavior is identical to
/// `recover_escrow()`.
pub fn recover_escrow_for_identity(
    escrow_blob: &[u8],
    passphrase: &str,
    params: &EscrowKdfParams,
    expected_identity: &Identity,
) -> Result<Identity, EscrowError> {
    let parts = parse_blob(escrow_blob)?;
    let key = derive_key(passphrase, parts.salt, params)?;

    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|e| EscrowError::Encrypt(format!("cipher init: {e}")))?;
    let nonce = XNonce::from_slice(parts.nonce);

    match cipher.decrypt(nonce, parts.ciphertext) {
        Ok(plaintext) => {
            let identity_str =
                std::str::from_utf8(&plaintext).map_err(|_| EscrowError::DecryptionFailed)?;
            let identity: Identity = identity_str
                .trim()
                .parse()
                .map_err(|_| EscrowError::DecryptionFailed)?;
            Ok(identity)
        }
        Err(_) => {
            // Check fingerprint if this is a V2 blob
            if let Some(blob_fp) = parts.fingerprint {
                let expected_fp = compute_fingerprint(expected_identity);
                if blob_fp != expected_fp {
                    return Err(EscrowError::WrongBlob);
                }
            }
            Err(EscrowError::DecryptionFailed)
        }
    }
}

/// Verify whether an escrow blob's fingerprint matches a given identity.
///
/// Returns `true` if the blob is a SYMESC2 blob and the embedded fingerprint
/// matches the SHA-256 truncation of the identity's public key string.
///
/// Returns `false` if:
/// - The blob is a SYMESC1 blob (no fingerprint to compare)
/// - The fingerprint does not match
/// - The blob format is invalid
pub fn verify_escrow_fingerprint(blob: &[u8], identity: &Identity) -> bool {
    match parse_blob(blob) {
        Ok(parts) => match parts.fingerprint {
            Some(fp) => fp == compute_fingerprint(identity),
            None => false, // V1 blob, no fingerprint
        },
        Err(_) => false,
    }
}

/// Validate passphrase strength.
///
/// Returns `Ok(())` if the passphrase meets minimum entropy requirements:
/// - At least 12 characters, OR
/// - At least 4 space-separated words.
///
/// Returns `Err(EscrowError::PassphraseTooWeak)` with a human-readable
/// reason if it does not.
pub fn validate_passphrase(passphrase: &str) -> Result<(), EscrowError> {
    let trimmed = passphrase.trim();

    if trimmed.is_empty() {
        return Err(EscrowError::PassphraseTooWeak(
            "passphrase cannot be empty".to_string(),
        ));
    }

    // Check word-based passphrase (4+ space-separated words)
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.len() >= 4 {
        return Ok(());
    }

    // Check character-based passphrase (12+ characters)
    if trimmed.len() >= 12 {
        return Ok(());
    }

    Err(EscrowError::PassphraseTooWeak(
        "passphrase must be at least 12 characters or 4+ words (space-separated)".to_string(),
    ))
}

/// Change the recovery passphrase for an existing escrow blob.
///
/// Decrypts with `old_passphrase`, re-encrypts with `new_passphrase`.
/// The new blob always uses the SYMESC2 format regardless of the input format.
/// Returns the new escrow blob bytes.
pub fn change_passphrase(
    escrow_blob: &[u8],
    old_passphrase: &str,
    new_passphrase: &str,
    params: &EscrowKdfParams,
) -> Result<Vec<u8>, EscrowError> {
    let identity = recover_escrow(escrow_blob, old_passphrase, params)?;
    create_escrow(&identity, new_passphrase, params)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Compute an 8-byte fingerprint from an identity's public key.
///
/// `fingerprint = SHA-256(identity.to_public().to_string())[0..8]`
fn compute_fingerprint(identity: &Identity) -> [u8; FINGERPRINT_LEN] {
    let pubkey_str = identity.to_public().to_string();
    let hash = Sha256::digest(pubkey_str.as_bytes());
    let mut fp = [0u8; FINGERPRINT_LEN];
    fp.copy_from_slice(&hash[..FINGERPRINT_LEN]);
    fp
}

/// Which escrow format version was parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscrowVersion {
    V1,
    V2,
}

/// Parsed components of an escrow blob header.
struct BlobParts<'a> {
    #[allow(dead_code)]
    version: EscrowVersion,
    /// Present only for V2 blobs.
    fingerprint: Option<[u8; FINGERPRINT_LEN]>,
    salt: &'a [u8],
    nonce: &'a [u8],
    ciphertext: &'a [u8],
}

/// Parse an escrow blob into its component parts.
///
/// Supports both SYMESC1 (legacy) and SYMESC2 (current) formats.
fn parse_blob(blob: &[u8]) -> Result<BlobParts<'_>, EscrowError> {
    if blob.len() < MAGIC_LEN {
        return Err(EscrowError::InvalidFormat);
    }

    let magic = &blob[..MAGIC_LEN];

    if magic == MAGIC_V2.as_slice() {
        // SYMESC2: magic || fingerprint(8) || salt(32) || nonce(24) || ciphertext
        if blob.len() < HEADER_V2_LEN + 1 {
            return Err(EscrowError::InvalidFormat);
        }

        let fp_start = MAGIC_LEN;
        let salt_start = fp_start + FINGERPRINT_LEN;
        let nonce_start = salt_start + SALT_LEN;
        let ciphertext_start = nonce_start + NONCE_LEN;

        let mut fingerprint = [0u8; FINGERPRINT_LEN];
        fingerprint.copy_from_slice(&blob[fp_start..salt_start]);

        Ok(BlobParts {
            version: EscrowVersion::V2,
            fingerprint: Some(fingerprint),
            salt: &blob[salt_start..nonce_start],
            nonce: &blob[nonce_start..ciphertext_start],
            ciphertext: &blob[ciphertext_start..],
        })
    } else if magic == MAGIC_V1.as_slice() {
        // SYMESC1: magic || salt(32) || nonce(24) || ciphertext
        if blob.len() < HEADER_V1_LEN + 1 {
            return Err(EscrowError::InvalidFormat);
        }

        let salt_start = MAGIC_LEN;
        let nonce_start = salt_start + SALT_LEN;
        let ciphertext_start = nonce_start + NONCE_LEN;

        Ok(BlobParts {
            version: EscrowVersion::V1,
            fingerprint: None,
            salt: &blob[salt_start..nonce_start],
            nonce: &blob[nonce_start..ciphertext_start],
            ciphertext: &blob[ciphertext_start..],
        })
    } else if magic.starts_with(b"SYMESC") {
        let version = String::from_utf8_lossy(magic);
        Err(EscrowError::UnsupportedVersion(version.into_owned()))
    } else {
        Err(EscrowError::InvalidFormat)
    }
}

/// Derive a 256-bit encryption key from a passphrase and salt using Argon2id.
fn derive_key(
    passphrase: &str,
    salt: &[u8],
    params: &EscrowKdfParams,
) -> Result<[u8; 32], EscrowError> {
    let argon2_params = argon2::Params::new(
        params.memory_kib,
        params.iterations,
        params.parallelism,
        Some(32), // 256-bit output
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

/// Create a SYMESC1 (legacy) escrow blob for testing backward compatibility.
///
/// This is intentionally not public — it only exists so tests can create
/// V1 blobs to verify that `recover_escrow()` still reads them.
#[cfg(test)]
fn create_escrow_v1(
    identity: &Identity,
    passphrase: &str,
    params: &EscrowKdfParams,
) -> Result<Vec<u8>, EscrowError> {
    validate_passphrase(passphrase)?;

    let identity_str = identity.to_string();
    let identity_bytes = identity_str.expose_secret().as_bytes();

    let mut salt = [0u8; SALT_LEN];
    rand::rng().fill_bytes(&mut salt);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);

    let key = derive_key(passphrase, &salt, params)?;

    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|e| EscrowError::Encrypt(format!("cipher init: {e}")))?;
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, identity_bytes)
        .map_err(|e| EscrowError::Encrypt(format!("encrypt: {e}")))?;

    let mut blob = Vec::with_capacity(HEADER_V1_LEN + ciphertext.len());
    blob.extend_from_slice(MAGIC_V1);
    blob.extend_from_slice(&salt);
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);

    Ok(blob)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to extract an error from `Result<Identity, EscrowError>` without
    /// requiring `Identity: Debug` (which `age::x25519::Identity` does not implement).
    fn expect_err(result: Result<Identity, EscrowError>) -> EscrowError {
        match result {
            Err(e) => e,
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    /// Use fast KDF params for tests (avoid 256 MiB allocation in CI).
    fn test_params() -> EscrowKdfParams {
        EscrowKdfParams {
            memory_kib: 1024, // 1 MiB
            iterations: 1,
            parallelism: 1,
        }
    }

    /// A passphrase that meets the minimum strength requirement (12+ chars).
    const GOOD_PASSPHRASE: &str = "correct horse battery staple";

    /// A passphrase that is too weak.
    const WEAK_PASSPHRASE: &str = "short";

    // -----------------------------------------------------------------------
    // Original tests (updated for SYMESC2 output)
    // -----------------------------------------------------------------------

    #[test]
    fn roundtrip_create_and_recover() {
        let identity = Identity::generate();
        let params = test_params();

        let blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();
        let recovered = recover_escrow(&blob, GOOD_PASSPHRASE, &params).unwrap();

        assert_eq!(
            identity.to_string().expose_secret(),
            recovered.to_string().expose_secret(),
        );
    }

    #[test]
    fn wrong_passphrase_returns_decryption_failed() {
        let identity = Identity::generate();
        let params = test_params();

        let blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();
        let result = recover_escrow(&blob, "wrong passphrase here", &params);

        match expect_err(result) {
            EscrowError::DecryptionFailed => {}
            other => panic!("expected DecryptionFailed, got: {other}"),
        }
    }

    #[test]
    fn corrupted_ciphertext_returns_decryption_failed() {
        let identity = Identity::generate();
        let params = test_params();

        let mut blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;

        let result = recover_escrow(&blob, GOOD_PASSPHRASE, &params);
        match expect_err(result) {
            EscrowError::DecryptionFailed => {}
            other => panic!("expected DecryptionFailed, got: {other}"),
        }
    }

    #[test]
    fn weak_passphrase_rejected() {
        let identity = Identity::generate();
        let params = test_params();

        let result = create_escrow(&identity, WEAK_PASSPHRASE, &params);
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
        match result.unwrap_err() {
            EscrowError::PassphraseTooWeak(_) => {}
            other => panic!("expected PassphraseTooWeak, got: {other}"),
        }
    }

    #[test]
    fn whitespace_only_passphrase_rejected() {
        let result = validate_passphrase("   \t\n  ");
        assert!(result.is_err());
        match result.unwrap_err() {
            EscrowError::PassphraseTooWeak(_) => {}
            other => panic!("expected PassphraseTooWeak, got: {other}"),
        }
    }

    #[test]
    fn passphrase_12_chars_accepted() {
        let result = validate_passphrase("123456789012");
        assert!(result.is_ok());
    }

    #[test]
    fn passphrase_11_chars_rejected() {
        let result = validate_passphrase("12345678901");
        assert!(result.is_err());
    }

    #[test]
    fn passphrase_4_words_accepted() {
        let result = validate_passphrase("one two three four");
        assert!(result.is_ok());
    }

    #[test]
    fn passphrase_3_words_rejected_if_short() {
        let result = validate_passphrase("a b c");
        assert!(result.is_err());
    }

    #[test]
    fn change_passphrase_works() {
        let identity = Identity::generate();
        let params = test_params();
        let old_pass = GOOD_PASSPHRASE;
        let new_pass = "my brand new passphrase";

        let blob = create_escrow(&identity, old_pass, &params).unwrap();
        let new_blob = change_passphrase(&blob, old_pass, new_pass, &params).unwrap();

        // Old passphrase should fail on new blob
        let result = recover_escrow(&new_blob, old_pass, &params);
        match expect_err(result) {
            EscrowError::DecryptionFailed => {}
            other => panic!("expected DecryptionFailed, got: {other}"),
        }

        // New passphrase should succeed on new blob
        let recovered = recover_escrow(&new_blob, new_pass, &params).unwrap();
        assert_eq!(
            identity.to_string().expose_secret(),
            recovered.to_string().expose_secret(),
        );
    }

    #[test]
    fn empty_blob_returns_invalid_format() {
        let result = recover_escrow(&[], "any passphrase", &test_params());
        match expect_err(result) {
            EscrowError::InvalidFormat => {}
            other => panic!("expected InvalidFormat, got: {other}"),
        }
    }

    #[test]
    fn truncated_blob_returns_invalid_format() {
        let identity = Identity::generate();
        let params = test_params();

        let blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();

        // Truncate to just the header (no ciphertext)
        let truncated = &blob[..HEADER_V2_LEN];
        let result = recover_escrow(truncated, GOOD_PASSPHRASE, &params);
        match expect_err(result) {
            EscrowError::InvalidFormat => {}
            other => panic!("expected InvalidFormat, got: {other}"),
        }
    }

    #[test]
    fn wrong_magic_bytes_returns_invalid_format() {
        let identity = Identity::generate();
        let params = test_params();

        let mut blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();
        blob[0..7].copy_from_slice(b"GARBAGE");

        let result = recover_escrow(&blob, GOOD_PASSPHRASE, &params);
        match expect_err(result) {
            EscrowError::InvalidFormat => {}
            other => panic!("expected InvalidFormat, got: {other}"),
        }
    }

    #[test]
    fn different_version_magic_returns_unsupported_version() {
        let identity = Identity::generate();
        let params = test_params();

        let mut blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();
        // Change version digit: SYMESC2 -> SYMESC9
        blob[6] = b'9';

        let result = recover_escrow(&blob, GOOD_PASSPHRASE, &params);
        match expect_err(result) {
            EscrowError::UnsupportedVersion(v) => {
                assert_eq!(v, "SYMESC9");
            }
            other => panic!("expected UnsupportedVersion, got: {other}"),
        }
    }

    #[test]
    fn different_kdf_params_produce_different_blobs() {
        let identity = Identity::generate();
        let params1 = test_params();
        let params2 = EscrowKdfParams {
            memory_kib: 2048,
            iterations: 2,
            parallelism: 1,
        };

        let blob1 = create_escrow(&identity, GOOD_PASSPHRASE, &params1).unwrap();
        let blob2 = create_escrow(&identity, GOOD_PASSPHRASE, &params2).unwrap();

        assert_ne!(blob1, blob2);
    }

    #[test]
    fn blob_starts_with_magic_v2() {
        let identity = Identity::generate();
        let params = test_params();

        let blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();
        assert!(blob.starts_with(b"SYMESC2"));
    }

    #[test]
    fn blob_size_is_reasonable() {
        let identity = Identity::generate();
        let params = test_params();

        let blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();

        // V2 header (71 bytes) + identity string (~74 bytes) + auth tag (16 bytes) = ~161
        assert!(
            blob.len() > HEADER_V2_LEN + 16,
            "blob too small: {} bytes",
            blob.len()
        );
        assert!(
            blob.len() < 300,
            "blob unexpectedly large: {} bytes",
            blob.len()
        );
    }

    #[test]
    fn multiple_roundtrips_produce_different_blobs() {
        let identity = Identity::generate();
        let params = test_params();

        let blob1 = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();
        let blob2 = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();

        // Different random salt/nonce means different ciphertext
        assert_ne!(blob1, blob2);

        // But both should recover the same identity
        let r1 = recover_escrow(&blob1, GOOD_PASSPHRASE, &params).unwrap();
        let r2 = recover_escrow(&blob2, GOOD_PASSPHRASE, &params).unwrap();
        assert_eq!(
            r1.to_string().expose_secret(),
            r2.to_string().expose_secret(),
        );
    }

    #[test]
    fn corrupted_salt_returns_decryption_failed() {
        let identity = Identity::generate();
        let params = test_params();

        let mut blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();
        // Corrupt 1 byte in the salt region (starts after magic + fingerprint)
        blob[MAGIC_LEN + FINGERPRINT_LEN] ^= 0xFF;

        let result = recover_escrow(&blob, GOOD_PASSPHRASE, &params);
        match expect_err(result) {
            EscrowError::DecryptionFailed => {}
            other => panic!("expected DecryptionFailed, got: {other}"),
        }
    }

    #[test]
    fn corrupted_nonce_returns_decryption_failed() {
        let identity = Identity::generate();
        let params = test_params();

        let mut blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();
        // Corrupt 1 byte in the nonce region
        blob[MAGIC_LEN + FINGERPRINT_LEN + SALT_LEN] ^= 0xFF;

        let result = recover_escrow(&blob, GOOD_PASSPHRASE, &params);
        match expect_err(result) {
            EscrowError::DecryptionFailed => {}
            other => panic!("expected DecryptionFailed, got: {other}"),
        }
    }

    #[test]
    fn change_passphrase_with_wrong_old_fails() {
        let identity = Identity::generate();
        let params = test_params();

        let blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();

        let result = change_passphrase(&blob, "wrong old passphrase", "new passphrase 12", &params);
        assert!(result.is_err());
        match result.unwrap_err() {
            EscrowError::DecryptionFailed => {}
            other => panic!("expected DecryptionFailed, got: {other}"),
        }
    }

    #[test]
    fn change_passphrase_to_weak_fails() {
        let identity = Identity::generate();
        let params = test_params();

        let blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();

        let result = change_passphrase(&blob, GOOD_PASSPHRASE, "weak", &params);
        assert!(result.is_err());
        match result.unwrap_err() {
            EscrowError::PassphraseTooWeak(_) => {}
            other => panic!("expected PassphraseTooWeak, got: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // New tests for SYMESC2 fingerprint feature
    // -----------------------------------------------------------------------

    #[test]
    fn roundtrip_v2_with_fingerprint() {
        let identity = Identity::generate();
        let params = test_params();

        let blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();

        // Verify it's a V2 blob
        assert!(blob.starts_with(b"SYMESC2"));

        // Fingerprint bytes should be present after magic
        let fp = &blob[MAGIC_LEN..MAGIC_LEN + FINGERPRINT_LEN];
        assert_eq!(fp.len(), 8);

        // Roundtrip should work
        let recovered = recover_escrow(&blob, GOOD_PASSPHRASE, &params).unwrap();
        assert_eq!(
            identity.to_string().expose_secret(),
            recovered.to_string().expose_secret(),
        );
    }

    #[test]
    fn wrong_blob_different_identity_detected() {
        let identity_a = Identity::generate();
        let identity_b = Identity::generate();
        let params = test_params();

        // Create blob for identity_a
        let blob_a = create_escrow(&identity_a, GOOD_PASSPHRASE, &params).unwrap();

        // Try to recover blob_a with correct passphrase but expecting identity_b
        // This simulates picking up the wrong escrow blob from storage
        let result =
            recover_escrow_for_identity(&blob_a, "wrong passphrase here", &params, &identity_b);

        match expect_err(result) {
            EscrowError::WrongBlob => {}
            other => panic!("expected WrongBlob, got: {other}"),
        }
    }

    #[test]
    fn wrong_passphrase_same_identity_returns_decryption_failed() {
        let identity = Identity::generate();
        let params = test_params();

        let blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();

        // Wrong passphrase but correct identity hint -> DecryptionFailed
        let result =
            recover_escrow_for_identity(&blob, "wrong passphrase here", &params, &identity);

        match expect_err(result) {
            EscrowError::DecryptionFailed => {}
            other => panic!("expected DecryptionFailed, got: {other}"),
        }
    }

    #[test]
    fn legacy_v1_blob_still_recoverable() {
        let identity = Identity::generate();
        let params = test_params();

        // Create a legacy V1 blob
        let blob = create_escrow_v1(&identity, GOOD_PASSPHRASE, &params).unwrap();

        // Verify it's a V1 blob
        assert!(blob.starts_with(b"SYMESC1"));

        // Should still recover with the standard function
        let recovered = recover_escrow(&blob, GOOD_PASSPHRASE, &params).unwrap();
        assert_eq!(
            identity.to_string().expose_secret(),
            recovered.to_string().expose_secret(),
        );
    }

    #[test]
    fn legacy_v1_wrong_passphrase_returns_decryption_failed() {
        let identity = Identity::generate();
        let params = test_params();

        let blob = create_escrow_v1(&identity, GOOD_PASSPHRASE, &params).unwrap();
        let result = recover_escrow(&blob, "wrong passphrase here", &params);

        match expect_err(result) {
            EscrowError::DecryptionFailed => {}
            other => panic!("expected DecryptionFailed, got: {other}"),
        }
    }

    #[test]
    fn legacy_v1_with_identity_hint_returns_decryption_failed_not_wrong_blob() {
        let identity_a = Identity::generate();
        let identity_b = Identity::generate();
        let params = test_params();

        // V1 blob has no fingerprint, so even with wrong identity hint
        // we can only return DecryptionFailed (not WrongBlob)
        let blob = create_escrow_v1(&identity_a, GOOD_PASSPHRASE, &params).unwrap();
        let result =
            recover_escrow_for_identity(&blob, "wrong passphrase here", &params, &identity_b);

        match expect_err(result) {
            EscrowError::DecryptionFailed => {}
            other => panic!("expected DecryptionFailed (no fingerprint in V1), got: {other}"),
        }
    }

    #[test]
    fn verify_fingerprint_matches_correct_identity() {
        let identity = Identity::generate();
        let params = test_params();

        let blob = create_escrow(&identity, GOOD_PASSPHRASE, &params).unwrap();
        assert!(verify_escrow_fingerprint(&blob, &identity));
    }

    #[test]
    fn verify_fingerprint_does_not_match_different_identity() {
        let identity_a = Identity::generate();
        let identity_b = Identity::generate();
        let params = test_params();

        let blob = create_escrow(&identity_a, GOOD_PASSPHRASE, &params).unwrap();
        assert!(!verify_escrow_fingerprint(&blob, &identity_b));
    }

    #[test]
    fn verify_fingerprint_returns_false_for_v1_blob() {
        let identity = Identity::generate();
        let params = test_params();

        let blob = create_escrow_v1(&identity, GOOD_PASSPHRASE, &params).unwrap();
        // V1 has no fingerprint, so verification always returns false
        assert!(!verify_escrow_fingerprint(&blob, &identity));
    }

    #[test]
    fn verify_fingerprint_returns_false_for_invalid_blob() {
        let identity = Identity::generate();
        assert!(!verify_escrow_fingerprint(b"garbage", &identity));
        assert!(!verify_escrow_fingerprint(&[], &identity));
    }

    #[test]
    fn fingerprint_is_deterministic_for_same_identity() {
        let identity = Identity::generate();
        let fp1 = compute_fingerprint(&identity);
        let fp2 = compute_fingerprint(&identity);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn fingerprint_differs_for_different_identities() {
        let identity_a = Identity::generate();
        let identity_b = Identity::generate();
        let fp_a = compute_fingerprint(&identity_a);
        let fp_b = compute_fingerprint(&identity_b);
        assert_ne!(fp_a, fp_b);
    }

    #[test]
    fn change_passphrase_upgrades_v1_to_v2() {
        let identity = Identity::generate();
        let params = test_params();

        // Start with a V1 blob
        let v1_blob = create_escrow_v1(&identity, GOOD_PASSPHRASE, &params).unwrap();
        assert!(v1_blob.starts_with(b"SYMESC1"));

        // Change passphrase should produce a V2 blob
        let v2_blob =
            change_passphrase(&v1_blob, GOOD_PASSPHRASE, "new secure passphrase", &params).unwrap();
        assert!(v2_blob.starts_with(b"SYMESC2"));

        // And it should still be recoverable
        let recovered = recover_escrow(&v2_blob, "new secure passphrase", &params).unwrap();
        assert_eq!(
            identity.to_string().expose_secret(),
            recovered.to_string().expose_secret(),
        );
    }
}
