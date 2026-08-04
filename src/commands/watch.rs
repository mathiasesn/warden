//! `warden watch` (MVP §3, build-order step 10 — the nice-to-have).
//!
//! MVP ships the `--oneline` form only: one status-bar line, computed from the
//! current month's partition, then exit. That is the shape a tmux `status-right`
//! actually wants, and it is honest about cost — a refresh is one bounded scan.
//!
//! The streaming form is deliberately absent rather than half-built: doing it
//! properly means tailing the partition from a byte offset instead of rescanning
//! (MVP §2.4), and that is not MVP work. It says so rather than pretending.

use std::io::{self, Write};

use chrono::Utc;

use crate::output::{emit, format_count, Cell, Report, Table};
use crate::reports::{count, format_span};
use crate::store::{Partition, ScanQuery, Scanner};

use super::Env;

/// The window burn rate is averaged over.
const RATE_WINDOW_MS: i64 = 60 * 60 * 1000;

/// What the status line says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Burn {
    pub project: Option<String>,
    /// Tokens in the last hour — the rate is per hour, so this *is* the rate.
    pub tokens_per_hour: u64,
    pub session_id: Option<String>,
    pub session_ms: i64,
    pub session_tokens: u64,
}

impl Burn {
    /// `acme-api · 418.0k tok/hr · session 1h12m · 84.2k this session`
    pub fn line(&self) -> String {
        let Some(project) = &self.project else {
            return "no activity in the current partition".to_string();
        };
        format!(
            "{project} · {} tok/hr · session {} · {} this session",
            format_count(self.tokens_per_hour as i64),
            crate::reports::format_span(self.session_ms),
            format_count(self.session_tokens as i64),
        )
    }
}

pub fn run(env: &Env<'_>, oneline: bool) -> io::Result<Burn> {
    if !oneline {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "streaming `warden watch` is not implemented in this release; use \
             `warden watch --oneline` (e.g. from a tmux status line, or `watch -n5`)",
        ));
    }

    env.pre_ingest()?;
    let now = Utc::now();
    let burn = burn(
        &Scanner::new(env.paths.clone()),
        env.project,
        now.timestamp_millis(),
    )?;

    if env.json {
        emit(&report(&burn, env), true)?;
    } else {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{}", burn.line())?;
        stdout.flush()?;
    }
    Ok(burn)
}

/// Burn rate from the current month's partition only.
///
/// Bounded by construction: the scan window starts at the beginning of the
/// current UTC month, so `watch` never widens with the age of the store.
pub fn burn(scanner: &Scanner, project: Option<&str>, now_ms: i64) -> io::Result<Burn> {
    let Some(current) = Partition::for_timestamp(now_ms) else {
        return Ok(Burn::default());
    };
    let window = crate::cli::TimeWindow::new(current.start_ms(), now_ms.saturating_add(1));
    let query = ScanQuery::new(window).with_project(project.map(str::to_string));

    // The session is whichever one the most recent event belongs to.
    let mut latest: Option<(i64, Option<String>, Option<String>)> = None;
    let mut recent_tokens: u64 = 0;
    let mut per_session: std::collections::HashMap<String, (i64, i64, u64)> =
        std::collections::HashMap::new();

    scanner.scan_with(&query, |event| {
        let tokens = event.total_tokens();
        if event.ts >= now_ms - RATE_WINDOW_MS {
            recent_tokens += tokens;
        }
        if latest.as_ref().is_none_or(|(ts, _, _)| event.ts >= *ts) {
            latest = Some((event.ts, event.project.clone(), event.session_id.clone()));
        }
        if let Some(session) = &event.session_id {
            let entry = per_session
                .entry(session.clone())
                .or_insert((event.ts, event.ts, 0));
            entry.0 = entry.0.min(event.ts);
            entry.1 = entry.1.max(event.ts);
            entry.2 += tokens;
        }
    })?;

    let Some((_, project, session_id)) = latest else {
        return Ok(Burn::default());
    };
    let (session_ms, session_tokens) = session_id
        .as_ref()
        .and_then(|id| per_session.get(id))
        .map(|(first, last, tokens)| (last - first, *tokens))
        .unwrap_or((0, 0));

    Ok(Burn {
        project,
        tokens_per_hour: recent_tokens,
        session_id,
        session_ms,
        session_tokens,
    })
}

