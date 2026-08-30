mod model;
mod server;
mod spec;
mod ui;

use std::{path::PathBuf, sync::mpsc};

use clap::Parser;
use server::CaptureServer;

#[derive(Debug, Parser)]
#[command(
    name = "lazyapi",
    version,
    about = "Inspect OpenAPI traffic in a keyboard-first terminal UI"
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

    let (output_tx, output_rx) = mpsc::sync_channel(256);
    let (logs_tx, logs_rx) = mpsc::sync_channel(256);
    let mut server = CaptureServer::new(cli.listen, cli.target, output_tx, logs_tx)?;
    server.start()?;

    let result = ui::run(endpoints, &mut server, output_rx, logs_rx);
    server.stop();
    result.map_err(Into::into)
}
