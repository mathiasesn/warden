//! `warden report <name>` (MVP §3).
//!
//! Two responsibilities beyond picking a report: run the implicit ingest first
//! (MVP §3: "Runs implicitly before any report unless `--no-ingest`"), and keep
//! its progress off stdout so `--json` stays a single parseable document.

use crate::output::{emit, Report};
use crate::reports::{self, ReportError};
use crate::store::Scanner;

use super::Env;

/// Build and print a named report.
pub fn run(env: &Env<'_>, name: &str) -> Result<Report, ReportError> {
    env.pre_ingest()?;
    let report = reports::run(&Scanner::new(env.paths.clone()), name, &env.ctx())?;
    emit(&report, env.json)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::TimeWindow;
    use crate::config::Config;
    use crate::reports::testkit::*;
    use crate::store::StorePaths;

    fn env<'a>(paths: &'a StorePaths, config: &'a Config) -> Env<'a> {
        Env {
            config,
            paths,
            window: TimeWindow::all(),
            project: None,
            json: false,
            no_ingest: true,
            include_sidechain: true,
        }
    }

    #[test]
    fn every_named_report_builds_against_a_populated_store() {
        let (_dir, paths) = store(&[
            with_tools(
                priced(used("a", ms(2026, 8, 4, 8), "acme", "opus", 100, 20), 1.0),
                &[("Read", "src/lib.rs")],
            ),
            used("b", ms(2026, 8, 4, 9), "acme", "sonnet", 10, 2),
        ]);
        let config = Config::default();
        let mut env = env(&paths, &config);
        // `compare` is the one report that needs a bounded period.
        env.window = TimeWindow::new(ms(2026, 8, 4, 0), ms(2026, 8, 5, 0));

        for name in reports::NAMES {
            let report = run(&env, name).unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(report.name, name);
            assert!(
                report
                    .notes
                    .iter()
                    .any(|n| n == "cost figures are estimates"),
                "{name} lost the estimate legend"
            );
            let envelope = serde_json::to_value(report.envelope()).unwrap();
            assert_eq!(envelope["report"], name);
            assert!(envelope["rows"].is_array(), "{name}");
        }
    }

    #[test]
    fn an_unknown_report_names_the_valid_ones() {
        let (_dir, paths) = store(&[]);
        let config = Config::default();
        let err = run(&env(&paths, &config), "spend").unwrap_err();
        assert!(err.to_string().contains("summary"), "{err}");
    }

    #[test]
    fn an_empty_store_reports_nothing_rather_than_failing() {
        let (_dir, paths) = store(&[]);
        let config = Config::default();
        let report = run(&env(&paths, &config), "projects").unwrap();
        assert!(report.json_rows.is_empty());
        assert!(report.table.is_empty());
    }
}
