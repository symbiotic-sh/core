//! Integration tests for escrow + key rotation lifecycle.
//!
//! Tests cover:
//! - Escrow passphrase rotation lifecycle
//! - Multi-device escrow decrypt
//! - Wrong blob and missing blob error paths
//! - Key rotation with recipient changes
//! - Concurrent safety
//! - Round-trip consistency

use age::secrecy::ExposeSecret;
use age::x25519::Identity;
use symbiotic_vault_store::escrow::{
    change_passphrase, create_escrow, recover_escrow, recover_escrow_for_identity,
    verify_escrow_fingerprint, EscrowError, EscrowKdfParams,
};
use symbiotic_vault_store::keys::Recipient;
use symbiotic_vault_store::{BlobCategory, BlobMetadata, BlobStore, VaultStoreError};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Fast KDF params for tests (avoid 256 MiB allocation in CI).
fn test_params() -> EscrowKdfParams {
    EscrowKdfParams {
        memory_kib: 1024, // 1 MiB
        iterations: 1,
        parallelism: 1,
    }
}

fn test_keypair() -> (Identity, Recipient) {
    let identity = Identity::generate();
    let recipient = identity.to_public();
    (identity, recipient)
}

fn test_metadata(title: &str, size: u64) -> BlobMetadata {
    BlobMetadata {
        title: title.to_string(),
        tags: vec!["test".to_string()],
        size_bytes: size,
        content_type: "text/plain".to_string(),
    }
}

/// Helper to extract an error from `Result<Identity, EscrowError>` without
/// requiring `Identity: Debug` (which `age::x25519::Identity` does not implement).
fn expect_escrow_err(result: Result<Identity, EscrowError>) -> EscrowError {
    match result {
        Err(e) => e,
        Ok(_) => panic!("expected Err, got Ok"),
    }
}

const PASS_A: &str = "correct horse battery staple";
const PASS_B: &str = "my brand new passphrase here";

// ---------------------------------------------------------------------------
// 1. Escrow passphrase rotation — create with A, change to B, recover with B
// ---------------------------------------------------------------------------

#[test]
fn escrow_passphrase_rotation_lifecycle() {
    let identity = Identity::generate();
    let params = test_params();

    // Create escrow with passphrase A
    let blob_a = create_escrow(&identity, PASS_A, &params).unwrap();

    // Change passphrase from A to B
    let blob_b = change_passphrase(&blob_a, PASS_A, PASS_B, &params).unwrap();

    // Recover with passphrase B succeeds
    let recovered = recover_escrow(&blob_b, PASS_B, &params).unwrap();
    assert_eq!(
        identity.to_string().expose_secret(),
        recovered.to_string().expose_secret(),
    );
}

// ---------------------------------------------------------------------------
// 2. Old passphrase fails after rotation
// ---------------------------------------------------------------------------

#[test]
fn escrow_old_passphrase_fails_after_rotation() {
    let identity = Identity::generate();
    let params = test_params();

    let blob_a = create_escrow(&identity, PASS_A, &params).unwrap();
    let blob_b = change_passphrase(&blob_a, PASS_A, PASS_B, &params).unwrap();

    // Old passphrase on new blob should fail
    let result = recover_escrow(&blob_b, PASS_A, &params);
    match expect_escrow_err(result) {
        EscrowError::DecryptionFailed => {}
        other => panic!("expected DecryptionFailed, got: {other}"),
    }
}

// ---------------------------------------------------------------------------
// 3. Key rotation — encrypt to recipient1, rotate to recipient2
// ---------------------------------------------------------------------------

