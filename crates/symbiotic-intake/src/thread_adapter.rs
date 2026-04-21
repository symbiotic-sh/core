//! Thread Conversation Adapter — converts thread messages into a [`RawInput`]
//! for the Distillery pipeline.
//!
//! This is the core of T109 Phase 1: feeding conversation threads through the
//! existing 5-stage Distillery pipeline (Reduce -> Reflect -> Verify -> Reweave
//! -> Archive) to extract structured knowledge from natural conversations.

use crate::distillery::RawInput;

/// Adapter that converts thread conversation messages into a [`RawInput`]
/// suitable for the Distillery pipeline.
///
/// Thread messages are formatted as a Markdown conversation with speaker
/// labels and timestamps, preserving the conversational context that the
/// Distillery's Reduce stage needs to extract atomic claims.
pub struct ThreadConversationAdapter;

impl ThreadConversationAdapter {
    /// Convert thread messages to a [`RawInput`] for the Distillery.
    ///
    /// Messages are formatted as a Markdown conversation:
    /// ```text
    /// # Thread: {title}
    ///
    /// **User** (2026-03-16 14:30): message body
    /// **Symbiotic** (2026-03-16 14:31): response body
    /// ```
    ///
    /// The `source_url` is set to `intake:thread-{thread_id}` so the Distillery
    /// can track provenance back to the originating conversation.
    ///
    /// # Arguments
    ///
    /// * `thread_id` - Unique thread identifier (e.g. `"thread-abc123"`)
    /// * `thread_title` - Human-readable thread title
    /// * `messages` - Tuples of `(sender, body, timestamp)` in chronological order
    pub fn messages_to_raw_input(
        thread_id: &str,
        thread_title: &str,
        messages: &[(String, String, String)],
    ) -> RawInput {
        let mut content = format!("# Thread: {thread_title}\n\n");

        for (sender, body, timestamp) in messages {
            content.push_str(&format!("**{sender}** ({timestamp}): {body}\n\n"));
        }

        RawInput {
            source_url: format!("intake:thread-{thread_id}"),
            raw_content: content,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_messages_produces_header_only() {
        let input =
            ThreadConversationAdapter::messages_to_raw_input("thread-abc", "Test Thread", &[]);

        assert_eq!(input.source_url, "intake:thread-thread-abc");
        assert_eq!(input.raw_content, "# Thread: Test Thread\n\n");
    }

    #[test]
    fn single_message_formatted_correctly() {
        let messages = vec![(
            "User".to_string(),
            "Hello world".to_string(),
            "2026-03-16 14:30".to_string(),
        )];

        let input =
            ThreadConversationAdapter::messages_to_raw_input("thread-hello", "Greeting", &messages);

        assert_eq!(input.source_url, "intake:thread-thread-hello");
        assert!(input.raw_content.contains("# Thread: Greeting"));
        assert!(input
            .raw_content
            .contains("**User** (2026-03-16 14:30): Hello world"));
    }

    #[test]
    fn multiple_messages_in_order() {
        let messages = vec![
            (
                "User".to_string(),
                "What is Rust?".to_string(),
                "2026-03-16 14:30".to_string(),
            ),
            (
                "Symbiotic".to_string(),
                "Rust is a systems programming language.".to_string(),
                "2026-03-16 14:31".to_string(),
            ),
            (
                "User".to_string(),
                "Tell me about ownership.".to_string(),
                "2026-03-16 14:32".to_string(),
            ),
        ];

        let input = ThreadConversationAdapter::messages_to_raw_input(
            "thread-rust",
            "Learning Rust",
            &messages,
        );

        assert_eq!(input.source_url, "intake:thread-thread-rust");
        assert!(input.raw_content.starts_with("# Thread: Learning Rust\n\n"));

        // Verify all messages are present
        assert!(input
            .raw_content
            .contains("**User** (2026-03-16 14:30): What is Rust?"));
        assert!(input
            .raw_content
            .contains("**Symbiotic** (2026-03-16 14:31): Rust is a systems programming language."));
        assert!(input
            .raw_content
            .contains("**User** (2026-03-16 14:32): Tell me about ownership."));

        // Verify order: "What is Rust?" appears before "Tell me about ownership."
        let pos_first = input.raw_content.find("What is Rust?").unwrap();
        let pos_last = input.raw_content.find("Tell me about ownership.").unwrap();
        assert!(pos_first < pos_last);
    }

    #[test]
    fn source_url_includes_thread_id() {
        let input =
            ThreadConversationAdapter::messages_to_raw_input("thread-xyz-789", "Some Title", &[]);
        assert_eq!(input.source_url, "intake:thread-thread-xyz-789");
    }

    #[test]
    fn multiline_message_body_preserved() {
        let messages = vec![(
            "User".to_string(),
            "Line one\nLine two\nLine three".to_string(),
            "2026-03-16 10:00".to_string(),
        )];

        let input = ThreadConversationAdapter::messages_to_raw_input(
            "thread-multi",
            "Multiline",
            &messages,
        );

        assert!(input.raw_content.contains("Line one\nLine two\nLine three"));
    }

    #[test]
    fn special_characters_in_title_preserved() {
        let input = ThreadConversationAdapter::messages_to_raw_input(
            "thread-special",
            "Thread with **bold** & <html>",
            &[],
        );

        assert!(input
            .raw_content
            .contains("# Thread: Thread with **bold** & <html>"));
    }
}
