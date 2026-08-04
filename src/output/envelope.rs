//! The versioned `--json` envelope (MVP §5).
//!
//! Compatibility contract, and the reason this type is centralized:
//!
//! - Fields may be **added** freely. Consumers must tolerate unknown fields.
//! - Fields may **not** be renamed, retyped, or removed without bumping
//!   [`crate::store::RECORD_VERSION`].
//! - `warden_version` tracks the binary; `record_version` tracks the shape of
//!   the rows and of this envelope. A consumer pins on `record_version`.
//! - An unbounded end of the reporting period is reported as `null`, never as a
//!   fabricated date.

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use serde::Serialize;

use crate::cli::TimeWindow;
use crate::store::RECORD_VERSION;

/// The `period` of a report. `null` on either end means "unbounded".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Period {
    pub from: Option<String>,
    pub to: Option<String>,
}

impl Period {
    /// Build a period from a window, rendering bounds as ISO-8601 UTC.
    ///
    /// [`TimeWindow::all`] uses saturating sentinels that are not representable
    /// as timestamps; those become `null` rather than a fake date.
    pub fn from_window(window: TimeWindow) -> Self {
        Self {
            from: window.from().map(iso8601),
            to: window.to().map(iso8601),
        }
    }
}

/// The one timestamp rendering warden publishes. Every ISO-8601 string in a
/// `--json` document — envelope period, note text, row fields — goes through
/// here, so a consumer never sees two precisions in one response.
pub fn iso8601(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The same rendering from epoch millis, for timestamps that are not already
/// bounded into a `DateTime`.
pub fn iso8601_ms(ts_ms: i64) -> Option<String> {
    Utc.timestamp_millis_opt(ts_ms).single().map(iso8601)
}

/// The one shape every `--json` response has (MVP §5).
///
/// `rows` is `Value` rather than a typed row: the row shape is per report and
/// may gain fields freely, so the envelope stays report-agnostic.
#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    pub warden_version: &'static str,
    pub record_version: u32,
    pub report: String,
    pub period: Period,
    pub rows: Vec<serde_json::Value>,
    pub notes: Vec<String>,
}

impl Envelope {
    pub fn new(
        report: impl Into<String>,
        window: TimeWindow,
        rows: Vec<serde_json::Value>,
    ) -> Self {
        Self {
            warden_version: env!("CARGO_PKG_VERSION"),
            record_version: RECORD_VERSION,
            report: report.into(),
            period: Period::from_window(window),
            rows,
            notes: Vec::new(),
        }
    }

    /// Attach the caller-supplied notes (e.g. `"cost figures are estimates"`).
    pub fn with_notes<S: Into<String>>(mut self, notes: impl IntoIterator<Item = S>) -> Self {
        self.notes = notes.into_iter().map(Into::into).collect();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> TimeWindow {
        TimeWindow::new(1_754_006_400_000, 1_754_611_200_000)
    }

    #[test]
    fn envelope_has_the_documented_shape() {
        let env = Envelope::new(
            "projects",
            window(),
            vec![serde_json::json!({"project": "acme"})],
        )
        .with_notes(["cost figures are estimates"]);
        let v = serde_json::to_value(&env).unwrap();

        assert_eq!(v["warden_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["record_version"], RECORD_VERSION);
        assert_eq!(v["report"], "projects");
        assert_eq!(v["period"]["from"], "2025-08-01T00:00:00.000Z");
        assert_eq!(v["period"]["to"], "2025-08-08T00:00:00.000Z");
        assert_eq!(v["rows"][0]["project"], "acme");
        assert_eq!(v["notes"][0], "cost figures are estimates");

        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "notes",
                "period",
                "record_version",
                "report",
                "rows",
                "warden_version"
            ],
            "the envelope has exactly the six documented fields"
        );

        // The wire order is the documented one.
        let wire = serde_json::to_string(&env).unwrap();
        assert!(wire.starts_with(r#"{"warden_version":"#), "{wire}");
    }

    #[test]
    fn unbounded_window_serializes_as_null_not_a_fake_date() {
        let env: Envelope = Envelope::new("summary", TimeWindow::all(), Vec::new());
        let v = serde_json::to_value(&env).unwrap();
        assert!(v["period"]["from"].is_null());
        assert!(v["period"]["to"].is_null());
        assert!(v["rows"].as_array().unwrap().is_empty());
        assert!(v["notes"].as_array().unwrap().is_empty());
    }

    #[test]
    fn notes_default_to_empty_rather_than_absent() {
        let env: Envelope = Envelope::new("models", window(), Vec::new());
        assert!(env.notes.is_empty());
    }
}