#[test]
fn key_rotation_old_key_fails_new_key_works() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
    let (old_identity, old_recipient) = test_keypair();
    let (new_identity, new_recipient) = test_keypair();

    let content = b"sensitive medical record";
    store
        .store(
            "rec-1",
            BlobCategory::Medical,
            test_metadata("Record 1", content.len() as u64),
            content,
            &[&old_recipient],
        )
        .unwrap();

    // Rotate keys
    let count = store.rotate_key(&old_identity, &[&new_recipient]).unwrap();
    assert_eq!(count, 1);

    // Old key fails
    let result = store.read("rec-1", &old_identity);
    assert!(result.is_err());
    match result.unwrap_err() {
        VaultStoreError::Decrypt(_) => {}
        other => panic!("expected Decrypt error, got: {other}"),
    }

    // New key succeeds
    let plaintext = store.read("rec-1", &new_identity).unwrap();
    assert_eq!(plaintext, content);
}

#[test]
fn key_rotation_to_multiple_recipients() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
    let (old_identity, old_recipient) = test_keypair();
    let (new_id1, new_rec1) = test_keypair();
    let (new_id2, new_rec2) = test_keypair();

    store
        .store(
            "a",
            BlobCategory::Medical,
            test_metadata("A", 5),
            b"alpha",
            &[&old_recipient],
        )
        .unwrap();
    store
        .store(
            "b",
            BlobCategory::Financial,
            test_metadata("B", 5),
            b"bravo",
            &[&old_recipient],
        )
        .unwrap();

    // Rotate to two new recipients
    let count = store
        .rotate_key(&old_identity, &[&new_rec1, &new_rec2])
        .unwrap();
    assert_eq!(count, 2);

    // Both new keys work for both blobs
    assert_eq!(store.read("a", &new_id1).unwrap(), b"alpha");
    assert_eq!(store.read("a", &new_id2).unwrap(), b"alpha");
    assert_eq!(store.read("b", &new_id1).unwrap(), b"bravo");
    assert_eq!(store.read("b", &new_id2).unwrap(), b"bravo");

    // Old key fails
    assert!(store.read("a", &old_identity).is_err());
    assert!(store.read("b", &old_identity).is_err());
}

// ---------------------------------------------------------------------------
// 4. Multi-device escrow — encrypt to [dev1, dev2], each can decrypt
// ---------------------------------------------------------------------------

#[test]
fn multi_device_each_can_decrypt() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
    let (id1, rec1) = test_keypair();
    let (id2, rec2) = test_keypair();

    let content = b"shared across two devices";
    store
        .store(
            "shared",
            BlobCategory::Credential,
            test_metadata("Shared Secret", content.len() as u64),
            content,
            &[&rec1, &rec2],
        )
        .unwrap();

    // Both devices can decrypt independently
    assert_eq!(store.read("shared", &id1).unwrap(), content);
    assert_eq!(store.read("shared", &id2).unwrap(), content);
}

#[test]
fn add_third_device_all_three_can_decrypt() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
    let (id1, rec1) = test_keypair();
    let (id2, rec2) = test_keypair();
    let (id3, rec3) = test_keypair();

    let content = b"evolving device access";
    store
        .store(
            "evolve",
            BlobCategory::Credential,
            test_metadata("Evolving", content.len() as u64),
            content,
            &[&rec1, &rec2],
        )
        .unwrap();

    // Device 3 cannot read yet
    assert!(store.read("evolve", &id3).is_err());

    // Add device 3 via add_recipient
    store
        .add_recipient("evolve", &id1, &[&rec1, &rec2, &rec3])
        .unwrap();

    // All three can now decrypt
    assert_eq!(store.read("evolve", &id1).unwrap(), content);
    assert_eq!(store.read("evolve", &id2).unwrap(), content);
    assert_eq!(store.read("evolve", &id3).unwrap(), content);
}

// ---------------------------------------------------------------------------
// 5. Wrong blob detection (V2 fingerprint)
// ---------------------------------------------------------------------------

