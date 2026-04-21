//! Trait + real impl for posting text to matrix rooms from deep daemon helpers.
//!
//! `MatrixPoster` lets call sites like `mirror_push_with_approval` enqueue
//! matrix messages without borrowing the `Box<dyn MatrixTransport>` that lives
//! in `main.rs`. The production `DaemonMatrixPoster` impl sends
//! `(room_id, envelope)` items through an mpsc channel; the pump loop in
//! `main.rs` drains the receiver and calls `transport.send_outgoing` for each
//! item. See T126 §08.b for the decoupling rationale.
//!
//! T128 §16a extension: in addition to the legacy fire-and-forget tuple
//! channel (kept untouched for backward compat with the ~6 existing call
//! sites in T126/T130/T132), `DaemonMatrixPoster` may optionally hold a
//! second `OutboundMatrixMessage` channel. New callers (archeology
//! notifications in §16b, future T42/T126 reply chains) use that channel to
//! capture the server-assigned `event_id` and to attach Matrix-spec
//! `m.relates_to` thread metadata. The pump loop in `main.rs` drains the
//! awaited channel separately and ships each result back through a
//! `oneshot::Sender<Result<EventId>>`.

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use symbiotic_core::protocol::{Kind, Status};
use symbiotic_matrix::events::MatrixEventEnvelope;

/// Server-assigned Matrix event identifier returned by `room.send`.
///
/// Re-exported / aliased here so trait methods can read clearly without
/// every caller reaching into `symbiotic_matrix`. The Matrix wire format is
/// always a string (e.g. `$abc123:matrix.org`); we keep it as a type alias
/// rather than a newtype so callers can interpolate it directly.
pub type EventId = String;

/// Item sent through the awaited matrix outbound channel by
/// `DaemonMatrixPoster` when a caller needs the server-assigned event_id
/// (or wants to attach `m.relates_to` thread metadata that depends on a
/// previously captured event_id).
///
/// Distinct from `crate::MatrixOutboundMessage` (the legacy tuple channel)
/// because the response oneshot is what makes capture possible at all —
/// adding it to the legacy tuple would have forced a breaking change on
/// every existing fire-and-forget call site.
pub struct OutboundMatrixMessage {
    pub room_id: String,
    pub envelope: MatrixEventEnvelope,
    /// If `Some`, the pump loop ships back the result of
    /// `transport.send_outgoing(...)` (mapped to `Result<EventId>`) so the
    /// caller can `.await` it. If `None`, fire-and-forget.
    pub response: Option<oneshot::Sender<Result<EventId>>>,
}

/// Sender half of the awaited matrix outbound channel. Held by
/// `DaemonMatrixPoster` instances that were constructed with
/// [`DaemonMatrixPoster::with_awaited`] so the new `*_returning_id` and
/// `post_threaded_with_source` methods can ship a `oneshot` response.
pub type AwaitedMatrixOutboundSender = UnboundedSender<OutboundMatrixMessage>;

/// Receiver half of the awaited matrix outbound channel. Created and held
/// in `main.rs` so the pump loop can drain it alongside the legacy
/// fire-and-forget tuple channel.
pub type AwaitedMatrixOutboundReceiver =
    tokio::sync::mpsc::UnboundedReceiver<OutboundMatrixMessage>;

/// Thin trait for posting matrix messages from deep daemon helpers (like
/// `mirror_push_with_approval`) without forcing them to take the
/// `MatrixTransport` generic. The production `DaemonMatrixPoster` impl wraps
/// the daemon's outbound mpsc sender; the test suite substitutes a mock.
///
/// T128 §16a additive extension: three new methods support source tagging,
/// event_id capture, and threaded replies. Existing `post_text` keeps its
/// fire-and-forget signature — additive trait methods carry default impls
/// so the ~6 existing implementors (mocks in T126/T128/T130/T132 plus
/// `DaemonMatrixPoster`) compile unchanged.
///
/// Default impls:
/// - `post_text_with_source` falls back to `post_text` (drops the source
///   tag) — fine for mocks that don't inspect it.
/// - `*_returning_id` and `post_threaded_with_source` return an
///   explanatory error so a caller wiring archeology notifications
///   accidentally to a fire-and-forget mock fails loudly rather than
///   silently dropping the reply chain.
#[async_trait]
pub trait MatrixPoster: Send + Sync {
    /// Post a plain-text body to a matrix room. Envelope metadata is filled in
    /// by the impl (it knows the daemon's sender identity). Source tag is
    /// inherited from the impl (`DaemonMatrixPoster` uses `scheduler`).
    async fn post_text(&self, room_id: &str, body: &str, now: u64) -> Result<()>;

