//! Bloom filter for fast local threat URL checking.
//!
//! Uses XXH3-128 with Kirsch-Mitzenmacker double-hashing for O(1) lookups.
//! Parameters: 1M URLs capacity, 0.1% FPR, ~1.8MB, 10 hash functions.

use std::io::{Read, Write};

use anyhow::Result;
use thiserror::Error;
use url::Url;
use xxhash_rust::xxh3::xxh3_128;

use crate::{StaticThreatChecker, ThreatChecker};

const BLOOM_MAGIC: &[u8; 4] = b"BLMF";

#[derive(Debug, Error)]
pub enum BloomError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid bloom filter file: {0}")]
    InvalidFile(String),
}

pub struct BloomFilter {
    bits: Vec<u64>,
    num_bits: u64,
    num_hashes: u32,
}

impl BloomFilter {
    /// Create a new bloom filter optimized for the given capacity and false positive rate.
    pub fn new(capacity: u64, fp_rate: f64) -> Self {
        let num_bits = Self::optimal_bits(capacity, fp_rate);
        let num_hashes = Self::optimal_hashes(num_bits, capacity);
        let words = num_bits.div_ceil(64) as usize;
        Self {
            bits: vec![0u64; words],
            num_bits,
            num_hashes,
        }
    }

    /// Create with default parameters: 1M URLs, 0.1% FPR.
    pub fn default_threat_filter() -> Self {
        Self::new(1_000_000, 0.001)
    }

    /// Insert a URL into the filter.
    pub fn insert(&mut self, url: &str) {
        let normalized = normalize_url(url);
        let hash = xxh3_128(normalized.as_bytes());
        let (h1, h2) = (hash as u64, (hash >> 64) as u64);
        for i in 0..self.num_hashes {
            let bit = h1.wrapping_add((i as u64).wrapping_mul(h2)) % self.num_bits;
            self.set_bit(bit);
        }
    }

    /// Check if a URL might be in the filter (may have false positives).
    pub fn contains(&self, url: &str) -> bool {
        let normalized = normalize_url(url);
        let hash = xxh3_128(normalized.as_bytes());
        let (h1, h2) = (hash as u64, (hash >> 64) as u64);
        (0..self.num_hashes).all(|i| {
            let bit = h1.wrapping_add((i as u64).wrapping_mul(h2)) % self.num_bits;
            self.get_bit(bit)
        })
    }

    /// Number of bits in the filter.
    pub fn capacity_bits(&self) -> u64 {
        self.num_bits
    }

    /// Save to a writer (binary format: magic + header + bit array).
    pub fn to_writer<W: Write>(&self, writer: &mut W) -> Result<(), BloomError> {
        writer.write_all(BLOOM_MAGIC)?;
        writer.write_all(&self.num_bits.to_le_bytes())?;
        writer.write_all(&self.num_hashes.to_le_bytes())?;
        for word in &self.bits {
            writer.write_all(&word.to_le_bytes())?;
        }
        Ok(())
    }

    /// Load from a reader.
    pub fn from_reader<R: Read>(reader: &mut R) -> Result<Self, BloomError> {
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;
        if &magic != BLOOM_MAGIC {
            return Err(BloomError::InvalidFile(format!(
                "bad magic: expected BLMF, got {:?}",
                magic
            )));
        }

        let mut buf8 = [0u8; 8];
        reader.read_exact(&mut buf8)?;
        let num_bits = u64::from_le_bytes(buf8);

        let mut buf4 = [0u8; 4];
        reader.read_exact(&mut buf4)?;
        let num_hashes = u32::from_le_bytes(buf4);

        let num_words = num_bits.div_ceil(64) as usize;
        let mut bits = Vec::with_capacity(num_words);
        for _ in 0..num_words {
            let mut word_buf = [0u8; 8];
            reader.read_exact(&mut word_buf)?;
            bits.push(u64::from_le_bytes(word_buf));
        }

        Ok(Self {
            bits,
            num_bits,
            num_hashes,
        })
    }

    /// Save to file.
    pub fn to_file(&self, path: &str) -> Result<(), BloomError> {
        let mut file = std::fs::File::create(path)?;
        self.to_writer(&mut file)
    }

    /// Load from file.
    pub fn from_file(path: &str) -> Result<Self, BloomError> {
        let mut file = std::fs::File::open(path)?;
        Self::from_reader(&mut file)
    }

    fn optimal_bits(n: u64, p: f64) -> u64 {
        (-(n as f64) * p.ln() / (2.0_f64.ln().powi(2))).ceil() as u64
    }

    fn optimal_hashes(m: u64, n: u64) -> u32 {
        ((m as f64 / n as f64) * 2.0_f64.ln()).round() as u32
    }

    fn get_bit(&self, bit: u64) -> bool {
        let word_index = (bit / 64) as usize;
        let bit_offset = bit % 64;
        (self.bits[word_index] >> bit_offset) & 1 == 1
    }

    fn set_bit(&mut self, bit: u64) {
        let word_index = (bit / 64) as usize;
        let bit_offset = bit % 64;
        self.bits[word_index] |= 1 << bit_offset;
    }
}