#[test]
fn wrong_blob_detected_via_fingerprint() {
    let identity_a = Identity::generate();
    let identity_b = Identity::generate();
    let params = test_params();

    // Create escrow for identity_a
    let blob_a = create_escrow(&identity_a, PASS_A, &params).unwrap();

    // Verify fingerprint matches identity_a but not identity_b
    assert!(verify_escrow_fingerprint(&blob_a, &identity_a));
    assert!(!verify_escrow_fingerprint(&blob_a, &identity_b));

    // Try to recover blob_a expecting identity_b with wrong passphrase
    let result =
        recover_escrow_for_identity(&blob_a, "wrong passphrase here", &params, &identity_b);
    match expect_escrow_err(result) {
        EscrowError::WrongBlob => {}
        other => panic!("expected WrongBlob, got: {other}"),
    }
}

#[test]
fn wrong_passphrase_correct_blob_returns_decryption_failed() {
    let identity = Identity::generate();
    let params = test_params();

    let blob = create_escrow(&identity, PASS_A, &params).unwrap();

    // Wrong passphrase but matching identity hint => DecryptionFailed (not WrongBlob)
    let result = recover_escrow_for_identity(&blob, "wrong passphrase here", &params, &identity);
    match expect_escrow_err(result) {
        EscrowError::DecryptionFailed => {}
        other => panic!("expected DecryptionFailed, got: {other}"),
    }
}

// ---------------------------------------------------------------------------
// 6. V1 backward compatibility — create_escrow always produces V2 now,
//    but change_passphrase on V2 should produce V2, and we can verify
//    the format roundtrips correctly.
// ---------------------------------------------------------------------------

#[test]
fn change_passphrase_produces_v2_blob() {
    let identity = Identity::generate();
    let params = test_params();

    let blob = create_escrow(&identity, PASS_A, &params).unwrap();
    assert!(blob.starts_with(b"SYMESC2"));

    let new_blob = change_passphrase(&blob, PASS_A, PASS_B, &params).unwrap();
    assert!(new_blob.starts_with(b"SYMESC2"));

    // Fingerprint should still match original identity
    assert!(verify_escrow_fingerprint(&new_blob, &identity));

    let recovered = recover_escrow(&new_blob, PASS_B, &params).unwrap();
    assert_eq!(
        identity.to_string().expose_secret(),
        recovered.to_string().expose_secret(),
    );
}

// ---------------------------------------------------------------------------
// 7. Empty recipient list → error
// ---------------------------------------------------------------------------

#[test]
fn store_empty_recipients_errors() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();

    let result = store.store(
        "no-recip",
        BlobCategory::Medical,
        test_metadata("No Recip", 1),
        b"x",
        &[],
    );
    match result.unwrap_err() {
        VaultStoreError::NoRecipients => {}
        other => panic!("expected NoRecipients, got: {other}"),
    }
}

#[test]
fn rotate_key_empty_recipients_errors() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
    let (old_identity, _) = test_keypair();

    let result = store.rotate_key(&old_identity, &[]);
    match result.unwrap_err() {
        VaultStoreError::NoRecipients => {}
        other => panic!("expected NoRecipients, got: {other}"),
    }
}

#[test]
fn add_recipient_empty_recipients_errors() {
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
    match result.unwrap_err() {
        VaultStoreError::NoRecipients => {}
        other => panic!("expected NoRecipients, got: {other}"),
    }
}

// ---------------------------------------------------------------------------
// 8. Missing blob → NotFound error
// ---------------------------------------------------------------------------

#[test]
fn read_missing_blob_returns_not_found() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
    let (identity, _) = test_keypair();

    let result = store.read("nonexistent", &identity);
    match result.unwrap_err() {
        VaultStoreError::NotFound(id) => assert_eq!(id, "nonexistent"),
        other => panic!("expected NotFound, got: {other}"),
    }
}

#[test]
fn delete_missing_blob_returns_not_found() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();

    let result = store.delete("nonexistent");
    match result.unwrap_err() {
        VaultStoreError::NotFound(id) => assert_eq!(id, "nonexistent"),
        other => panic!("expected NotFound, got: {other}"),
    }
}

