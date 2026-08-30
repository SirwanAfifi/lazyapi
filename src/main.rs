mod model;
mod server;
mod session;
mod spec;
mod ui;

use std::{path::PathBuf, sync::mpsc};

use clap::Parser;
use server::CaptureServer;

#[derive(Debug, Parser)]
#[command(
    name = "lazyapi",
    version,
    about = "Capture, validate, mock, and replay OpenAPI traffic in a terminal UI"
)]
struct Cli {
    /// Path to an OpenAPI 3 document (JSON or YAML).
    #[arg(long, value_name = "FILE")]
    spec: PathBuf,

    /// Address for the capture server.
    #[arg(long, default_value = "127.0.0.1:3000", value_name = "HOST:PORT")]
    listen: String,

    /// Optional upstream base URL to proxy requests to.
    #[arg(long, value_name = "URL")]
    target: Option<String>,

    /// Load a previously saved LazyAPI JSON or JSONL session.
    #[arg(long, value_name = "FILE")]
    load: Option<PathBuf>,

    /// Stream captured traffic to .json, .jsonl/.ndjson, or .har.
    #[arg(long, value_name = "FILE")]
    save: Option<PathBuf>,

    /// Capture sensitive headers and fields without masking them.
    #[arg(long)]
    no_redact: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("lazyapi: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let endpoints = spec::load_spec(&cli.spec)?;
    let recorder = cli
        .save
        .as_deref()
        .map(session::SessionRecorder::new)
        .transpose()?;

    let (output_tx, output_rx) = mpsc::sync_channel(256);
    // Captured bodies can be several MiB, so keep the lossless handoff queue modest and
    // apply backpressure to completed request workers instead of buffering a large burst.
    let (logs_tx, logs_rx) = mpsc::sync_channel(64);
    let mut server = CaptureServer::new(cli.listen, cli.target, output_tx, logs_tx)?
        .with_endpoints(endpoints.clone())
        .with_redaction(!cli.no_redact);
    let result = ui::run(
        endpoints,
        cli.load.as_deref(),
        recorder,
        &mut server,
        output_rx,
        logs_rx,
    );
    server.stop();
    result?;
    Ok(())
}