/// Normalize a URL for consistent bloom filter matching.
///
/// 1. If no scheme, prepend "https://"
/// 2. Parse with `url::Url`
/// 3. Lowercase host
/// 4. Remove default ports (80, 443)
/// 5. Strip trailing slash from path
/// 6. Remove query parameters and fragments
/// 7. Return "host/path" for hashing (or just "host" if path is empty or "/")
pub fn normalize_url(input: &str) -> String {
    let with_scheme = if !input.contains("://") {
        format!("https://{input}")
    } else {
        input.to_string()
    };

    let parsed = match Url::parse(&with_scheme) {
        Ok(u) => u,
        Err(_) => return input.to_ascii_lowercase(),
    };

    let host = match parsed.host_str() {
        Some(h) => h.to_ascii_lowercase(),
        None => return input.to_ascii_lowercase(),
    };

    // Check for default ports to strip
    let has_default_port = matches!(
        (parsed.scheme(), parsed.port()),
        ("http", Some(80)) | ("https", Some(443))
    );
    let port_suffix = match parsed.port() {
        Some(p) if !has_default_port => format!(":{p}"),
        _ => String::new(),
    };

    let path = parsed.path().trim_end_matches('/');
    if path.is_empty() {
        format!("{host}{port_suffix}")
    } else {
        format!("{host}{port_suffix}{path}")
    }
}

/// Layered threat checker: bloom filter first, then static blocklist.
pub struct BloomThreatChecker {
    bloom: BloomFilter,
    static_checker: StaticThreatChecker,
}

impl BloomThreatChecker {
    pub fn new(bloom: BloomFilter, static_checker: StaticThreatChecker) -> Self {
        Self {
            bloom,
            static_checker,
        }
    }
}

impl ThreatChecker for BloomThreatChecker {
    fn is_safe(&self, target: &str) -> Result<bool> {
        let normalized = normalize_url(target);
        if self.bloom.contains(&normalized) {
            return Ok(false);
        }
        self.static_checker.is_safe(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn insert_and_contains() {
        let mut bf = BloomFilter::new(1000, 0.01);
        bf.insert("https://evil.com/phishing");
        assert!(bf.contains("https://evil.com/phishing"));
        assert!(!bf.contains("https://safe.com"));
    }

    #[test]
    fn url_normalization_basic() {
        assert_eq!(normalize_url("HTTPS://EXAMPLE.COM/"), "example.com");
        assert_eq!(
            normalize_url("http://Example.Com/path/"),
            "example.com/path"
        );
        assert_eq!(normalize_url("https://foo.com:443/bar"), "foo.com/bar");
        assert_eq!(normalize_url("http://foo.com:80/"), "foo.com");
    }

    #[test]
    fn url_normalization_strips_query_and_fragment() {
        assert_eq!(
            normalize_url("https://evil.com/page?id=1&ref=x"),
            "evil.com/page"
        );
        assert_eq!(
            normalize_url("https://evil.com/page#section"),
            "evil.com/page"
        );
    }

    #[test]
    fn url_normalization_adds_scheme() {
        assert_eq!(normalize_url("evil.com/phishing"), "evil.com/phishing");
    }

    #[test]
    fn false_positive_rate_within_bounds() {
        let mut bf = BloomFilter::new(10_000, 0.01);
        for i in 0..10_000 {
            bf.insert(&format!("https://evil{i}.example.com"));
        }
        let mut false_positives = 0;
        for i in 0..10_000 {
            if bf.contains(&format!("https://safe{i}.test.org")) {
                false_positives += 1;
            }
        }
        let fpr = false_positives as f64 / 10_000.0;
        assert!(fpr < 0.03, "FPR {fpr:.4} exceeds 3% (expected ~1%)");
    }

    #[test]
    fn persistence_roundtrip() {
        let mut bf = BloomFilter::new(1000, 0.01);
        bf.insert("https://phishing.example.com");
        bf.insert("https://malware.evil.org");

        let mut buf = Vec::new();
        bf.to_writer(&mut buf).unwrap();

        let loaded = BloomFilter::from_reader(&mut &buf[..]).unwrap();
        assert!(loaded.contains("https://phishing.example.com"));
        assert!(loaded.contains("https://malware.evil.org"));
        assert!(!loaded.contains("https://safe.example.com"));
    }

    #[test]
    fn default_threat_filter_parameters() {
        let bf = BloomFilter::default_threat_filter();
        // 1M capacity, 0.1% FPR -> ~14.4M bits
        assert!(bf.num_bits > 14_000_000);
        assert!(bf.num_bits < 15_000_000);
        assert_eq!(bf.num_hashes, 10);
    }

    #[test]
    fn bloom_threat_checker_integration() {
        let mut bloom = BloomFilter::new(1000, 0.01);
        bloom.insert("https://phishing.com/login");

        let static_checker = StaticThreatChecker::new(HashSet::from(["evil.org".to_string()]));

        let checker = BloomThreatChecker::new(bloom, static_checker);
        // Bloom catches this
        assert!(!checker.is_safe("https://phishing.com/login").unwrap());
        // Static catches this
        assert!(!checker.is_safe("evil.org").unwrap());
        // Neither catches this
        assert!(checker.is_safe("https://safe.example.com").unwrap());
    }

    #[test]
    fn empty_filter_contains_nothing() {
        let bf = BloomFilter::new(1000, 0.01);
        assert!(!bf.contains("https://anything.com"));
    }

    #[test]
    fn file_persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bloom");
        let path_str = path.to_str().unwrap();

        let mut bf = BloomFilter::new(1000, 0.01);
        bf.insert("https://test.com");
        bf.to_file(path_str).unwrap();

        let loaded = BloomFilter::from_file(path_str).unwrap();
        assert!(loaded.contains("https://test.com"));
    }
}
