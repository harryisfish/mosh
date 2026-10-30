use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use mosh_rs::{
    diagnostics::{self, Role},
    session::SessionActor,
    transport,
};

#[derive(Parser)]
#[command(version, about = "Host a resumable Rust terminal session")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read an authenticated bootstrap request from SSH standard input.
    Bootstrap {
        #[arg(long)]
        diagnostics: bool,
    },
    #[command(hide = true)]
    Worker {
        #[arg(long, hide = true)]
        cleanup_dir: Option<PathBuf>,
        #[arg(long)]
        diagnostics: Option<PathBuf>,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mosh-rs-server: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    match Args::parse().command {
        Command::Bootstrap { diagnostics } => transport::spawn_worker(diagnostics).await,
        Command::Worker {
            cleanup_dir,
            diagnostics: path,
        } => {
            let _cleanup = cleanup_dir.map(transport::WorkerTempDirGuard::new);
            let _diagnostics = path
                .as_ref()
                .map(|path| diagnostics::start(Role::Server, path))
                .transpose()?;
            let request = transport::read_request(std::io::stdin().lock())?;
            let (endpoint, grant) = transport::listen(&request)?;
            let session = SessionActor::new(&request, grant.session.clone())?;
            transport::write_grant(std::io::stdout().lock(), &grant)?;
            session.run(endpoint).await
        }
    }
}
