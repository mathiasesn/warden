//! The `warden` binary: a thin dispatcher over the library.

use std::process::ExitCode;

use chrono::Utc;
use clap::Parser;

use warden::cli::{Cli, Command, TimeWindow};
use warden::config::Config;
use warden::store::StorePaths;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("warden: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Everything a subcommand needs, resolved once. Later phases consume these
/// fields; nothing is wired up yet.
#[allow(dead_code)]
struct Context {
    config: Config,
    paths: StorePaths,
    window: TimeWindow,
    project: Option<String>,
    json: bool,
    no_ingest: bool,
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let _ctx = context(&cli)?;
    match &cli.command {
        Command::Ingest
        | Command::Report { .. }
        | Command::Query { .. }
        | Command::Watch { .. }
        | Command::Suggest { .. }
        | Command::Doctor
        | Command::Purge { .. } => {
            Err(format!("`{}` is not implemented yet", cli.command.name()).into())
        }
    }
}

/// Resolve config and store location. The config file lives inside the store,
/// so the flag (or the default root) locates it first; `general.data_dir` can
/// then redirect the store itself.
fn context(cli: &Cli) -> Result<Context, Box<dyn std::error::Error>> {
    let bootstrap = StorePaths::resolve(cli.data_dir.as_deref(), None)?;
    let config = Config::load_from_dir(bootstrap.root())?;
    let paths = StorePaths::resolve(cli.data_dir.as_deref(), config.general.data_dir.as_deref())?;

    let window = match &cli.since {
        Some(spec) => TimeWindow::parse_since(spec, Utc::now())?,
        None => TimeWindow::all(),
    };

    Ok(Context {
        config,
        paths,
        window,
        project: cli.project.clone(),
        json: cli.json,
        no_ingest: cli.no_ingest,
    })
}
