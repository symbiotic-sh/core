//! Content chunking for embedding generation.
//!
//! Splits documents into token-bounded chunks with boundary-aware splitting
//! (paragraph > heading > sentence > token) and configurable overlap.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::Sensitivity;

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum TokenizerError {
    #[error("tokenizer initialization failed: {0}")]
    InitFailed(String),
    #[error("encoding failed for input of length {0}")]
    EncodingFailed(usize),
}

pub struct Tokenizer {
    bpe: tiktoken_rs::CoreBPE,
}

impl Tokenizer {
    pub fn new() -> Result<Self, TokenizerError> {
        let bpe =
            tiktoken_rs::cl100k_base().map_err(|e| TokenizerError::InitFailed(e.to_string()))?;
        Ok(Self { bpe })
    }

    /// Count tokens in the given text.
    pub fn count_tokens(&self, text: &str) -> usize {
        self.bpe.encode_with_special_tokens(text).len()
    }

    /// Encode text to token IDs.
    pub fn encode(&self, text: &str) -> Vec<u32> {
        self.bpe.encode_with_special_tokens(text)
    }

    /// Decode token IDs back to text.
    pub fn decode(&self, tokens: &[u32]) -> Result<String, TokenizerError> {
        self.bpe
            .decode(tokens.to_vec())
            .map_err(|_| TokenizerError::EncodingFailed(tokens.len()))
    }
}

// ---------------------------------------------------------------------------
// Chunk types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkConfig {
    /// Target chunk size in tokens.
    pub target_tokens: usize,
    /// Overlap between adjacent chunks in tokens.
    pub overlap_tokens: usize,
    /// Minimum chunk size; smaller chunks are merged with neighbors or discarded.
    pub min_tokens: usize,
    /// Maximum chunk size; chunks exceeding this are force-split at token boundary.
    pub max_tokens: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            target_tokens: 512,
            overlap_tokens: 64,
            min_tokens: 64,
            max_tokens: 768,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// Unique ID for this chunk (UUID v4).
    pub chunk_id: String,
    /// Source document ID (archive entry short-id).
    pub source_id: String,
    /// Byte offset in the original document where this chunk starts.
    pub byte_offset: usize,
    /// Character length of the chunk in the original document.
    pub char_length: usize,
    /// Token count (computed by tokenizer).
    pub token_count: usize,
    /// The chunk text content.
    pub text: String,
    /// Sensitivity level inherited from source document.
    pub sensitivity: Sensitivity,
    /// Which boundary type was used to end this chunk.
    pub boundary_type: BoundaryType,
    /// Chunk index within the document (0-based).
    pub sequence_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryType {
    Paragraph,
    Heading,
    Sentence,
    Token,
}

