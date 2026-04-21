//! The core blob store implementation.
//!
//! Provides encrypted storage using the age format with X25519 keys.
//! Decrypted content is held only in memory -- never written to disk.

use std::io::{Read, Write};
use std::path::PathBuf;

use age::x25519::{Identity, Recipient};

use crate::error::VaultStoreError;
use crate::types::{BlobCategory, BlobMetadata, EncryptedBlob};

/// Index file name within the store root directory.
const INDEX_FILE: &str = "index.json";

/// Age-encrypted blob store.
///
/// Stores sensitive data as individually encrypted `.age` files with an
/// unencrypted metadata index (`index.json`) for search and listing.
///
/// # Key management
///
/// The store does not manage keys itself. Callers must provide:
/// - One or more `Recipient`s (public keys) for encryption via `store()`
/// - An `Identity` (secret key) for decryption via `read()`
/// - Both for `rotate_key()`
///
/// Keys are expected to be held in the symbiotic-trust Vault.
///
/// # Multi-recipient encryption
///
/// Each blob can be encrypted to multiple recipients (e.g., multiple devices).
/// Any of the corresponding identities can decrypt the blob. Use
/// `add_recipient()` to grant access to an additional device without
/// requiring the original passphrase or re-uploading content.
pub struct BlobStore {
    /// Root directory for the blob store.
    root: PathBuf,
}

impl BlobStore {
    /// Create a new blob store rooted at the given directory.
    ///
    /// Creates the directory if it does not exist.
    pub fn new(root: PathBuf) -> Result<Self, VaultStoreError> {
        std::fs::create_dir_all(&root)?;
        let store = Self { root };
        // Ensure index.json exists
        if !store.index_path().exists() {
            store.write_index(&[])?;
        }
        Ok(store)
    }

    /// Store a new encrypted blob.
    ///
    /// The content is encrypted with age using the provided recipients'
    /// public keys. Any of the corresponding identities can decrypt the blob.
    /// The encrypted data is written to `{id}.age` and the metadata is added
    /// to `index.json`.
    ///
    /// # Arguments
    ///
    /// * `recipients` - One or more recipients to encrypt to. For
    ///   single-recipient usage, pass a slice of one element.
    ///
    /// Returns the created `EncryptedBlob` entry.
    pub fn store(
        &self,
        id: &str,
        category: BlobCategory,
        metadata: BlobMetadata,
        content: &[u8],
        recipients: &[&Recipient],
    ) -> Result<EncryptedBlob, VaultStoreError> {
        if recipients.is_empty() {
            return Err(VaultStoreError::NoRecipients);
        }

        // Encrypt content to all recipients
        let encrypted = encrypt_content(content, recipients)?;

        // Write encrypted file atomically (write to temp, then rename)
        let age_path = self.blob_path(id);
        atomic_write(&age_path, &encrypted)?;

        // Create index entry
        let blob = EncryptedBlob {
            id: id.to_string(),
            category,
            created_at: now_unix(),
            metadata,
        };

        // Update index atomically
        let mut index = self.read_index()?;
        // Remove existing entry with same id (upsert behavior)
        index.retain(|b| b.id != id);
        index.push(blob.clone());
        self.write_index(&index)?;

        Ok(blob)
    }

    /// Decrypt and read a blob's content.
    ///
    /// Returns the plaintext only in memory -- never written to disk.
    /// Requires any one of the identities (secret keys) that correspond to
    /// the recipients used during encryption.
    pub fn read(&self, id: &str, identity: &Identity) -> Result<Vec<u8>, VaultStoreError> {
        // Verify blob exists in index
        let index = self.read_index()?;
        if !index.iter().any(|b| b.id == id) {
            return Err(VaultStoreError::NotFound(id.to_string()));
        }

        // Read encrypted file
        let age_path = self.blob_path(id);
        if !age_path.exists() {
            return Err(VaultStoreError::FileMissing(id.to_string()));
        }

        let encrypted = std::fs::read(&age_path)?;
        decrypt_content(&encrypted, identity)
    }

    /// List blobs, optionally filtered by category.
    ///
    /// Reads metadata only -- no decryption occurs.
    pub fn list(
        &self,
        category: Option<&BlobCategory>,
    ) -> Result<Vec<EncryptedBlob>, VaultStoreError> {
        let index = self.read_index()?;
        match category {
            Some(cat) => Ok(index.into_iter().filter(|b| &b.category == cat).collect()),
            None => Ok(index),
        }
    }

