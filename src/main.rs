//! `device-reporter`: reads clinic devices over USB serial and publishes their
//! results over HTTP and WebSocket.
//!
//! ```text
//! device-reporter                 # serve (default); auto-detects supported devices
//! device-reporter --demo          # serve with a simulated scale, no hardware needed
//! device-reporter list            # show serial ports with USB VID/PID/product
//! device-reporter sniff COM5      # dump whatever a port sends (RealTerm-style)
//! ```
//!
//! Every flag also reads an environment variable (see `--help`), which is how
//! the systemd unit on the Pi configures it.

mod demo;
mod driver;
mod drivers;
mod manager;
mod model;
mod serial;
mod sniff;
mod state;
mod web;

use clap::{Args, Parser, Subcommand};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "device-reporter", version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    serve: ServeArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List serial ports with their USB descriptors.
    List,
    /// Dump everything a port sends, as hex and text, until Ctrl-C.
    Sniff(sniff::SniffArgs),
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Address to listen on. Keep it loopback when nginx fronts the service.
    #[arg(long, env = "DR_BIND", default_value = "127.0.0.1:8080")]
    bind: SocketAddr,

    /// Force a driver onto a port: `--assign /dev/ttyUSB0=healthometer_scale`. Repeatable.
    #[arg(long, env = "DR_ASSIGN", value_delimiter = ',', value_parser = parse_assignment)]
    assign: Vec<(String, String)>,

    /// Driver for /dev/ttyUSB* or /dev/ttyACM* ports that expose no USB descriptors.
    /// Rarely needed; known devices are recognised by USB vendor/product ID.
    #[arg(long, env = "DR_FALLBACK_DRIVER")]
    fallback_driver: Option<String>,

    /// Browser origins allowed to call the API cross-origin. `*` for any. Repeatable.
    #[arg(long, env = "DR_CORS_ORIGIN", value_delimiter = ',')]
    cors_origin: Vec<String>,

    /// How many observations to keep in memory.
    #[arg(long, env = "DR_HISTORY", default_value_t = 100)]
    history: usize,

    /// Name reported as `host` and used in device IDs. Defaults to the machine hostname.
    #[arg(long, env = "DR_HOST")]
    host: Option<String>,

    /// Seconds between serial port scans (hot-plug detection).
    #[arg(long, env = "DR_SCAN_SECS", default_value_t = 3)]
    scan_secs: u64,

    /// Simulate a scale instead of opening serial ports.
    #[arg(long, env = "DR_DEMO")]
    demo: bool,

    /// Scale: milliseconds of silence that end a weigh-in.
    #[arg(long, env = "DR_SCALE_QUIET_MS", default_value_t = 2500)]
    scale_quiet_ms: u64,

    /// Scale: weights below this many kilograms are flagged `below_minimum`.
    #[arg(long, env = "DR_SCALE_MIN_WEIGHT_KG", default_value_t = 1.0)]
    scale_min_weight_kg: f64,
}

fn parse_assignment(s: &str) -> Result<(String, String), String> {
    let (port, kind) = s
        .split_once('=')
        .ok_or_else(|| format!("expected PORT=DRIVER, got {s:?}"))?;
    if port.is_empty() || kind.is_empty() {
        return Err(format!("expected PORT=DRIVER, got {s:?}"));
    }
    Ok((port.to_owned(), kind.to_owned()))
}

fn hostname() -> String {
    for var in ["DR_HOST", "HOSTNAME", "COMPUTERNAME"] {
        if let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            return v.trim().to_owned();
        }
    }
    if let Ok(v) = std::fs::read_to_string("/etc/hostname")
        && !v.trim().is_empty()
    {
        return v.trim().to_owned();
    }
    "device-reporter".to_owned()
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Command::List) => sniff::list(),
        Some(Command::Sniff(args)) => sniff::sniff(&args),
        None => tokio::runtime::Runtime::new()?.block_on(serve(cli.serve)),
    }
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let host = args.host.clone().unwrap_or_else(hostname);
    let opts = driver::DriverOptions {
        scale_quiet_ms: args.scale_quiet_ms,
        scale_min_weight_kg: args.scale_min_weight_kg,
    };
    let registry = driver::registry(&opts);
    tracing::info!(
        host = %host,
        drivers = ?registry.iter().map(|d| d.kind()).collect::<Vec<_>>(),
        "device-reporter {} starting",
        env!("CARGO_PKG_VERSION")
    );

    let state = Arc::new(state::AppState::new(host.clone(), args.history));
    let manager_cfg = manager::ManagerConfig {
        host,
        assignments: args.assign.into_iter().collect::<HashMap<_, _>>(),
        fallback_kind: args.fallback_driver,
        scan_interval: Duration::from_secs(args.scan_secs.max(1)),
        demo: args.demo,
    };
    tokio::spawn(manager::run(manager_cfg, registry, Arc::clone(&state)));

    web::serve(
        web::WebConfig {
            bind: args.bind,
            cors_origins: args.cors_origin,
        },
        state,
    )
    .await
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::assert_is_empty,
    clippy::similar_names
)]
mod tests {
    use super::*;

    #[test]
    fn assignments_parse() {
        assert_eq!(
            parse_assignment("COM3=healthometer_scale").unwrap(),
            ("COM3".to_owned(), "healthometer_scale".to_owned())
        );
        assert!(parse_assignment("COM3").is_err());
        assert!(parse_assignment("=x").is_err());
    }

    #[test]
    fn cli_defaults_to_serving() {
        let cli = Cli::try_parse_from(["device-reporter", "--demo"]).unwrap();
        assert!(cli.command.is_none());
        assert!(cli.serve.demo);
        assert_eq!(cli.serve.bind.port(), 8080);
        let cli =
            Cli::try_parse_from(["device-reporter", "sniff", "COM5", "--baud", "115200"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Sniff(_))));
    }
}
