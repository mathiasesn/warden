//! The `warden` binary: a thin dispatcher over the library.

use std::process::ExitCode;

use chrono::Utc;
use clap::Parser;

use warden::cli::{Cli, Command, TimeWindow};
use warden::commands::Env;
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

/// Everything a subcommand needs, resolved once.
struct Context {
    config: Config,
    paths: StorePaths,
    window: TimeWindow,
    project: Option<String>,
    json: bool,
    no_ingest: bool,
    include_sidechain: bool,
}

impl Context {
    /// The borrowed view the reporting commands take.
    fn env(&self) -> Env<'_> {
        Env {
            config: &self.config,
            paths: &self.paths,
            window: self.window,
            project: self.project.as_deref(),
            json: self.json,
            no_ingest: self.no_ingest,
            include_sidechain: self.include_sidechain,
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let ctx = context(&cli)?;
    match &cli.command {
        Command::Ingest => {
            warden::commands::ingest::run(
                &ctx.config,
                &ctx.paths,
                ctx.window,
                ctx.project.as_deref(),
            )?;
            Ok(())
        }
        Command::Doctor => {
            warden::commands::doctor::run(
                &ctx.config,
                &ctx.paths,
                ctx.window,
                ctx.project.as_deref(),
            )?;
            Ok(())
        }
        Command::Report { name } => {
            warden::commands::report::run(&ctx.env(), name)?;
            Ok(())
        }
        Command::Query { group_by } => {
            warden::commands::query::run(&ctx.env(), group_by.as_deref())?;
            Ok(())
        }
        Command::Watch { .. } | Command::Suggest { .. } | Command::Purge { .. } => {
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
        include_sidechain: !cli.no_sidechain,
    })
}
