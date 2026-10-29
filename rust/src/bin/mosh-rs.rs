use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};
use mosh_rs::client::{self, ClientOptions};
use mosh_rs::diagnostics::{self, Role};
use mosh_rs::display::{Graphics, PredictionPolicy};
use mosh_rs::input::LocalTerminal;
use mosh_rs::model::{BootstrapRequest, PROTOCOL_VERSION};
use mosh_rs::transport::{self, ClientIdentity};

#[derive(Clone, Copy, ValueEnum)]
enum GraphicsArg {
    Auto,
    Kitty,
    Sixel,
    None,
}

#[derive(Clone, Copy, ValueEnum)]
enum PredictionArg {
    Adaptive,
    Never,
    Conservative,
}

#[derive(Parser)]
#[command(
    version,
    about = "Connect to a resumable Rust terminal over authenticated UDP"
)]
struct Args {
    #[arg(required_unless_present = "local")]
    target: Option<String>,
    /// Start the server on this machine.
    #[arg(long)]
    local: bool,
    /// Server executable path, resolved on the remote host for SSH sessions.
    #[arg(long)]
    server: Option<String>,
    #[arg(long)]
    ssh_port: Option<u16>,
    /// Server UDP port. The operating system selects a port when omitted.
    #[arg(long)]
    port: Option<u16>,
    /// Server listening IP address.
    #[arg(long)]
    bind: Option<String>,
    /// Lifetime of a disconnected session.
    #[arg(long, default_value_t = 86400)]
    idle_seconds: u64,
    #[arg(long, value_enum, default_value_t = PredictionArg::Adaptive)]
    predict: PredictionArg,
    #[arg(long, value_enum, default_value_t = GraphicsArg::Auto)]
    graphics: GraphicsArg,
    /// Allow remote programs to request the local clipboard.
    #[arg(long)]
    clipboard_read: bool,
    /// Record bounded diagnostics on both endpoints without terminal contents.
    #[arg(long)]
    diagnostics: bool,
    /// Program and literal arguments. Omit to start a login shell.
    #[arg(last = true)]
    command: Vec<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(error) => {
            if can_report() {
                eprintln!("mosh-rs: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<i32> {
    let args = Args::parse();
    ensure!(
        !(args.local && args.target.is_some()),
        "--local does not take an SSH target"
    );
    let _diagnostics = if args.diagnostics {
        let path = diagnostics::create_path(Role::Client)?;
        let monitor = diagnostics::start(Role::Client, &path)?;
        eprintln!("mosh-rs: diagnostics: {}", path.display());
        Some(monitor)
    } else {
        None
    };
    let size = LocalTerminal::size().context("a local terminal is required")?;
    let identity = ClientIdentity::generate()?;
    let request = BootstrapRequest {
        version: PROTOCOL_VERSION,
        client_cert: identity.certificate(),
        size,
        command: args.command,
        bind: args.bind,
        port: args.port,
        idle_seconds: args.idle_seconds,
        clipboard_read: args.clipboard_read,
    };
    let grant = if args.local {
        let path = args.server.as_ref().map(PathBuf::from).unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap_or_else(|_| PathBuf::from("mosh-rs"))
                .with_file_name("mosh-rs-server")
        });
        transport::bootstrap_local(&identity, Path::new(&path), request).await?
    } else {
        transport::bootstrap_ssh(
            &identity,
            args.target.as_deref().context("SSH target is required")?,
            args.server.as_deref().unwrap_or("mosh-rs-server"),
            request,
            args.ssh_port,
        )
        .await?
    };
    if can_report() {
        eprintln!("mosh-rs: UDP {}:{}", grant.address, grant.port);
    }
    let endpoint = identity.endpoint(&grant)?;
    let graphics = match args.graphics {
        GraphicsArg::Auto => Graphics::detect(),
        GraphicsArg::Kitty => Graphics::Kitty,
        GraphicsArg::Sixel => Graphics::Sixel,
        GraphicsArg::None => Graphics::None,
    };
    let prediction = match args.predict {
        PredictionArg::Adaptive => PredictionPolicy::Adaptive,
        PredictionArg::Never => PredictionPolicy::Never,
        PredictionArg::Conservative => PredictionPolicy::Conservative,
    };
    client::run(
        endpoint,
        grant,
        ClientOptions {
            graphics,
            prediction,
            clipboard_read: args.clipboard_read,
        },
    )
    .await
}

fn can_report() -> bool {
    !std::io::stderr().is_terminal() || LocalTerminal::foreground().unwrap_or(true)
}
