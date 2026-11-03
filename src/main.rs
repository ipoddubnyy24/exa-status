//! EXA status: a local dashboard that answers, for each customer group, each
//! release and each initiative, where we are, what is blocking, and whether we
//! are on track for the proposed dates.
//!
//! `exa-status serve [--port N]`  the dashboard (default command)
//! `exa-status refresh-once [--print] [--out FILE] [--data-dir DIR] [--commitments FILE]`
//!                                one refresh into data/latest.json
//! `exa-status settings`          the port and certificate directory, for scripts

mod build;
mod commitments;
mod config;
mod confluence;
mod fetch;
mod gerrit;
mod github;
mod http;
mod jira;
mod model;
mod reporter;
mod rules;
mod server;
mod store;
mod tls;
mod worker;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use tracing::error;

use crate::config::Config;

/// Local EXA status dashboard.
#[derive(Debug, Parser)]
#[command(name = "exa-status")]
struct Cli {
    /// What to run; `serve` when left out.
    #[command(subcommand)]
    command: Option<Command>,
}

/// What to run.
#[derive(Debug, PartialEq, Subcommand)]
enum Command {
    /// Serve the dashboard over HTTPS.
    Serve {
        /// Port to listen on instead of the configured one.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Run one refresh and write the snapshot.
    RefreshOnce {
        /// Print the numbers when done.
        #[arg(long)]
        print: bool,
        /// Write the snapshot here instead of data/latest.json, and add no
        /// history line. The caches are still updated.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Data directory to use instead of the configured one (the server
        /// passes its own, so a config edit cannot split the two).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Commitments file to use instead of the configured one.
        #[arg(long)]
        commitments: Option<PathBuf>,
    },
    /// Print the configured port and the absolute certificate directory as
    /// `name=value` lines, for scripts/exa_status_daemon.sh.
    Settings,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
    let cli = Cli::parse();
    match run(cli.command.unwrap_or(Command::Serve { port: None })) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<()> {
    let mut cfg = Config::load()?;
    tls::install_crypto_provider()?;
    match command {
        Command::Serve { port } => {
            let cert = tls::ensure_server_cert(&cfg.cert_dir, Utc::now().date_naive())?;
            let port = port.unwrap_or(cfg.port);
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("starting the async runtime")?
                .block_on(server::serve(cfg, port, cert))
        }
        Command::RefreshOnce {
            print,
            out,
            data_dir,
            commitments,
        } => {
            if let Some(dir) = data_dir {
                cfg.data_dir = dir;
            }
            if let Some(file) = commitments {
                cfg.commitments_file = file;
            }
            worker::refresh_once(&cfg, out.as_deref(), print)
        }
        Command::Settings => {
            let cert_dir = std::path::absolute(&cfg.cert_dir)
                .context("resolving the certificate directory")?;
            println!("port={}\ncert_dir={}", cfg.port, cert_dir.display());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use clap::error::ErrorKind;
    use test_case::test_case;

    fn parse(args: &[&str]) -> Result<Option<Command>, clap::Error> {
        Cli::try_parse_from(std::iter::once("exa-status").chain(args.iter().copied()))
            .map(|c| c.command)
    }

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test_case(&[], None ; "no arguments serves")]
    #[test_case(&["serve", "--port", "9900"], Some(Command::Serve { port: Some(9900) }) ; "port override")]
    #[test_case(&["refresh-once", "--print"], Some(Command::RefreshOnce { print: true, out: None, data_dir: None, commitments: None }) ; "print")]
    #[test_case(&["refresh-once", "--out", "x.json"], Some(Command::RefreshOnce { print: false, out: Some(PathBuf::from("x.json")), data_dir: None, commitments: None }) ; "out")]
    #[test_case(
        &["refresh-once", "--data-dir", "/srv/data", "--commitments", "/srv/c.json"],
        Some(Command::RefreshOnce { print: false, out: None, data_dir: Some(PathBuf::from("/srv/data")), commitments: Some(PathBuf::from("/srv/c.json")) }) ;
        "paths from the server"
    )]
    #[test_case(&["settings"], Some(Command::Settings) ; "settings for scripts")]
    fn parses_commands(args: &[&str], expected: Option<Command>) {
        assert_eq!(parse(args).unwrap(), expected);
    }

    #[test_case(&["serve", "--port"], ErrorKind::InvalidValue ; "missing port")]
    #[test_case(&["serve", "--port", "http"], ErrorKind::ValueValidation ; "bad port")]
    #[test_case(&["serve", "--prot", "9900"], ErrorKind::UnknownArgument ; "misspelt flag")]
    #[test_case(&["refresh-once", "--out", "--print"], ErrorKind::InvalidValue ; "flag taken as a value")]
    #[test_case(&["deploy"], ErrorKind::InvalidSubcommand ; "unknown command")]
    fn rejects_bad_arguments(args: &[&str], kind: ErrorKind) {
        assert_eq!(parse(args).unwrap_err().kind(), kind);
    }
}
