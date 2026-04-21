//! Mobile FFI bridge for the Symbiotic Flutter app.
//!
//! This crate is the single Rust dependency for the mobile app. It re-exports
//! all platform-agnostic client logic from [`symbiotic_client`] and adds
//! mobile-specific functionality (escrow crypto).
//!
//! # Architecture
//!
//! ```text
//! Flutter app (Dart)
//!   └── flutter_rust_bridge (generated bindings)
//!         └── symbiotic-mobile-core (this crate)
//!               ├── escrow         (Argon2id + XChaCha20-Poly1305 escrow blobs)
//!               └── symbiotic-client (re-exported)
//!                     ├── parser   (parse sym.e events)
//!                     └── commands (build sym.c commands)
//! ```
//!
//! # FFI Surface
//!
//! `flutter_rust_bridge` generates Dart bindings from the public API of this
//! crate. The Dart side calls generated functions — no manual `extern "C"` or
//! MethodChannel wiring needed.

pub mod api;
pub mod escrow;
mod frb_generated;

// Re-export all client types so the app only depends on this one crate.
pub use symbiotic_client::commands;
pub use symbiotic_client::parser;
pub use symbiotic_client::{CommandPayload, EventPayload, Kind, Status};

// ── Escrow high-level API (for flutter_rust_bridge) ─────────────────

use std::path::Path;

/// Result of an escrow operation (internal; FRB uses `api::symbiotic::InternalEscrowResult`).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct InternalEscrowResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Create an escrow blob from the device's identity key.
///
/// Reads `{data_dir}/identity.key` (or generates 32 random bytes if missing),
/// encrypts with the passphrase, and writes to `{data_dir}/escrow.blob`.
pub(crate) fn escrow_create(passphrase: &str, data_dir: &str) -> InternalEscrowResult {
    let identity_path = Path::new(data_dir).join("identity.key");

    let secret_data = if identity_path.exists() {
        match std::fs::read(&identity_path) {
            Ok(data) => data,
            Err(e) => {
                return InternalEscrowResult {
                    ok: false,
                    blob_path: None,
                    error: Some(format!("failed to read identity key: {e}")),
                }
            }
        }
    } else {
        // Generate placeholder secret data (32 random bytes).
        let mut data = vec![0u8; 32];
        use rand::Rng;
        rand::rng().fill_bytes(&mut data);

        if let Err(e) = std::fs::create_dir_all(data_dir) {
            return InternalEscrowResult {
                ok: false,
                blob_path: None,
                error: Some(format!("failed to create data dir: {e}")),
            };
        }
        if let Err(e) = std::fs::write(&identity_path, &data) {
            return InternalEscrowResult {
                ok: false,
                blob_path: None,
                error: Some(format!("failed to write identity key: {e}")),
            };
        }
        data
    };

    match escrow::create_escrow_file(data_dir, &secret_data, passphrase) {
        Ok(blob_path) => InternalEscrowResult {
            ok: true,
            blob_path: Some(blob_path),
            error: None,
        },
        Err(e) => InternalEscrowResult {
            ok: false,
            blob_path: None,
            error: Some(e.to_string()),
        },
    }
}

/// Recover identity key from an escrow blob.
///
/// Reads `{data_dir}/escrow.blob`, decrypts with the passphrase, and writes
/// the recovered key to `{data_dir}/identity.key`.
pub(crate) fn escrow_recover(passphrase: &str, data_dir: &str) -> InternalEscrowResult {
    match escrow::recover_escrow_file(data_dir, passphrase) {
        Ok(secret_data) => {
            let identity_path = Path::new(data_dir).join("identity.key");
            if let Err(e) = std::fs::write(&identity_path, &secret_data) {
                return InternalEscrowResult {
                    ok: false,
                    blob_path: None,
                    error: Some(format!("failed to write recovered key: {e}")),
                };
            }
            InternalEscrowResult {
                ok: true,
                blob_path: None,
                error: None,
            }
        }
        Err(e) => InternalEscrowResult {
            ok: false,
            blob_path: None,
            error: Some(e.to_string()),
        },
    }
}

/// Check whether an escrow blob exists at `{data_dir}/escrow.blob`.
#[allow(dead_code)]
pub(crate) fn escrow_exists(data_dir: &str) -> bool {
    escrow::escrow_exists(data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_recover_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        // Create.
        let result = escrow_create("correct horse battery staple", data_dir);
        assert!(result.ok, "create failed: {:?}", result.error);
        assert!(result.blob_path.is_some());
        assert!(escrow_exists(data_dir));

        // Recover.
        let result = escrow_recover("correct horse battery staple", data_dir);
        assert!(result.ok, "recover failed: {:?}", result.error);
    }

    #[test]
    fn wrong_passphrase_fails() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        escrow_create("correct horse battery staple", data_dir);
        let result = escrow_recover("wrong passphrase here!", data_dir);
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("decryption failed"));
    }

    #[test]
    fn weak_passphrase_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        let result = escrow_create("weak", data_dir);
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("passphrase too weak"));
    }

    #[test]
    fn recover_without_blob_fails() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        let result = escrow_recover("correct horse battery staple", data_dir);
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("I/O error"));
    }

    #[test]
    fn exists_returns_false_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!escrow_exists(dir.path().to_str().unwrap()));
    }

    // Verify re-exports work.
    #[test]
    fn client_reexports_accessible() {
        // Protocol types from symbiotic-core via symbiotic-client.
        let _kind = Kind::Message;
        let _status = Status::Working;

        // Command builder.
        let envelope = commands::goal_deliberate("test");
        assert!(envelope.contains("goal.deliberate"));

        // Event parser.
        let json = serde_json::json!({
            "msgtype": "sym.e",
            "body": "Hello",
            "sym": {"v": 2, "k": 0, "s": 1, "ts": 100}
        });
        let parsed = parser::parse_event(&json, 0).unwrap();
        assert_eq!(parsed.kind, Kind::Message);
    }
}
