//! Firewall version + semver helpers.
//!
//! Every [`crate::FirewallVerdict`] records the firewall version under which
//! it was produced. The background Replay job (design §6.3) compares stored
//! verdicts against [`SECURITY_VERSION`] and re-scans entries with older
//! versions. The helpers below keep the version-comparison logic in one
//! place so the Replay job (future chunk) doesn't hand-roll a parser.

use std::cmp::Ordering;

use crate::errors::{FirewallError, FirewallResult};

/// Current firewall security version (semver). Bump when scan rules change
/// in a way that should trigger Replay over existing Archive entries.
pub const SECURITY_VERSION: &str = "0.1.0";

/// Parsed semver triple. Only the `major.minor.patch` form is supported —
/// pre-release / build-metadata suffixes are rejected to keep the wire
/// format dead-simple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SecurityVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl SecurityVersion {
    /// Current firewall version parsed from [`SECURITY_VERSION`].
    pub fn current() -> Self {
        Self::parse(SECURITY_VERSION).expect("SECURITY_VERSION must be valid semver")
    }

    /// Parse a `major.minor.patch` string.
    pub fn parse(input: &str) -> FirewallResult<Self> {
        let mut parts = input.split('.');
        let major = parse_part(parts.next(), input)?;
        let minor = parse_part(parts.next(), input)?;
        let patch = parse_part(parts.next(), input)?;
        if parts.next().is_some() {
            return Err(FirewallError::UnsupportedVersion(input.to_string()));
        }
        Ok(Self {
            major,
            minor,
            patch,
        })
    }

    /// Ordering helper: is `self` strictly older than `other`?
    pub fn is_older_than(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Less
    }
}

impl PartialOrd for SecurityVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SecurityVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
    }
}

fn parse_part(part: Option<&str>, full: &str) -> FirewallResult<u32> {
    part.ok_or_else(|| FirewallError::UnsupportedVersion(full.to_string()))
        .and_then(|p| {
            p.parse::<u32>()
                .map_err(|_| FirewallError::UnsupportedVersion(full.to_string()))
        })
}

/// Should an entry scanned at `stored` be re-evaluated by Replay against
/// `current`? True when `stored < current`. Returns an error if either
/// version string is malformed.
pub fn needs_replay(stored: &str, current: &str) -> FirewallResult<bool> {
    let stored = SecurityVersion::parse(stored)?;
    let current = SecurityVersion::parse(current)?;
    Ok(stored.is_older_than(&current))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_version_parses() {
        assert_eq!(
            SecurityVersion::parse("0.1.0").unwrap(),
            SecurityVersion {
                major: 0,
                minor: 1,
                patch: 0
            }
        );
        assert_eq!(
            SecurityVersion::parse("12.34.56").unwrap(),
            SecurityVersion {
                major: 12,
                minor: 34,
                patch: 56
            }
        );
    }

    #[test]
    fn security_version_rejects_malformed() {
        assert!(SecurityVersion::parse("0.1").is_err());
        assert!(SecurityVersion::parse("0.1.0.0").is_err());
        assert!(SecurityVersion::parse("0.1.0-beta").is_err());
        assert!(SecurityVersion::parse("not-a-version").is_err());
        assert!(SecurityVersion::parse("").is_err());
    }

    #[test]
    fn current_matches_const() {
        let v = SecurityVersion::current();
        assert_eq!(v.major, 0);
        assert_eq!(v.minor, 1);
        assert_eq!(v.patch, 0);
    }

    #[test]
    fn ordering_is_lexicographic_on_triples() {
        let a = SecurityVersion::parse("0.1.0").unwrap();
        let b = SecurityVersion::parse("0.1.1").unwrap();
        let c = SecurityVersion::parse("0.2.0").unwrap();
        let d = SecurityVersion::parse("1.0.0").unwrap();
        assert!(a.is_older_than(&b));
        assert!(b.is_older_than(&c));
        assert!(c.is_older_than(&d));
        assert!(!d.is_older_than(&a));
    }

    #[test]
    fn needs_replay_compares_strings() {
        assert!(needs_replay("0.0.9", "0.1.0").unwrap());
        assert!(!needs_replay("0.1.0", "0.1.0").unwrap());
        assert!(!needs_replay("0.2.0", "0.1.0").unwrap());
        assert!(needs_replay("0.1.0", "1.0.0").unwrap());
    }

    #[test]
    fn needs_replay_errors_on_malformed_input() {
        assert!(needs_replay("bogus", "0.1.0").is_err());
        assert!(needs_replay("0.1.0", "bogus").is_err());
    }
}