#[test]
fn add_recipient_to_missing_blob_returns_not_found() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
    let (identity, _) = test_keypair();
    let (_, recipient) = test_keypair();

    let result = store.add_recipient("nonexistent", &identity, &[&recipient]);
    match result.unwrap_err() {
        VaultStoreError::NotFound(id) => assert_eq!(id, "nonexistent"),
        other => panic!("expected NotFound, got: {other}"),
    }
}

#[test]
fn read_blob_with_missing_age_file_returns_file_missing() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
    let (identity, recipient) = test_keypair();

    store
        .store(
            "ghost",
            BlobCategory::Medical,
            test_metadata("Ghost", 1),
            b"x",
            &[&recipient],
        )
        .unwrap();

    // Manually remove the .age file to simulate corruption
    std::fs::remove_file(dir.path().join("ghost.age")).unwrap();

    let result = store.read("ghost", &identity);
    match result.unwrap_err() {
        VaultStoreError::FileMissing(id) => assert_eq!(id, "ghost"),
        other => panic!("expected FileMissing, got: {other}"),
    }
}

// ---------------------------------------------------------------------------
// 9. Concurrent rotation safety — multiple operations don't corrupt state
// ---------------------------------------------------------------------------

#[test]
fn sequential_rotations_preserve_data() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();

    let (id_gen0, rec_gen0) = test_keypair();
    let (id_gen1, rec_gen1) = test_keypair();
    let (id_gen2, rec_gen2) = test_keypair();

    // Store initial blobs
    store
        .store(
            "a",
            BlobCategory::Medical,
            test_metadata("A", 5),
            b"alpha",
            &[&rec_gen0],
        )
        .unwrap();
    store
        .store(
            "b",
            BlobCategory::Financial,
            test_metadata("B", 5),
            b"bravo",
            &[&rec_gen0],
        )
        .unwrap();
    store
        .store(
            "c",
            BlobCategory::Legal,
            test_metadata("C", 7),
            b"charlie",
            &[&rec_gen0],
        )
        .unwrap();

    // Rotation 1: gen0 → gen1
    let count = store.rotate_key(&id_gen0, &[&rec_gen1]).unwrap();
    assert_eq!(count, 3);

    // Rotation 2: gen1 → gen2
    let count = store.rotate_key(&id_gen1, &[&rec_gen2]).unwrap();
    assert_eq!(count, 3);

    // gen0 and gen1 should both fail
    assert!(store.read("a", &id_gen0).is_err());
    assert!(store.read("a", &id_gen1).is_err());

    // gen2 should work for all
    assert_eq!(store.read("a", &id_gen2).unwrap(), b"alpha");
    assert_eq!(store.read("b", &id_gen2).unwrap(), b"bravo");
    assert_eq!(store.read("c", &id_gen2).unwrap(), b"charlie");
}

#[test]
fn store_and_rotate_interleaved() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();

    let (id1, rec1) = test_keypair();
    let (id2, rec2) = test_keypair();

    // Store blob with key 1
    store
        .store(
            "blob-1",
            BlobCategory::Credential,
            test_metadata("Blob 1", 4),
            b"one!",
            &[&rec1],
        )
        .unwrap();

    // Rotate to key 2
    store.rotate_key(&id1, &[&rec2]).unwrap();

    // Store another blob with key 2 (post-rotation)
    store
        .store(
            "blob-2",
            BlobCategory::Credential,
            test_metadata("Blob 2", 4),
            b"two!",
            &[&rec2],
        )
        .unwrap();

    // Both blobs should be readable with key 2
    assert_eq!(store.read("blob-1", &id2).unwrap(), b"one!");
    assert_eq!(store.read("blob-2", &id2).unwrap(), b"two!");

    // Key 1 should fail for the rotated blob
    assert!(store.read("blob-1", &id1).is_err());
}

// ---------------------------------------------------------------------------
// 10. Round-trip consistency — create → store → retrieve → decrypt
// ---------------------------------------------------------------------------

