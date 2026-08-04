//! Content-derived event ids, so re-ingest dedupes.
//!
//! The id is a truncated SHA-256 over the identifying parts of an event, joined
//! with a separator that cannot appear in a length-prefixed encoding. It is
//! stable across runs, processes, and machines.

use sha2::{Digest, Sha256};

/// Length of a rendered id in hex characters.
const ID_HEX_LEN: usize = 32;

/// The identifying content of an event. Two source records with identical
/// identity produce the same event id.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventIdentity<'a> {
    pub agent: &'a str,
    pub session_id: Option<&'a str>,
    /// The source record's own id (e.g. Claude Code's `uuid`), when it has one.
    pub source_id: Option<&'a str>,
    pub ts: i64,
    pub role: &'a str,
    /// Any extra discriminator an adapter needs to keep sibling records apart.
    pub extra: Option<&'a str>,
}

/// Deterministic id for an event.
pub fn event_id(identity: EventIdentity<'_>) -> String {
    let mut hasher = Sha256::new();
    write_field(&mut hasher, identity.agent.as_bytes());
    write_field(&mut hasher, identity.session_id.unwrap_or("").as_bytes());
    write_field(&mut hasher, identity.source_id.unwrap_or("").as_bytes());
    write_field(&mut hasher, &identity.ts.to_be_bytes());
    write_field(&mut hasher, identity.role.as_bytes());
    write_field(&mut hasher, identity.extra.unwrap_or("").as_bytes());
    hex(&hasher.finalize())[..ID_HEX_LEN].to_string()
}

/// Deterministic hash of prompt text, used for exact-duplicate detection.
pub fn text_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex(&hasher.finalize())[..ID_HEX_LEN].to_string()
}

/// Length-prefixed so concatenation is unambiguous: `("ab","c")` and
/// `("a","bc")` must not hash alike.
fn write_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> EventIdentity<'static> {
        EventIdentity {
            agent: "claude-code",
            session_id: Some("sess-1"),
            source_id: Some("uuid-1"),
            ts: 1_754_300_000_000,
            role: "assistant",
            extra: None,
        }
    }

    #[test]
    fn same_content_same_id() {
        assert_eq!(event_id(identity()), event_id(identity()));
    }

    #[test]
    fn id_is_stable_across_runs() {
        // Pinned: a change here means previously ingested events would re-add.
        assert_eq!(event_id(identity()), "6cbc1e2d028a0bc4fee7ae15c0405df6");
    }

    #[test]
    fn different_content_different_id() {
        let base = event_id(identity());
        let mut other = identity();
        other.ts += 1;
        assert_ne!(event_id(other), base);

        let mut other = identity();
        other.role = "user";
        assert_ne!(event_id(other), base);

        let mut other = identity();
        other.extra = Some("1");
        assert_ne!(event_id(other), base);
    }

    #[test]
    fn field_boundaries_are_unambiguous() {
        let a = EventIdentity {
            agent: "ab",
            session_id: Some("c"),
            ..EventIdentity::default()
        };
        let b = EventIdentity {
            agent: "a",
            session_id: Some("bc"),
            ..EventIdentity::default()
        };
        assert_ne!(event_id(a), event_id(b));
    }

    #[test]
    fn text_hash_is_deterministic() {
        assert_eq!(text_hash("run the tests"), text_hash("run the tests"));
        assert_ne!(text_hash("run the tests"), text_hash("run the test"));
        assert_eq!(text_hash("").len(), ID_HEX_LEN);
    }
}