    /// Delete a blob and its encrypted file.
    ///
    /// Removes both the `.age` file and the index entry.
    /// Returns `Ok(())` even if the `.age` file is already missing
    /// (the index entry is still cleaned up).
    pub fn delete(&self, id: &str) -> Result<(), VaultStoreError> {
        // Remove from index
        let mut index = self.read_index()?;
        let before = index.len();
        index.retain(|b| b.id != id);
        if index.len() == before {
            return Err(VaultStoreError::NotFound(id.to_string()));
        }
        self.write_index(&index)?;

        // Remove .age file (ignore if already gone)
        let age_path = self.blob_path(id);
        if age_path.exists() {
            std::fs::remove_file(&age_path)?;
        }

        Ok(())
    }

    /// Re-encrypt all blobs with new keys.
    ///
    /// For each blob:
    /// 1. Decrypt with `old_identity`
    /// 2. Re-encrypt with `new_recipients`
    /// 3. Overwrite the `.age` file atomically
    ///
    /// Returns the number of blobs rotated.
    ///
    /// If rotation is interrupted (e.g., power loss), some blobs will be
    /// encrypted with the old key and some with the new key. This is safe --
    /// the caller should keep both keys until rotation completes.
    pub fn rotate_key(
        &self,
        old_identity: &Identity,
        new_recipients: &[&Recipient],
    ) -> Result<u32, VaultStoreError> {
        if new_recipients.is_empty() {
            return Err(VaultStoreError::NoRecipients);
        }

        let index = self.read_index()?;
        let mut count = 0u32;

        for blob in &index {
            let age_path = self.blob_path(&blob.id);
            if !age_path.exists() {
                // Skip missing files -- they'll be caught by a future audit
                continue;
            }

            // Decrypt with old key
            let encrypted = std::fs::read(&age_path)?;
            let plaintext = decrypt_content(&encrypted, old_identity)?;

            // Re-encrypt with new keys
            let re_encrypted = encrypt_content(&plaintext, new_recipients)?;

            // Overwrite atomically
            atomic_write(&age_path, &re_encrypted)?;

            count += 1;
        }

        Ok(count)
    }

    /// Add a recipient to an existing blob.
    ///
    /// Decrypts the blob with `identity`, then re-encrypts with all
    /// `new_recipients` (which should include the original recipients
    /// plus the new one).
    ///
    /// This is a convenience method for the common case of granting a new
    /// device access to an existing blob.
    pub fn add_recipient(
        &self,
        id: &str,
        identity: &Identity,
        new_recipients: &[&Recipient],
    ) -> Result<(), VaultStoreError> {
        if new_recipients.is_empty() {
            return Err(VaultStoreError::NoRecipients);
        }

        // Read and decrypt
        let age_path = self.blob_path(id);
        if !age_path.exists() {
            // Check if it's in the index at all
            let index = self.read_index()?;
            if !index.iter().any(|b| b.id == id) {
                return Err(VaultStoreError::NotFound(id.to_string()));
            }
            return Err(VaultStoreError::FileMissing(id.to_string()));
        }

        let encrypted = std::fs::read(&age_path)?;
        let plaintext = decrypt_content(&encrypted, identity)?;

        // Re-encrypt with expanded recipient list
        let re_encrypted = encrypt_content(&plaintext, new_recipients)?;

        // Overwrite atomically
        atomic_write(&age_path, &re_encrypted)?;

        Ok(())
    }

    /// Return the root directory of this blob store.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    // --- Private helpers ---

    fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILE)
    }

    fn blob_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.age"))
    }

    fn read_index(&self) -> Result<Vec<EncryptedBlob>, VaultStoreError> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let data = std::fs::read_to_string(&path)?;
        let index: Vec<EncryptedBlob> = serde_json::from_str(&data)?;
        Ok(index)
    }

    fn write_index(&self, index: &[EncryptedBlob]) -> Result<(), VaultStoreError> {
        let data = serde_json::to_string_pretty(index)?;
        atomic_write(&self.index_path(), data.as_bytes())?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Encryption helpers
// ---------------------------------------------------------------------------

/// Encrypt content using age with the given recipients' public keys.
///
/// All recipients can decrypt the resulting ciphertext independently.
fn encrypt_content(
    plaintext: &[u8],
    recipients: &[&Recipient],
) -> Result<Vec<u8>, VaultStoreError> {
    let recipients_iter = recipients.iter().map(|r| *r as &dyn age::Recipient);
    let encryptor = age::Encryptor::with_recipients(recipients_iter)
        .map_err(|e| VaultStoreError::Encrypt(format!("{e}")))?;

    let mut encrypted = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut encrypted)
        .map_err(|e| VaultStoreError::Encrypt(format!("{e}")))?;
    writer
        .write_all(plaintext)
        .map_err(|e| VaultStoreError::Encrypt(format!("write: {e}")))?;
    writer
        .finish()
        .map_err(|e| VaultStoreError::Encrypt(format!("finish: {e}")))?;

    Ok(encrypted)
}

/// Decrypt age-encrypted content using the given identity (secret key).
fn decrypt_content(encrypted: &[u8], identity: &Identity) -> Result<Vec<u8>, VaultStoreError> {
    let decryptor = age::Decryptor::new_buffered(encrypted)
        .map_err(|e| VaultStoreError::Decrypt(format!("{e}")))?;

    let identity_trait: &dyn age::Identity = identity;
    let mut reader = decryptor
        .decrypt(std::iter::once(identity_trait))
        .map_err(|e| VaultStoreError::Decrypt(format!("{e}")))?;

    let mut plaintext = Vec::new();
    reader
        .read_to_end(&mut plaintext)
        .map_err(|e| VaultStoreError::Decrypt(format!("read: {e}")))?;

    Ok(plaintext)
}

// ---------------------------------------------------------------------------
// File helpers
// ---------------------------------------------------------------------------

/// Write data to a file atomically: write to a temp file in the same
/// directory, then rename. This ensures the target file is never in a
/// partially-written state.
fn atomic_write(path: &std::path::Path, data: &[u8]) -> Result<(), std::io::Error> {
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));

    // Create temp file in the same directory (same filesystem for rename)
    let temp_path = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));

    // Write content
    std::fs::write(&temp_path, data)?;

    // Atomic rename
    std::fs::rename(&temp_path, path)?;

    Ok(())
}