#[test]
fn full_roundtrip_consistency() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
    let (identity, recipient) = test_keypair();

    let content = b"The quick brown fox jumps over the lazy dog. 0123456789!@#$%";
    let meta = test_metadata("Roundtrip Test", content.len() as u64);

    // Store
    let blob = store
        .store(
            "roundtrip",
            BlobCategory::Medical,
            meta.clone(),
            content,
            &[&recipient],
        )
        .unwrap();

    // Verify metadata
    assert_eq!(blob.id, "roundtrip");
    assert_eq!(blob.category, BlobCategory::Medical);
    assert_eq!(blob.metadata.title, "Roundtrip Test");
    assert_eq!(blob.metadata.size_bytes, content.len() as u64);

    // List and verify
    let listed = store.list(None).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "roundtrip");
    assert_eq!(listed[0].metadata, meta);

    // Decrypt and verify content
    let plaintext = store.read("roundtrip", &identity).unwrap();
    assert_eq!(plaintext, content);
}

#[test]
fn roundtrip_with_binary_content() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
    let (identity, recipient) = test_keypair();

    // Binary content with all byte values
    let content: Vec<u8> = (0..=255).collect();

    store
        .store(
            "binary",
            BlobCategory::Credential,
            test_metadata("Binary", content.len() as u64),
            &content,
            &[&recipient],
        )
        .unwrap();

    let plaintext = store.read("binary", &identity).unwrap();
    assert_eq!(plaintext, content);
}

#[test]
fn roundtrip_empty_content() {
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

// ---------------------------------------------------------------------------
// Escrow + BlobStore combined lifecycle
// ---------------------------------------------------------------------------

#[test]
fn escrow_and_blobstore_full_lifecycle() {
    // This test exercises the full lifecycle:
    // 1. Generate identity, create escrow backup
    // 2. Store encrypted blobs
    // 3. "Lose" the identity (drop it)
    // 4. Recover from escrow
    // 5. Read blobs with recovered identity

    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
    let params = test_params();

    // Step 1: Generate identity and create escrow
    let identity = Identity::generate();
    let recipient = identity.to_public();
    let escrow_blob = create_escrow(&identity, PASS_A, &params).unwrap();

    // Step 2: Store some blobs
    store
        .store(
            "medical-1",
            BlobCategory::Medical,
            test_metadata("Blood Test", 8),
            b"results!",
            &[&recipient],
        )
        .unwrap();
    store
        .store(
            "financial-1",
            BlobCategory::Financial,
            test_metadata("Tax Return", 7),
            b"numbers",
            &[&recipient],
        )
        .unwrap();

    // Step 3: "Lose" the identity — just use a fresh variable via recovery
    // (In real code, this simulates losing the key and recovering from backup)

    // Step 4: Recover from escrow
    let recovered_identity = recover_escrow(&escrow_blob, PASS_A, &params).unwrap();

    // Step 5: Read blobs with recovered identity
    assert_eq!(
        store.read("medical-1", &recovered_identity).unwrap(),
        b"results!"
    );
    assert_eq!(
        store.read("financial-1", &recovered_identity).unwrap(),
        b"numbers"
    );
}

#[test]
fn escrow_passphrase_change_then_recover_and_read_blobs() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
    let params = test_params();

    let identity = Identity::generate();
    let recipient = identity.to_public();

    // Create escrow and store blob
    let escrow_blob = create_escrow(&identity, PASS_A, &params).unwrap();
    store
        .store(
            "secret",
            BlobCategory::Credential,
            test_metadata("API Key", 10),
            b"sk-12345678",
            &[&recipient],
        )
        .unwrap();

    // Change escrow passphrase
    let new_escrow = change_passphrase(&escrow_blob, PASS_A, PASS_B, &params).unwrap();

    // Recover with new passphrase
    let recovered = recover_escrow(&new_escrow, PASS_B, &params).unwrap();

    // Blob is still readable
    assert_eq!(store.read("secret", &recovered).unwrap(), b"sk-12345678");

    // Old passphrase fails
    let result = recover_escrow(&new_escrow, PASS_A, &params);
    assert!(result.is_err());
}
