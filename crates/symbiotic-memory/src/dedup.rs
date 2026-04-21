//! Entity deduplication: name normalization and fuzzy matching.

/// Normalize an entity name for deduplication matching.
///
/// Steps: lowercase, trim whitespace, collapse internal whitespace.
pub fn normalize_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Compute Levenshtein edit distance between two strings.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a_len = a.chars().count();
    let b_len = b.chars().count();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0usize; b_len + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_len]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_trims_and_lowercases() {
        assert_eq!(normalize_name("  Alice  Smith  "), "alice smith");
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize_name("Bob   Jones"), "bob jones");
    }

    #[test]
    fn edit_distance_identical() {
        assert_eq!(edit_distance("rust", "rust"), 0);
    }

    #[test]
    fn edit_distance_one_char() {
        assert_eq!(edit_distance("rust", "ruts"), 2);
    }

    #[test]
    fn edit_distance_empty() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
    }

    #[test]
    fn edit_distance_similar_names() {
        // "alice" vs "alise" — 1 substitution
        assert_eq!(edit_distance("alice", "alise"), 1);
    }

    #[test]
    fn edit_distance_very_different() {
        assert!(edit_distance("alice", "bob") > 2);
    }
}