fn report(burn: &Burn, env: &Env<'_>) -> Report {
    let table = Table::new(["project", "tok/hr", "session", "session tokens"]).with_row(vec![
        Cell::text(burn.project.clone().unwrap_or_else(|| "(none)".into())),
        count(burn.tokens_per_hour),
        Cell::text(format_span(burn.session_ms)),
        count(burn.session_tokens),
    ]);

    Report::new("watch", env.window, table)
        .with_json_rows(vec![serde_json::json!({
            "project": burn.project,
            "tokens_per_hour": burn.tokens_per_hour,
            "session_id": burn.session_id,
            "session_ms": burn.session_ms,
            "session_tokens": burn.session_tokens,
        })])
        .with_notes([
            "tok/hr is every token in the last hour — input, output, and cache — from the \
                current month's partition only",
            "--since does not apply to watch: it always reports on now",
        ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reports::testkit::{store, used};
    use chrono::{TimeZone, Utc};

    /// Timestamps relative to a fixed "now" inside a real month partition.
    fn now() -> i64 {
        Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
            .unwrap()
            .timestamp_millis()
    }

    fn mins(n: i64) -> i64 {
        now() - n * 60_000
    }

    #[test]
    fn rate_counts_only_the_last_hour_but_the_session_counts_all_of_it() {
        let (_dir, paths) = store(&[
            // Three hours ago: in the session, out of the rate window.
            used("a", mins(180), "acme-api", "m", 1_000, 100),
            used("b", mins(30), "acme-api", "m", 2_000, 200),
            used("c", mins(5), "acme-api", "m", 3_000, 300),
        ]);
        let burn = burn(&Scanner::new(paths), None, now()).unwrap();

        // `used` also sets cache_read = input * 10.
        assert_eq!(
            burn.tokens_per_hour,
            (2_000 + 200 + 20_000) + (3_000 + 300 + 30_000)
        );
        assert_eq!(burn.session_tokens, 11_100 + 22_200 + 33_300);
        assert_eq!(burn.session_ms, 175 * 60_000);
        assert_eq!(burn.project.as_deref(), Some("acme-api"));
        assert!(burn.line().starts_with("acme-api · "), "{}", burn.line());
        assert!(burn.line().contains("2h55m"), "{}", burn.line());
    }

    #[test]
    fn the_session_is_the_one_the_latest_event_belongs_to() {
        let (_dir, paths) = store(&[
            used("a", mins(50), "acme-api", "m", 1_000, 0),
            used("b", mins(10), "dotfiles", "m", 5, 0),
        ]);
        let burn = burn(&Scanner::new(paths), None, now()).unwrap();
        assert_eq!(burn.project.as_deref(), Some("dotfiles"));
        assert_eq!(burn.session_tokens, 55);
        // The rate still spans both projects.
        assert_eq!(burn.tokens_per_hour, 11_000 + 55);
    }

    #[test]
    fn an_empty_store_says_so_rather_than_printing_zeroes() {
        let (_dir, paths) = store(&[]);
        let burn = burn(&Scanner::new(paths), None, now()).unwrap();
        assert_eq!(burn, Burn::default());
        assert_eq!(burn.line(), "no activity in the current partition");
    }

    #[test]
    fn honours_the_project_filter() {
        let (_dir, paths) = store(&[
            used("a", mins(10), "acme-api", "m", 1_000, 0),
            used("b", mins(5), "dotfiles", "m", 7, 0),
        ]);
        let burn = burn(&Scanner::new(paths), Some("acme-api"), now()).unwrap();
        assert_eq!(burn.project.as_deref(), Some("acme-api"));
        assert_eq!(burn.tokens_per_hour, 11_000);
    }
}