    /// Fire-and-forget post with caller-supplied source tag. Use this when
    /// the message origin needs UI/mobile filterability distinct from
    /// scheduler-originated text (e.g. `archeology`, `mirror`).
    ///
    /// Default impl forwards to `post_text` and discards the source tag —
    /// preserves backward compat for existing test mocks. Production
    /// `DaemonMatrixPoster` overrides this with a source-aware envelope
    /// builder.
    async fn post_text_with_source(
        &self,
        room_id: &str,
        body: &str,
        _source: &str,
        now: u64,
    ) -> Result<()> {
        self.post_text(room_id, body, now).await
    }

    /// Post + capture server-assigned event_id. Required for thread-parenting
    /// subsequent replies. Uses the supplied source tag.
    ///
    /// Default impl returns an error — a caller that wired this to a
    /// fire-and-forget mock would otherwise silently lose the event_id
    /// the reply chain depends on.
    async fn post_text_with_source_returning_id(
        &self,
        _room_id: &str,
        _body: &str,
        _source: &str,
        _now: u64,
    ) -> Result<EventId> {
        Err(anyhow::anyhow!(
            "MatrixPoster impl does not support `post_text_with_source_returning_id` — \
             use `DaemonMatrixPoster::with_awaited(...)` for the production wiring"
        ))
    }

    /// Post a threaded reply carrying `m.relates_to` to the parent. Returns
    /// the new reply's event_id (so consumers can chain further replies if
    /// they want, though MVP only chains 1 level deep).
    ///
    /// Default impl returns an error — same rationale as
    /// `post_text_with_source_returning_id`.
    async fn post_threaded_with_source(
        &self,
        _room_id: &str,
        _body: &str,
        _source: &str,
        _parent_event_id: &EventId,
        _now: u64,
    ) -> Result<EventId> {
        Err(anyhow::anyhow!(
            "MatrixPoster impl does not support `post_threaded_with_source` — \
             use `DaemonMatrixPoster::with_awaited(...)` for the production wiring"
        ))
    }
}

/// Real matrix poster. Sends `(room_id, envelope)` into an mpsc channel;
/// the pump loop in `main.rs` drains it and calls `transport.send_outgoing`.
///
/// This decoupling exists because `MatrixTransport` is owned by `main.rs` as
/// `Box<dyn MatrixTransport>` (non-Arc, non-cloneable). Background tasks like
/// the repo scheduler (§08.c) cannot borrow the transport; the mpsc channel
/// is the only decoupled delivery path.
///
/// `awaited_sender` is the second (response-bearing) channel — `None` when
/// the poster was built with the legacy [`DaemonMatrixPoster::new`]
/// constructor, `Some` when built with [`DaemonMatrixPoster::with_awaited`].
/// Calling the new `*_returning_id` / `post_threaded_with_source` methods on
/// a poster without `awaited_sender` returns an error rather than silently
/// downgrading, so misconfiguration is loud.
pub struct DaemonMatrixPoster {
    sender: UnboundedSender<(String, MatrixEventEnvelope)>,
    awaited_sender: Option<AwaitedMatrixOutboundSender>,
}

impl DaemonMatrixPoster {
    /// Legacy constructor — fire-and-forget only. New `*_returning_id` and
    /// `post_threaded_with_source` calls on a poster built this way return
    /// an error explaining that the awaited channel is not wired.
    ///
    /// Kept for backward compat with `repo_scheduler.rs` (the only
    /// existing call site as of §16a), which only ever uses fire-and-forget
    /// posts.
    pub fn new(sender: UnboundedSender<(String, MatrixEventEnvelope)>) -> Self {
        Self {
            sender,
            awaited_sender: None,
        }
    }