/// Return the current Unix timestamp in seconds.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock must be after epoch")
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper: generate a fresh X25519 keypair for testing.
    fn test_keypair() -> (Identity, Recipient) {
        let identity = Identity::generate();
        let recipient = identity.to_public();
        (identity, recipient)
    }

    /// Helper: create a BlobMetadata for testing.
    fn test_metadata(title: &str, size: u64) -> BlobMetadata {
        BlobMetadata {
            title: title.to_string(),
            tags: vec!["test".to_string()],
            size_bytes: size,
            content_type: "text/plain".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // Original tests (updated for multi-recipient signature)
    // -----------------------------------------------------------------------

    #[test]
    fn store_and_read_back() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (identity, recipient) = test_keypair();

        let content = b"This is my medical record.";
        let meta = test_metadata("Blood Test Results", content.len() as u64);

        let blob = store
            .store(
                "med-blood-2026",
                BlobCategory::Medical,
                meta.clone(),
                content,
                &[&recipient],
            )
            .unwrap();

        assert_eq!(blob.id, "med-blood-2026");
        assert_eq!(blob.category, BlobCategory::Medical);
        assert_eq!(blob.metadata.title, "Blood Test Results");
        assert_eq!(blob.metadata.size_bytes, content.len() as u64);

        // Read back
        let plaintext = store.read("med-blood-2026", &identity).unwrap();
        assert_eq!(plaintext, content);
    }

    #[test]
    fn store_and_read_large_content() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (identity, recipient) = test_keypair();

        // 1 MB of random-ish data
        let content: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();
        let meta = test_metadata("Large File", content.len() as u64);

        store
            .store(
                "large-blob",
                BlobCategory::Financial,
                meta,
                &content,
                &[&recipient],
            )
            .unwrap();

        let plaintext = store.read("large-blob", &identity).unwrap();
        assert_eq!(plaintext, content);
    }

    #[test]
    fn list_all_blobs() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (_identity, recipient) = test_keypair();

        store
            .store(
                "a",
                BlobCategory::Medical,
                test_metadata("A", 1),
                b"a",
                &[&recipient],
            )
            .unwrap();
        store
            .store(
                "b",
                BlobCategory::Financial,
                test_metadata("B", 1),
                b"b",
                &[&recipient],
            )
            .unwrap();
        store
            .store(
                "c",
                BlobCategory::Medical,
                test_metadata("C", 1),
                b"c",
                &[&recipient],
            )
            .unwrap();

        let all = store.list(None).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn list_by_category() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (_identity, recipient) = test_keypair();

        store
            .store(
                "a",
                BlobCategory::Medical,
                test_metadata("A", 1),
                b"a",
                &[&recipient],
            )
            .unwrap();
        store
            .store(
                "b",
                BlobCategory::Financial,
                test_metadata("B", 1),
                b"b",
                &[&recipient],
            )
            .unwrap();
        store
            .store(
                "c",
                BlobCategory::Medical,
                test_metadata("C", 1),
                b"c",
                &[&recipient],
            )
            .unwrap();

        let medical = store.list(Some(&BlobCategory::Medical)).unwrap();
        assert_eq!(medical.len(), 2);
        assert!(medical.iter().all(|b| b.category == BlobCategory::Medical));

        let financial = store.list(Some(&BlobCategory::Financial)).unwrap();
        assert_eq!(financial.len(), 1);

        let legal = store.list(Some(&BlobCategory::Legal)).unwrap();
        assert_eq!(legal.len(), 0);
    }

    #[test]
    fn list_custom_category() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (_identity, recipient) = test_keypair();

        let custom = BlobCategory::Custom("insurance".to_string());
        store
            .store(
                "ins-1",
                custom.clone(),
                test_metadata("Policy", 1),
                b"x",
                &[&recipient],
            )
            .unwrap();

        let results = store.list(Some(&custom)).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "ins-1");
    }

    #[test]
    fn delete_blob() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (identity, recipient) = test_keypair();

        store
            .store(
                "del-me",
                BlobCategory::Legal,
                test_metadata("Contract", 5),
                b"hello",
                &[&recipient],
            )
            .unwrap();

        // Verify it exists
        assert_eq!(store.list(None).unwrap().len(), 1);
        assert!(store.read("del-me", &identity).is_ok());

        // Delete
        store.delete("del-me").unwrap();

        // Verify gone
        assert_eq!(store.list(None).unwrap().len(), 0);
        assert!(!dir.path().join("del-me.age").exists());
    }

    #[test]
    fn delete_nonexistent_blob_errors() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();

        let result = store.delete("nonexistent");
        assert!(result.is_err());
        match result.unwrap_err() {
            VaultStoreError::NotFound(id) => assert_eq!(id, "nonexistent"),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn read_nonexistent_blob_errors() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (identity, _recipient) = test_keypair();

        let result = store.read("nonexistent", &identity);
        assert!(result.is_err());
        match result.unwrap_err() {
            VaultStoreError::NotFound(id) => assert_eq!(id, "nonexistent"),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn read_with_wrong_key_errors() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (_identity1, recipient1) = test_keypair();
        let (identity2, _recipient2) = test_keypair();

        store
            .store(
                "secret",
                BlobCategory::Credential,
                test_metadata("API Key", 10),
                b"super-secret",
                &[&recipient1],
            )
            .unwrap();

        // Try to decrypt with wrong key
        let result = store.read("secret", &identity2);
        assert!(result.is_err());
        match result.unwrap_err() {
            VaultStoreError::Decrypt(_) => {} // expected
            other => panic!("expected Decrypt error, got: {other}"),
        }
    }

    #[test]
    fn key_rotation() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (old_identity, old_recipient) = test_keypair();
        let (new_identity, new_recipient) = test_keypair();

        // Store some blobs with old key
        store
            .store(
                "a",
                BlobCategory::Medical,
                test_metadata("A", 1),
                b"alpha",
                &[&old_recipient],
            )
            .unwrap();
        store
            .store(
                "b",
                BlobCategory::Financial,
                test_metadata("B", 1),
                b"bravo",
                &[&old_recipient],
            )
            .unwrap();
        store
            .store(
                "c",
                BlobCategory::Legal,
                test_metadata("C", 1),
                b"charlie",
                &[&old_recipient],
            )
            .unwrap();

        // Verify old key works
        assert_eq!(store.read("a", &old_identity).unwrap(), b"alpha");

        // Rotate
        let count = store.rotate_key(&old_identity, &[&new_recipient]).unwrap();
        assert_eq!(count, 3);

        // Old key should fail
        assert!(store.read("a", &old_identity).is_err());

        // New key should work
        assert_eq!(store.read("a", &new_identity).unwrap(), b"alpha");
        assert_eq!(store.read("b", &new_identity).unwrap(), b"bravo");
        assert_eq!(store.read("c", &new_identity).unwrap(), b"charlie");
    }

    #[test]
    fn key_rotation_empty_store() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (old_identity, _old_recipient) = test_keypair();
        let (_new_identity, new_recipient) = test_keypair();

        let count = store.rotate_key(&old_identity, &[&new_recipient]).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn empty_store_operations() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();

        // List on empty store
        let all = store.list(None).unwrap();
        assert!(all.is_empty());

        let medical = store.list(Some(&BlobCategory::Medical)).unwrap();
        assert!(medical.is_empty());
    }

    #[test]
    fn store_upserts_on_same_id() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (identity, recipient) = test_keypair();

        // Store initial version
        store
            .store(
                "doc-1",
                BlobCategory::Legal,
                test_metadata("Contract v1", 5),
                b"v1",
                &[&recipient],
            )
            .unwrap();

        // Overwrite with same id
        store
            .store(
                "doc-1",
                BlobCategory::Legal,
                test_metadata("Contract v2", 5),
                b"v2",
                &[&recipient],
            )
            .unwrap();

        // Index should have one entry
        let all = store.list(None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].metadata.title, "Contract v2");

        // Content should be updated
        let plaintext = store.read("doc-1", &identity).unwrap();
        assert_eq!(plaintext, b"v2");
    }

    #[test]
    fn index_file_is_valid_json() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (_identity, recipient) = test_keypair();

        store
            .store(
                "x",
                BlobCategory::Medical,
                test_metadata("X", 1),
                b"x",
                &[&recipient],
            )
            .unwrap();

        // Read index.json directly and parse
        let index_content = std::fs::read_to_string(dir.path().join("index.json")).unwrap();
        let parsed: Vec<EncryptedBlob> = serde_json::from_str(&index_content).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "x");
    }

    #[test]
    fn age_file_exists_on_disk() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (_identity, recipient) = test_keypair();

        store
            .store(
                "test-blob",
                BlobCategory::Financial,
                test_metadata("Test", 1),
                b"x",
                &[&recipient],
            )
            .unwrap();

        assert!(dir.path().join("test-blob.age").exists());
    }

    #[test]
    fn encrypted_file_is_not_plaintext() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (_identity, recipient) = test_keypair();

        let content = b"This should be encrypted and not readable as plaintext on disk.";
        store
            .store(
                "enc-check",
                BlobCategory::Credential,
                test_metadata("Enc Check", content.len() as u64),
                content,
                &[&recipient],
            )
            .unwrap();

        let on_disk = std::fs::read(dir.path().join("enc-check.age")).unwrap();
        // The encrypted file should NOT contain the original plaintext
        assert!(
            !on_disk.windows(content.len()).any(|w| w == content),
            "plaintext found in encrypted file!"
        );
    }

    #[test]
    fn blob_metadata_roundtrip() {
        let meta = BlobMetadata {
            title: "Tax Return 2025".to_string(),
            tags: vec![
                "tax".to_string(),
                "2025".to_string(),
                "important".to_string(),
            ],
            size_bytes: 1_234_567,
            content_type: "application/pdf".to_string(),
        };

        let json = serde_json::to_string(&meta).unwrap();
        let parsed: BlobMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, parsed);
    }

    #[test]
    fn blob_category_serialization() {
        // Standard categories
        assert_eq!(
            serde_json::to_string(&BlobCategory::Medical).unwrap(),
            "\"medical\""
        );
        assert_eq!(
            serde_json::to_string(&BlobCategory::Financial).unwrap(),
            "\"financial\""
        );
        assert_eq!(
            serde_json::to_string(&BlobCategory::Legal).unwrap(),
            "\"legal\""
        );
        assert_eq!(
            serde_json::to_string(&BlobCategory::Credential).unwrap(),
            "\"credential\""
        );

        // Custom category
        let custom = BlobCategory::Custom("insurance".to_string());
        let json = serde_json::to_string(&custom).unwrap();
        let parsed: BlobCategory = serde_json::from_str(&json).unwrap();
        assert_eq!(custom, parsed);
    }

    #[test]
    fn blob_category_display() {
        assert_eq!(BlobCategory::Medical.to_string(), "medical");
        assert_eq!(BlobCategory::Financial.to_string(), "financial");
        assert_eq!(BlobCategory::Legal.to_string(), "legal");
        assert_eq!(BlobCategory::Credential.to_string(), "credential");
        assert_eq!(
            BlobCategory::Custom("insurance".to_string()).to_string(),
            "custom:insurance"
        );
    }

    #[test]
    fn read_missing_age_file_returns_file_missing() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (identity, recipient) = test_keypair();

        // Store a blob
        store
            .store(
                "ghost",
                BlobCategory::Medical,
                test_metadata("Ghost", 1),
                b"x",
                &[&recipient],
            )
            .unwrap();

        // Manually delete the .age file (simulating corruption)
        std::fs::remove_file(dir.path().join("ghost.age")).unwrap();

        // Read should return FileMissing, not NotFound
        let result = store.read("ghost", &identity);
        assert!(result.is_err());
        match result.unwrap_err() {
            VaultStoreError::FileMissing(id) => assert_eq!(id, "ghost"),
            other => panic!("expected FileMissing, got: {other}"),
        }
    }

    #[test]
    fn store_creates_root_directory() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a").join("b").join("c");

        // Directory doesn't exist yet
        assert!(!nested.exists());

        let store = BlobStore::new(nested.clone()).unwrap();
        assert!(nested.exists());
        assert!(store.list(None).unwrap().is_empty());
    }

    #[test]
    fn multiple_stores_share_directory() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let (_identity, recipient) = test_keypair();

        // First store instance writes a blob
        {
            let store = BlobStore::new(root.clone()).unwrap();
            store
                .store(
                    "shared-1",
                    BlobCategory::Medical,
                    test_metadata("S1", 1),
                    b"a",
                    &[&recipient],
                )
                .unwrap();
        }

        // Second store instance reads back
        {
            let store = BlobStore::new(root).unwrap();
            let all = store.list(None).unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].id, "shared-1");
        }
    }

    #[test]
    fn empty_content_blob() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (identity, recipient) = test_keypair();

        store
            .store(
                "empty",
                BlobCategory::Credential,
                test_metadata("Empty", 0),
                b"",
                &[&recipient],
            )
            .unwrap();

        let plaintext = store.read("empty", &identity).unwrap();
        assert!(plaintext.is_empty());
    }

    // -----------------------------------------------------------------------
    // New tests for multi-recipient support
    // -----------------------------------------------------------------------

    #[test]
    fn store_with_two_recipients_read_with_either() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (identity1, recipient1) = test_keypair();
        let (identity2, recipient2) = test_keypair();

        let content = b"shared secret between two devices";
        store
            .store(
                "shared",
                BlobCategory::Credential,
                test_metadata("Shared", content.len() as u64),
                content,
                &[&recipient1, &recipient2],
            )
            .unwrap();

        // Both identities should be able to decrypt
        let pt1 = store.read("shared", &identity1).unwrap();
        assert_eq!(pt1, content);

        let pt2 = store.read("shared", &identity2).unwrap();
        assert_eq!(pt2, content);
    }

    #[test]
    fn store_with_two_recipients_wrong_key_fails() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (_identity1, recipient1) = test_keypair();
        let (_identity2, recipient2) = test_keypair();
        let (identity3, _recipient3) = test_keypair();

        store
            .store(
                "shared",
                BlobCategory::Credential,
                test_metadata("Shared", 5),
                b"hello",
                &[&recipient1, &recipient2],
            )
            .unwrap();

        // Third identity should NOT be able to decrypt
        let result = store.read("shared", &identity3);
        assert!(result.is_err());
        match result.unwrap_err() {
            VaultStoreError::Decrypt(_) => {}
            other => panic!("expected Decrypt error, got: {other}"),
        }
    }

    #[test]
    fn add_recipient_allows_new_device_to_decrypt() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (identity1, recipient1) = test_keypair();
        let (identity2, recipient2) = test_keypair();
        let (identity3, recipient3) = test_keypair();

        let content = b"evolving access control";

        // Initially encrypted to device 1 and 2
        store
            .store(
                "evolve",
                BlobCategory::Medical,
                test_metadata("Evolving", content.len() as u64),
                content,
                &[&recipient1, &recipient2],
            )
            .unwrap();

        // Device 3 cannot decrypt yet
        assert!(store.read("evolve", &identity3).is_err());

        // Add device 3 (re-encrypt with all three recipients)
        store
            .add_recipient(
                "evolve",
                &identity1,
                &[&recipient1, &recipient2, &recipient3],
            )
            .unwrap();

        // Now all three can decrypt
        assert_eq!(store.read("evolve", &identity1).unwrap(), content);
        assert_eq!(store.read("evolve", &identity2).unwrap(), content);
        assert_eq!(store.read("evolve", &identity3).unwrap(), content);
    }

    #[test]
    fn add_recipient_to_nonexistent_blob_errors() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (identity1, _recipient1) = test_keypair();
        let (_identity2, recipient2) = test_keypair();

        let result = store.add_recipient("nonexistent", &identity1, &[&recipient2]);
        assert!(result.is_err());
        match result.unwrap_err() {
            VaultStoreError::NotFound(id) => assert_eq!(id, "nonexistent"),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn rotate_key_with_multiple_recipients() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (old_identity, old_recipient) = test_keypair();
        let (new_identity1, new_recipient1) = test_keypair();
        let (new_identity2, new_recipient2) = test_keypair();

        // Store with old key
        store
            .store(
                "a",
                BlobCategory::Medical,
                test_metadata("A", 1),
                b"alpha",
                &[&old_recipient],
            )
            .unwrap();
        store
            .store(
                "b",
                BlobCategory::Financial,
                test_metadata("B", 1),
                b"bravo",
                &[&old_recipient],
            )
            .unwrap();

        // Rotate to two new recipients
        let count = store
            .rotate_key(&old_identity, &[&new_recipient1, &new_recipient2])
            .unwrap();
        assert_eq!(count, 2);

        // Old key should fail
        assert!(store.read("a", &old_identity).is_err());

        // Both new keys should work
        assert_eq!(store.read("a", &new_identity1).unwrap(), b"alpha");
        assert_eq!(store.read("a", &new_identity2).unwrap(), b"alpha");
        assert_eq!(store.read("b", &new_identity1).unwrap(), b"bravo");
        assert_eq!(store.read("b", &new_identity2).unwrap(), b"bravo");
    }

    #[test]
    fn single_recipient_still_works() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (identity, recipient) = test_keypair();

        let content = b"single recipient test";
        store
            .store(
                "single",
                BlobCategory::Credential,
                test_metadata("Single", content.len() as u64),
                content,
                &[&recipient],
            )
            .unwrap();

        let plaintext = store.read("single", &identity).unwrap();
        assert_eq!(plaintext, content);
    }

    #[test]
    fn store_with_no_recipients_errors() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();

        let result = store.store(
            "no-recip",
            BlobCategory::Medical,
            test_metadata("No Recip", 1),
            b"x",
            &[],
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            VaultStoreError::NoRecipients => {}
            other => panic!("expected NoRecipients, got: {other}"),
        }
    }

    #[test]
    fn rotate_key_with_no_recipients_errors() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (old_identity, _old_recipient) = test_keypair();

        let result = store.rotate_key(&old_identity, &[]);
        assert!(result.is_err());
        match result.unwrap_err() {
            VaultStoreError::NoRecipients => {}
            other => panic!("expected NoRecipients, got: {other}"),
        }
    }

    #[test]
    fn add_recipient_with_no_recipients_errors() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let (identity, recipient) = test_keypair();

        store
            .store(
                "test",
                BlobCategory::Medical,
                test_metadata("Test", 1),
                b"x",
                &[&recipient],
            )
            .unwrap();

        let result = store.add_recipient("test", &identity, &[]);
        assert!(result.is_err());
        match result.unwrap_err() {
            VaultStoreError::NoRecipients => {}
            other => panic!("expected NoRecipients, got: {other}"),
        }
    }
}
