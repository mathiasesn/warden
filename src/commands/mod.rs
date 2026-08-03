//! Command implementations: the thin layer between the CLI and the library.
//!
//! Rendering here is plain `println!` on purpose. Phase 3 routes both commands
//! through the shared output layer and the `--json` envelope; keeping the
//! formatting in one function per command is what makes that a small change.

pub mod doctor;
pub mod ingest;

/// `1203` → `1,203`. Counts in this output are read by humans.
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_digits_in_threes() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_203), "1,203");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }
}