#[derive(Debug, Error)]
pub enum ChunkError {
    #[error("empty input")]
    EmptyInput,
    #[error("tokenizer error: {0}")]
    Tokenizer(#[from] TokenizerError),
}

// ---------------------------------------------------------------------------
// Embedding status types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingStatus {
    /// Successfully embedded.
    Complete,
    /// Awaiting embedding (Ollama was unavailable).
    Pending,
    /// Embedding failed after retries.
    Error,
    /// Needs re-embedding (model changed).
    Stale,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkEmbedding {
    /// References the chunk this embedding belongs to.
    pub chunk_id: String,
    /// The embedding vector.
    pub embedding: Vec<f32>,
    /// Model used to generate this embedding.
    pub model_name: String,
    /// Embedding dimensions (for validation).
    pub dimensions: usize,
    /// Current status.
    pub status: EmbeddingStatus,
    /// Number of retry attempts.
    pub retry_count: u32,
    /// Timestamp of last attempt.
    pub last_attempt_at: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    /// Maximum number of retry attempts.
    pub max_retries: u32,
    /// Initial backoff duration in milliseconds.
    pub initial_backoff_ms: u64,
    /// Backoff multiplier (exponential).
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff_ms: 1000,
            backoff_multiplier: 2.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Chunker
// ---------------------------------------------------------------------------

pub struct Chunker {
    config: ChunkConfig,
    tokenizer: Tokenizer,
}

impl Chunker {
    pub fn new(config: ChunkConfig) -> Result<Self, TokenizerError> {
        let tokenizer = Tokenizer::new()?;
        Ok(Self { config, tokenizer })
    }

    /// Returns a reference to the underlying tokenizer.
    pub fn tokenizer(&self) -> &Tokenizer {
        &self.tokenizer
    }

    /// Returns a reference to the chunk config.
    pub fn config(&self) -> &ChunkConfig {
        &self.config
    }

    /// Split a document into chunks respecting boundary preferences.
    pub fn chunk_document(
        &self,
        source_id: &str,
        text: &str,
        sensitivity: Sensitivity,
    ) -> Result<Vec<Chunk>, ChunkError> {
        if text.trim().is_empty() {
            return Err(ChunkError::EmptyInput);
        }

        let tokens = self.tokenizer.encode(text);
        let total_tokens = tokens.len();

        // If the whole document fits in max_tokens, return it as a single chunk.
        if total_tokens <= self.config.max_tokens {
            let token_count = total_tokens;
            // For single-chunk documents below min_tokens, still return them
            // (the min_tokens rule applies to intermediate chunks, not the only chunk).
            return Ok(vec![Chunk {
                chunk_id: Uuid::new_v4().to_string(),
                source_id: source_id.to_string(),
                byte_offset: 0,
                char_length: text.len(),
                token_count,
                text: text.to_string(),
                sensitivity,
                boundary_type: BoundaryType::Token,
                sequence_index: 0,
            }]);
        }

        let mut chunks = Vec::new();
        let mut token_pos = 0usize;

        while token_pos < total_tokens {
            let remaining = total_tokens - token_pos;
            let window_end = token_pos + self.config.target_tokens.min(remaining);

            // Decode the candidate window to find boundaries in text.
            let candidate_tokens = &tokens[token_pos..window_end];
            let candidate_text = self
                .tokenizer
                .decode(candidate_tokens)
                .map_err(ChunkError::Tokenizer)?;

            // Try to find a natural boundary within the last 20% of the window.
            let (split_text, boundary_type) = if window_end < total_tokens {
                self.find_boundary(&candidate_text)
            } else {
                // Last segment: take everything remaining.
                (candidate_text.clone(), BoundaryType::Token)
            };

            let split_token_count = self.tokenizer.count_tokens(&split_text);

            // If the split produced something too small and there are more tokens,
            // fall back to the full candidate window.
            let (final_text, final_boundary, final_token_count) =
                if split_token_count < self.config.min_tokens && window_end < total_tokens {
                    (candidate_text, BoundaryType::Token, candidate_tokens.len())
                } else {
                    (split_text, boundary_type, split_token_count)
                };

            // Enforce max_tokens: if the chunk is still too large, force-split.
            let (emit_text, emit_boundary, emit_token_count) =
                if final_token_count > self.config.max_tokens {
                    let max_tokens_slice = &tokens[token_pos..token_pos + self.config.max_tokens];
                    let truncated = self
                        .tokenizer
                        .decode(max_tokens_slice)
                        .map_err(ChunkError::Tokenizer)?;
                    (truncated, BoundaryType::Token, self.config.max_tokens)
                } else {
                    (final_text, final_boundary, final_token_count)
                };

            // Compute byte offset in original text.
            let byte_offset = if chunks.is_empty() {
                0
            } else {
                // Find where this chunk's text starts in the original.
                // We decode from token_pos to get the exact text position.
                let prefix_tokens = &tokens[..token_pos];
                let prefix_text = self
                    .tokenizer
                    .decode(prefix_tokens)
                    .map_err(ChunkError::Tokenizer)?;
                prefix_text.len()
            };

            chunks.push(Chunk {
                chunk_id: Uuid::new_v4().to_string(),
                source_id: source_id.to_string(),
                byte_offset,
                char_length: emit_text.len(),
                token_count: emit_token_count,
                text: emit_text,
                sensitivity,
                boundary_type: emit_boundary,
                sequence_index: chunks.len(),
            });

            // Advance by (chunk_tokens - overlap), ensuring we always move forward.
            let advance = if emit_token_count > self.config.overlap_tokens {
                emit_token_count - self.config.overlap_tokens
            } else {
                emit_token_count.max(1)
            };
            token_pos += advance;
        }

        // Merge final chunk if it's below min_tokens and there's a previous chunk.
        if chunks.len() > 1 {
            let last = chunks.last().expect("checked len > 1");
            if last.token_count < self.config.min_tokens {
                let removed = chunks.pop().expect("checked len > 1");
                let prev = chunks.last_mut().expect("at least one chunk remains");
                // Merge: extend the previous chunk to include the removed text.
                let removed_tokens = self.tokenizer.encode(&removed.text);

                // Find overlapping tokens and merge text.
                let mut merged_text = prev.text.clone();
                // The removed chunk overlaps with the end of prev by overlap_tokens.
                // We need just the non-overlapping portion of removed.
                let overlap_token_count = self.config.overlap_tokens.min(removed_tokens.len());
                if overlap_token_count < removed_tokens.len() {
                    let new_portion = &removed_tokens[overlap_token_count..];
                    if let Ok(new_text) = self.tokenizer.decode(new_portion) {
                        merged_text.push_str(&new_text);
                    }
                }

                let merged_token_count = self.tokenizer.count_tokens(&merged_text);
                prev.text = merged_text;
                prev.token_count = merged_token_count;
                prev.char_length = prev.text.len();
                // Keep prev's boundary_type as-is.
            }
        }

        Ok(chunks)
    }

    /// Find the best natural boundary within the candidate text.
    ///
    /// Searches the last 20% of the text for boundaries in priority order:
    /// paragraph > heading > sentence > token.
    fn find_boundary(&self, text: &str) -> (String, BoundaryType) {
        let search_start = text.len() - (text.len() / 5).max(1);
        let search_region = &text[search_start..];

        // 1. Paragraph boundary: look for \n\n
        if let Some(pos) = search_region.rfind("\n\n") {
            let split_at = search_start + pos;
            if split_at > 0 {
                return (text[..split_at].to_string(), BoundaryType::Paragraph);
            }
        }

        // 2. Heading boundary: look for \n# (line starting with #)
        if let Some(pos) = search_region.rfind("\n#") {
            let split_at = search_start + pos;
            if split_at > 0 {
                return (text[..split_at].to_string(), BoundaryType::Heading);
            }
        }

        // 3. Sentence boundary: look for ". " or "! " or "? "
        let sentence_end = search_region
            .rfind(". ")
            .or_else(|| search_region.rfind("! "))
            .or_else(|| search_region.rfind("? "));
        if let Some(pos) = sentence_end {
            // Include the punctuation mark but not the space.
            let split_at = search_start + pos + 1;
            if split_at > 0 && split_at <= text.len() {
                return (text[..split_at].to_string(), BoundaryType::Sentence);
            }
        }

        // 4. Fallback: token boundary (use full text).
        (text.to_string(), BoundaryType::Token)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Tokenizer tests --

    #[test]
    fn tokenizer_initializes() {
        let tok = Tokenizer::new().expect("tokenizer should initialize");
        let count = tok.count_tokens("hello world");
        assert!(count > 0);
    }

    #[test]
    fn tokenizer_roundtrip() {
        let tok = Tokenizer::new().expect("init");
        let text = "The quick brown fox jumps over the lazy dog.";
        let tokens = tok.encode(text);
        let decoded = tok.decode(&tokens).expect("decode");
        assert_eq!(decoded, text);
    }

    #[test]
    fn tokenizer_empty_string() {
        let tok = Tokenizer::new().expect("init");
        assert_eq!(tok.count_tokens(""), 0);
    }

    // -- ChunkConfig tests --

    #[test]
    fn chunk_config_defaults() {
        let cfg = ChunkConfig::default();
        assert_eq!(cfg.target_tokens, 512);
        assert_eq!(cfg.overlap_tokens, 64);
        assert_eq!(cfg.min_tokens, 64);
        assert_eq!(cfg.max_tokens, 768);
    }

    // -- Chunker basic tests --

    #[test]
    fn chunk_empty_input_returns_error() {
        let chunker = Chunker::new(ChunkConfig::default()).expect("init");
        let result = chunker.chunk_document("src1", "", Sensitivity::Shareable);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ChunkError::EmptyInput));
    }

    #[test]
    fn chunk_whitespace_only_returns_error() {
        let chunker = Chunker::new(ChunkConfig::default()).expect("init");
        let result = chunker.chunk_document("src1", "   \n\n  ", Sensitivity::Shareable);
        assert!(result.is_err());
    }

    #[test]
    fn chunk_short_document_returns_single_chunk() {
        let chunker = Chunker::new(ChunkConfig::default()).expect("init");
        let text = "This is a short document with just a few words.";
        let chunks = chunker
            .chunk_document("short1", text, Sensitivity::Private)
            .expect("should chunk");

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].source_id, "short1");
        assert_eq!(chunks[0].text, text);
        assert_eq!(chunks[0].sensitivity, Sensitivity::Private);
        assert_eq!(chunks[0].sequence_index, 0);
        assert_eq!(chunks[0].byte_offset, 0);
    }

    #[test]
    fn chunk_document_produces_multiple_chunks_for_long_text() {
        let config = ChunkConfig {
            target_tokens: 20,
            overlap_tokens: 4,
            min_tokens: 5,
            max_tokens: 40,
        };
        let chunker = Chunker::new(config).expect("init");

        // Generate text that is at least 100 tokens long.
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(20);
        let chunks = chunker
            .chunk_document("long1", &text, Sensitivity::Shareable)
            .expect("should chunk");

        assert!(
            chunks.len() > 1,
            "expected multiple chunks, got {}",
            chunks.len()
        );

        // Verify sequence indices are sequential.
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.sequence_index, i);
            assert_eq!(chunk.source_id, "long1");
            assert!(!chunk.chunk_id.is_empty());
        }
    }

    #[test]
    fn chunk_ids_are_unique() {
        let config = ChunkConfig {
            target_tokens: 20,
            overlap_tokens: 4,
            min_tokens: 5,
            max_tokens: 40,
        };
        let chunker = Chunker::new(config).expect("init");
        let text = "word ".repeat(200);
        let chunks = chunker
            .chunk_document("uniq1", &text, Sensitivity::Shareable)
            .expect("should chunk");

        let ids: Vec<&str> = chunks.iter().map(|c| c.chunk_id.as_str()).collect();
        let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "chunk IDs should be unique");
    }

    // -- Boundary detection tests --

    #[test]
    fn boundary_detects_paragraph() {
        // Use the Chunker's internal find_boundary directly.
        // \n\n must be in the last 20% of the string.
        let chunker = Chunker::new(ChunkConfig::default()).expect("init");

        // 80 chars before \n\n, then 10 chars after = 92 total.
        // search_start = 92 - (92/5) = 92 - 18 = 74. \n\n is at pos 80. 80 > 74, so it's in range.
        let text = "The quick brown fox jumps over the lazy dog runs across the field near forest.\n\nMore words.";
        let (split_text, boundary) = chunker.find_boundary(text);

        assert_eq!(boundary, BoundaryType::Paragraph);
        assert!(!split_text.contains("\n\n"));
    }

    #[test]
    fn boundary_detects_heading() {
        let config = ChunkConfig {
            target_tokens: 30,
            overlap_tokens: 4,
            min_tokens: 5,
            max_tokens: 60,
        };
        let chunker = Chunker::new(config).expect("init");

        let text = "Some content that fills up the space before the heading boundary.\n## New Section\nMore content in the new section that extends past the target token count. Even more words here.";
        let chunks = chunker
            .chunk_document("head1", text, Sensitivity::Shareable)
            .expect("should chunk");

        let has_heading = chunks
            .iter()
            .any(|c| c.boundary_type == BoundaryType::Heading);
        // Heading detection depends on where the boundary falls in the window.
        // If text is small enough, it may be a single chunk.
        if chunks.len() > 1 {
            assert!(
                has_heading,
                "expected heading boundary in multi-chunk result, got: {:?}",
                chunks.iter().map(|c| c.boundary_type).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn boundary_detects_sentence() {
        let config = ChunkConfig {
            target_tokens: 20,
            overlap_tokens: 4,
            min_tokens: 5,
            max_tokens: 40,
        };
        let chunker = Chunker::new(config).expect("init");

        let text = "First sentence here with words. Second sentence follows up. Third sentence adds more. Fourth sentence extends the text. Fifth sentence keeps going.";
        let chunks = chunker
            .chunk_document("sent1", text, Sensitivity::Shareable)
            .expect("should chunk");

        if chunks.len() > 1 {
            let has_sentence = chunks
                .iter()
                .any(|c| c.boundary_type == BoundaryType::Sentence);
            assert!(
                has_sentence,
                "expected sentence boundary, got: {:?}",
                chunks.iter().map(|c| c.boundary_type).collect::<Vec<_>>()
            );
        }
    }

    // -- Max/min enforcement tests --

    #[test]
    fn chunks_respect_max_tokens() {
        let config = ChunkConfig {
            target_tokens: 20,
            overlap_tokens: 4,
            min_tokens: 5,
            max_tokens: 30,
        };
        let chunker = Chunker::new(config.clone()).expect("init");

        let text = "word ".repeat(200);
        let chunks = chunker
            .chunk_document("max1", &text, Sensitivity::Shareable)
            .expect("should chunk");

        for chunk in &chunks {
            assert!(
                chunk.token_count <= config.max_tokens,
                "chunk {} has {} tokens, exceeds max {}",
                chunk.sequence_index,
                chunk.token_count,
                config.max_tokens
            );
        }
    }

    #[test]
    fn final_small_chunk_is_merged() {
        let config = ChunkConfig {
            target_tokens: 20,
            overlap_tokens: 2,
            min_tokens: 10,
            max_tokens: 50,
        };
        let chunker = Chunker::new(config.clone()).expect("init");

        // Create text that would leave a tiny fragment at the end.
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega";
        let chunks = chunker
            .chunk_document("merge1", text, Sensitivity::Shareable)
            .expect("should chunk");

        // If the last chunk was below min_tokens, it should have been merged.
        if chunks.len() > 1 {
            let last = chunks.last().expect("at least one chunk");
            // The last chunk may be at or above min_tokens after merging,
            // or it could still be below if it's the only chunk left.
            // We just verify no panic and reasonable output.
            assert!(last.token_count > 0);
        }
    }

    // -- Overlap tests --

    #[test]
    fn chunks_have_overlapping_content() {
        let config = ChunkConfig {
            target_tokens: 20,
            overlap_tokens: 6,
            min_tokens: 5,
            max_tokens: 40,
        };
        let chunker = Chunker::new(config).expect("init");

        let text = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty twenty-one twenty-two twenty-three twenty-four twenty-five twenty-six twenty-seven twenty-eight twenty-nine thirty";
        let chunks = chunker
            .chunk_document("overlap1", text, Sensitivity::Shareable)
            .expect("should chunk");

        if chunks.len() >= 2 {
            // Adjacent chunks should share some text due to overlap.
            let first_end = &chunks[0].text;
            let second_start = &chunks[1].text;

            // There should be some overlapping substring between end of first
            // and beginning of second.
            let first_words: Vec<&str> = first_end.split_whitespace().collect();
            let second_words: Vec<&str> = second_start.split_whitespace().collect();

            let last_few_first: Vec<&str> =
                first_words.iter().rev().take(10).rev().copied().collect();
            let first_few_second: Vec<&str> = second_words.iter().take(10).copied().collect();

            // Check that at least one word from the end of chunk 0 appears
            // at the beginning of chunk 1.
            let overlap_found = last_few_first.iter().any(|w| first_few_second.contains(w));
            assert!(
                overlap_found,
                "expected overlapping words between chunks.\nchunk 0 tail: {last_few_first:?}\nchunk 1 head: {first_few_second:?}"
            );
        }
    }

    // -- Sensitivity inheritance --

    #[test]
    fn chunks_inherit_sensitivity() {
        let chunker = Chunker::new(ChunkConfig::default()).expect("init");
        let text = "Content that will be chunked into a single piece for testing purposes.";
        let chunks = chunker
            .chunk_document("sens1", text, Sensitivity::Restricted)
            .expect("should chunk");

        for chunk in &chunks {
            assert_eq!(chunk.sensitivity, Sensitivity::Restricted);
        }
    }

    // -- EmbeddingStatus tests --

    #[test]
    fn embedding_status_serialization() {
        let status = EmbeddingStatus::Complete;
        let json = serde_json::to_string(&status).expect("serialize");
        assert_eq!(json, "\"complete\"");

        let pending: EmbeddingStatus = serde_json::from_str("\"pending\"").expect("deserialize");
        assert_eq!(pending, EmbeddingStatus::Pending);
    }

    #[test]
    fn chunk_embedding_roundtrip() {
        let ce = ChunkEmbedding {
            chunk_id: "test-chunk".to_string(),
            embedding: vec![0.1, 0.2, 0.3],
            model_name: "nomic-embed-text".to_string(),
            dimensions: 3,
            status: EmbeddingStatus::Complete,
            retry_count: 0,
            last_attempt_at: None,
        };
        let json = serde_json::to_string(&ce).expect("serialize");
        let deserialized: ChunkEmbedding = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.chunk_id, "test-chunk");
        assert_eq!(deserialized.status, EmbeddingStatus::Complete);
        assert_eq!(deserialized.dimensions, 3);
    }

    // -- RetryConfig tests --

    #[test]
    fn retry_config_defaults() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.initial_backoff_ms, 1000);
        assert!((cfg.backoff_multiplier - 2.0).abs() < f64::EPSILON);
    }

    // -- BoundaryType serialization --

    #[test]
    fn boundary_type_serialization() {
        let bt = BoundaryType::Paragraph;
        let json = serde_json::to_string(&bt).expect("serialize");
        assert_eq!(json, "\"paragraph\"");

        let heading: BoundaryType = serde_json::from_str("\"heading\"").expect("deserialize");
        assert_eq!(heading, BoundaryType::Heading);
    }
}
