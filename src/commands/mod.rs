//! Command implementations: the thin layer between the CLI and the library.
//!
//! `ingest` and `doctor` render with plain `println!`; the reporting commands go
//! through [`crate::output::emit`], which owns the table/`--json` split.

pub mod doctor;
pub mod ingest;
pub mod query;
pub mod report;

use std::io;

use crate::cli::TimeWindow;
use crate::config::Config;
use crate::reports::ReportCtx;
use crate::store::StorePaths;

/// The resolved global flags a reporting command needs.
///
/// Borrowed rather than owned so `main` can resolve config and paths once and
/// hand the same view to every command.
#[derive(Debug, Clone, Copy)]
pub struct Env<'a> {
    pub config: &'a Config,
    pub paths: &'a StorePaths,
    pub window: TimeWindow,
    pub project: Option<&'a str>,
    pub json: bool,
    pub no_ingest: bool,
    /// Sidechain (subagent) events are real spend, so they are in by default.
    pub include_sidechain: bool,
}

impl Env<'_> {
    pub fn ctx(&self) -> ReportCtx {
        ReportCtx::new(
            self.window,
            self.project.map(str::to_string),
            self.include_sidechain,
        )
    }

    /// The implicit ingest before a report (MVP §3), unless `--no-ingest`.
    ///
    /// Its progress goes to **stderr**: stdout belongs to the report, and in
    /// `--json` mode it must stay a single parseable document.
    pub fn pre_ingest(&self) -> io::Result<()> {
        if self.no_ingest {
            return Ok(());
        }
        let report = ingest::run_quiet(self.config, self.paths, self.window, self.project)?;
        ingest::write_lines(&mut io::stderr(), &report)
    }
}

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