    /// New constructor — supports both fire-and-forget (legacy tuple
    /// channel) and event_id-capturing / threaded posts (awaited channel).
    /// Used by `main.rs` when wiring archeology notifications (§16b) and
    /// any future call site that needs the new methods.
    pub fn with_awaited(
        sender: UnboundedSender<(String, MatrixEventEnvelope)>,
        awaited_sender: AwaitedMatrixOutboundSender,
    ) -> Self {
        Self {
            sender,
            awaited_sender: Some(awaited_sender),
        }
    }

    fn awaited(&self) -> Result<&AwaitedMatrixOutboundSender> {
        self.awaited_sender.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "DaemonMatrixPoster: awaited matrix channel not wired — \
                 construct with `with_awaited(...)` to use \
                 `*_returning_id` / `post_threaded_with_source`"
            )
        })
    }
}

#[async_trait]
impl MatrixPoster for DaemonMatrixPoster {
    async fn post_text(&self, room_id: &str, body: &str, now: u64) -> Result<()> {
        let envelope = build_text_envelope(body, now);
        self.sender
            .send((room_id.to_string(), envelope))
            .map_err(|e| anyhow::anyhow!("matrix outbound channel closed: {e}"))
    }

    async fn post_text_with_source(
        &self,
        room_id: &str,
        body: &str,
        source: &str,
        now: u64,
    ) -> Result<()> {
        let envelope = build_text_envelope_with_source(body, source, now);
        self.sender
            .send((room_id.to_string(), envelope))
            .map_err(|e| anyhow::anyhow!("matrix outbound channel closed: {e}"))
    }

    async fn post_text_with_source_returning_id(
        &self,
        room_id: &str,
        body: &str,
        source: &str,
        now: u64,
    ) -> Result<EventId> {
        let envelope = build_text_envelope_with_source(body, source, now);
        let (tx, rx) = oneshot::channel::<Result<EventId>>();
        let item = OutboundMatrixMessage {
            room_id: room_id.to_string(),
            envelope,
            response: Some(tx),
        };
        self.awaited()?
            .send(item)
            .map_err(|e| anyhow::anyhow!("awaited matrix channel closed: {e}"))?;
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "awaited matrix response oneshot dropped \
                 (pump loop may have crashed or is shutting down)"
            )),
        }
    }

    async fn post_threaded_with_source(
        &self,
        room_id: &str,
        body: &str,
        source: &str,
        parent_event_id: &EventId,
        now: u64,
    ) -> Result<EventId> {
        let envelope = build_thread_reply_envelope(body, source, parent_event_id, now);
        let (tx, rx) = oneshot::channel::<Result<EventId>>();
        let item = OutboundMatrixMessage {
            room_id: room_id.to_string(),
            envelope,
            response: Some(tx),
        };
        self.awaited()?
            .send(item)
            .map_err(|e| anyhow::anyhow!("awaited matrix channel closed: {e}"))?;
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "awaited matrix response oneshot dropped \
                 (pump loop may have crashed or is shutting down)"
            )),
        }
    }
}

/// Minimal plain-text envelope builder for scheduler-originated messages.
/// Uses `Kind::Message` + `Status::Success` — scheduler messages are
/// informational notifications, not goal-state transitions. The
/// `source=scheduler` detail tag lets downstream routers distinguish
/// scheduler-originated text from operator/agent-originated text.
pub(crate) fn build_text_envelope(body: &str, now: u64) -> MatrixEventEnvelope {
    build_text_envelope_with_source(body, "scheduler", now)
}

/// Same shape as [`build_text_envelope`] but with a caller-supplied source
/// tag. Used by all `*_with_source*` post methods so the source field is
/// driven by the caller rather than the impl.
pub(crate) fn build_text_envelope_with_source(
    body: &str,
    source: &str,
    now: u64,
) -> MatrixEventEnvelope {
    MatrixEventEnvelope::new(Kind::Message, Status::Success, now, body)
        .with_detail_field("source", source)
}

