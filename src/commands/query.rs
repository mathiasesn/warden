//! `warden query` (MVP §3).

use crate::output::{emit, Report};
use crate::reports::query::{self, DEFAULT_GROUP_BY};
use crate::reports::ReportError;
use crate::store::Scanner;

use super::Env;

/// Roll the window up over the requested dimensions.
pub fn run(env: &Env<'_>, group_by: Option<&str>) -> Result<Report, ReportError> {
    let dims = query::parse_dimensions(group_by.unwrap_or(DEFAULT_GROUP_BY))?;
    env.pre_ingest()?;
    let report = query::build(&Scanner::new(env.paths.clone()), &env.ctx(), &dims)?;
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
    fn defaults_to_grouping_by_project() {
        let (_dir, paths) = store(&[used("a", ms(2026, 8, 4, 8), "acme", "opus", 10, 1)]);
        let config = Config::default();
        let report = run(&env(&paths, &config), None).unwrap();
        assert_eq!(report.name, "query");
        assert_eq!(report.json_rows[0]["project"], "acme");
    }

    #[test]
    fn an_unknown_dimension_fails_before_anything_is_scanned() {
        let (_dir, paths) = store(&[used("a", ms(2026, 8, 4, 8), "acme", "opus", 10, 1)]);
        let config = Config::default();
        let err = run(&env(&paths, &config), Some("project,colour")).unwrap_err();
        assert!(err.to_string().contains("colour"), "{err}");
    }
}
