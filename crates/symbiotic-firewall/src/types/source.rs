//! Source-receipt + content-source types (design §6.4.1).
//!
//! The **source receipt store** is a separate immutable blob-store of original
//! bytes, referenced by [`SourceReceiptRef`]. A [`ContentSource`] carries the
//! provenance of an ingested payload (URL, fetch time, content-type, headers),
//! while [`CaptureCompleteness`] records whether the preserved raw bytes
//! represent the full source or only a partial capture (some APIs — Gmail,
//! Graph — never expose true raw bytes).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use time::OffsetDateTime;

/// Content-addressed pointer to an immutable blob in the source receipt store.
///
/// Lives at `knowledge-base/self/source_receipts/{receipt_id}.blob` with a
/// sibling `.meta.json`. The receipt id is the SHA-256 (hex) of the raw
/// payload bytes; collisions are impossible within a single Archive.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceReceiptRef {
    /// SHA-256 hex of the raw source bytes. Used as the filename stem.
    pub receipt_id: String,
    /// Size of the preserved blob, in bytes. `None` when metadata-only
    /// receipts are written (e.g. partial-source capture).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_length: Option<u64>,
    /// Whether the stored bytes represent the full source or a partial
    /// capture. See [`CaptureCompleteness`].
    pub completeness: CaptureCompleteness,
}

/// Whether the source receipt preserves the *full* original payload, or only
/// a *partial* capture (e.g. Gmail API returns a structured object, not raw
/// MIME — the best we can preserve is the API envelope).
///
/// Wire format uses a `type` discriminant so the partial case can carry a
/// free-form reason field without changing the variant name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CaptureCompleteness {
    /// The receipt bytes are the exact payload received from the source.
    /// Rebuild on this receipt reproduces the original ingest conditions.
    FullSourcePreserved,
    /// The receipt bytes are a best-effort capture that does not represent
    /// the true original. Rebuild must acknowledge the limitation.
    PartialSourcePreserved {
        /// Human-readable reason the capture is partial (e.g. "Gmail API
        /// returns structured objects, not raw .eml MIME").
        reason: String,
    },
}

/// Provenance of a content payload: where it came from, when it was fetched,
/// what the source declared it to be, and any transport-level metadata.
///
/// Stored alongside the receipt blob in `.meta.json` for Rebuild. Also
/// referenced by [`crate::types::ScanContext`] when a scan runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSource {
    /// Free-form source kind (e.g. `"web_fetch"`, `"tool_observation"`,
    /// `"gmail_api"`, `"swarm_artifact"`). Not an enum — the connector
    /// catalog is open-ended and lives outside this crate.
    pub kind: String,
    /// Canonical source URL or URI, if one exists. Missing for synthetic
    /// sources (e.g. tool observations that aren't addressable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// When the payload was fetched / observed.
    #[serde(with = "time::serde::rfc3339")]
    pub fetched_at: OffsetDateTime,
    /// Content-type the source *claimed* (e.g. `"text/html; charset=utf-8"`).
    /// Stage A (structural sanitization) validates this against the actual
    /// bytes to catch MIME confusion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_content_type: Option<String>,
    /// Transport headers captured at fetch time (HTTP headers for web
    /// fetches, etc.). Empty map for sources without transport headers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_completeness_full_round_trips() {
        let value = CaptureCompleteness::FullSourcePreserved;
        let json = serde_json::to_string(&value).expect("serialize");
        assert_eq!(json, r#"{"type":"FullSourcePreserved"}"#);
        let back: CaptureCompleteness = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn capture_completeness_partial_round_trips() {
        let value = CaptureCompleteness::PartialSourcePreserved {
            reason: "Gmail API returns structured objects, not raw .eml MIME".into(),
        };
        let json = serde_json::to_string(&value).expect("serialize");
        assert!(json.contains(r#""type":"PartialSourcePreserved""#));
        assert!(json.contains("Gmail API"));
        let back: CaptureCompleteness = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn source_receipt_ref_round_trips() {
        let value = SourceReceiptRef {
            receipt_id: "deadbeef".repeat(8),
            byte_length: Some(1234),
            completeness: CaptureCompleteness::FullSourcePreserved,
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: SourceReceiptRef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn content_source_round_trips_with_headers() {
        let fetched_at =
            OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp");
        let mut headers = BTreeMap::new();
        headers.insert("etag".into(), "abc123".into());
        let value = ContentSource {
            kind: "web_fetch".into(),
            url: Some("https://example.com/doc".into()),
            fetched_at,
            claimed_content_type: Some("text/html; charset=utf-8".into()),
            headers,
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: ContentSource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn content_source_round_trips_minimal() {
        let fetched_at =
            OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp");
        let value = ContentSource {
            kind: "tool_observation".into(),
            url: None,
            fetched_at,
            claimed_content_type: None,
            headers: BTreeMap::new(),
        };
        let json = serde_json::to_string(&value).expect("serialize");
        // Minimal form should omit the `None` / empty fields.
        assert!(!json.contains("\"url\""));
        assert!(!json.contains("\"claimed_content_type\""));
        assert!(!json.contains("\"headers\""));
        let back: ContentSource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }
}