/// Build a threaded reply envelope carrying the Matrix-spec `m.relates_to`
/// payload (MSC3440/3771). The `is_falling_back: true` + nested
/// `m.in_reply_to` is the spec-recommended fallback so older clients without
/// thread support still render the reply as a regular reply.
pub(crate) fn build_thread_reply_envelope(
    body: &str,
    source: &str,
    parent_event_id: &EventId,
    now: u64,
) -> MatrixEventEnvelope {
    build_text_envelope_with_source(body, source, now).with_thread_parent(parent_event_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test 1 (backward compat) — existing `post_text` still works.
    /// Channel receives `(room, envelope)` tuple; method returns `Ok(())`
    /// immediately without awaiting any response.
    #[tokio::test]
    async fn daemon_matrix_poster_sends_via_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let poster = DaemonMatrixPoster::new(tx);
        poster
            .post_text("!room:matrix.org", "test body", 42)
            .await
            .expect("post_text should succeed");
        let (room_id, envelope) = rx.recv().await.expect("channel should yield one item");
        assert_eq!(room_id, "!room:matrix.org");
        let rendered = format!("{envelope:?}");
        assert!(
            rendered.contains("test body"),
            "body missing from envelope: {rendered}"
        );
    }

    #[tokio::test]
    async fn daemon_matrix_poster_post_returns_error_when_channel_closed() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let poster = DaemonMatrixPoster::new(tx);
        drop(rx);
        let result = poster.post_text("!room", "body", 0).await;
        assert!(
            result.is_err(),
            "expected Err when receiver is dropped, got Ok"
        );
    }

    #[test]
    fn build_text_envelope_carries_body_and_scheduler_source_tag() {
        let env = build_text_envelope("hello world", 1234);
        let rendered = format!("{env:?}");
        assert!(
            rendered.contains("hello world"),
            "body missing from envelope: {rendered}"
        );
        assert!(
            rendered.contains("scheduler"),
            "source tag missing: {rendered}"
        );
    }

    /// Test 2 — `post_text_with_source` puts the caller's source tag into
    /// the envelope's detail field (verified via `sym.d.source`).
    #[tokio::test]
    async fn post_text_with_source_carries_custom_source_tag() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let poster = DaemonMatrixPoster::new(tx);
        poster
            .post_text_with_source("!room:matrix.org", "hello", "archeology", 100)
            .await
            .expect("post_text_with_source should succeed");
        let (room_id, envelope) = rx.recv().await.expect("channel yielded one item");
        assert_eq!(room_id, "!room:matrix.org");
        let detail = envelope
            .sym
            .d
            .as_ref()
            .expect("envelope detail should be populated");
        let source = detail
            .get("source")
            .and_then(|v| v.as_str())
            .expect("source field should be present");
        assert_eq!(source, "archeology");
        // Critically: NOT the default `scheduler`.
        assert_ne!(source, "scheduler");
    }

    /// Test 3 — `post_text_with_source_returning_id` sends with
    /// `response: Some(...)` and returns the event_id that the (mock) pump
    /// loop ships back through the oneshot.
    #[tokio::test]
    async fn post_text_with_source_returning_id_returns_event_id_from_response() {
        let (tuple_tx, _tuple_rx) = tokio::sync::mpsc::unbounded_channel();
        let (awaited_tx, mut awaited_rx) = tokio::sync::mpsc::unbounded_channel();
        let poster = DaemonMatrixPoster::with_awaited(tuple_tx, awaited_tx);

        // Spawn a tiny "pump loop" that drains the awaited channel and
        // ships back a synthetic event_id for each item.
        let pump = tokio::spawn(async move {
            while let Some(item) = awaited_rx.recv().await {
                if let Some(response) = item.response {
                    let _ = response.send(Ok(format!("$evt-{}", item.envelope.sym.ts)));
                }
            }
        });

        let event_id = poster
            .post_text_with_source_returning_id("!room:matrix.org", "with id", "archeology", 12345)
            .await
            .expect("post_text_with_source_returning_id should succeed");

        assert_eq!(event_id, "$evt-12345");
        drop(pump);
    }

    /// Test 4 — `post_threaded_with_source` builds an envelope with
    /// `m.relates_to.rel_type = "m.thread"`, the parent event_id, and
    /// `is_falling_back: true` + nested `m.in_reply_to`.
    #[tokio::test]
    async fn post_threaded_with_source_sets_thread_relation_field() {
        let (tuple_tx, _tuple_rx) = tokio::sync::mpsc::unbounded_channel();
        let (awaited_tx, mut awaited_rx) = tokio::sync::mpsc::unbounded_channel();
        let poster = DaemonMatrixPoster::with_awaited(tuple_tx, awaited_tx);
        let parent: EventId = "$parent-evt:matrix.org".to_string();

        let pump = tokio::spawn(async move {
            // We only inspect the first item.
            if let Some(item) = awaited_rx.recv().await {
                let detail = item
                    .envelope
                    .sym
                    .d
                    .as_ref()
                    .expect("envelope detail should carry m.relates_to");
                let relates = detail
                    .get("m.relates_to")
                    .expect("m.relates_to field present");
                assert_eq!(
                    relates.get("rel_type").and_then(|v| v.as_str()),
                    Some("m.thread"),
                    "rel_type should be m.thread"
                );
                assert_eq!(
                    relates.get("event_id").and_then(|v| v.as_str()),
                    Some("$parent-evt:matrix.org"),
                    "thread parent event_id should match"
                );
                assert_eq!(
                    relates.get("is_falling_back").and_then(|v| v.as_bool()),
                    Some(true),
                    "is_falling_back should be true (spec fallback)"
                );
                let in_reply_to = relates
                    .get("m.in_reply_to")
                    .expect("nested m.in_reply_to present (spec fallback)");
                assert_eq!(
                    in_reply_to.get("event_id").and_then(|v| v.as_str()),
                    Some("$parent-evt:matrix.org"),
                );
                if let Some(response) = item.response {
                    let _ = response.send(Ok("$reply-evt".to_string()));
                }
            }
        });

        let reply_id = poster
            .post_threaded_with_source(
                "!room:matrix.org",
                "threaded reply",
                "archeology",
                &parent,
                999,
            )
            .await
            .expect("post_threaded_with_source should succeed");

        assert_eq!(reply_id, "$reply-evt");
        // Allow the pump to finish so its assertions run before test exit.
        let _ = pump.await;
    }

    /// Test 5 — channel-closed-mid-await: if the response oneshot is
    /// dropped (pump crashed / shutdown), the awaiting method returns
    /// `Err` cleanly rather than hanging.
    #[tokio::test]
    async fn post_text_with_source_returning_id_errors_when_response_dropped() {
        let (tuple_tx, _tuple_rx) = tokio::sync::mpsc::unbounded_channel();
        let (awaited_tx, mut awaited_rx) = tokio::sync::mpsc::unbounded_channel();
        let poster = DaemonMatrixPoster::with_awaited(tuple_tx, awaited_tx);

        // Pump that explicitly drops the response oneshot without sending.
        let pump = tokio::spawn(async move {
            if let Some(item) = awaited_rx.recv().await {
                drop(item.response);
            }
        });

        let result = poster
            .post_text_with_source_returning_id("!room", "body", "archeology", 0)
            .await;

        assert!(
            result.is_err(),
            "expected Err when response oneshot is dropped, got Ok({result:?})"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("oneshot dropped"),
            "error should mention oneshot drop, got: {err_msg}"
        );
        let _ = pump.await;
    }

    /// Defensive: calling `*_returning_id` on a poster that was constructed
    /// without the awaited channel returns an explanatory error.
    #[tokio::test]
    async fn returning_id_without_awaited_channel_errors() {
        let (tuple_tx, _tuple_rx) = tokio::sync::mpsc::unbounded_channel();
        let poster = DaemonMatrixPoster::new(tuple_tx);
        let result = poster
            .post_text_with_source_returning_id("!room", "body", "archeology", 0)
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("awaited matrix channel not wired"));
    }
}
