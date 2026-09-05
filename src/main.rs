//! `device-reporter`: reads clinic devices over USB serial and publishes their
//! results over HTTP and WebSocket.
//!
//! ```text
//! device-reporter                 # serve (default); auto-detects supported devices
//! device-reporter --demo          # serve with a simulated scale, no hardware needed
//! device-reporter list            # show serial ports with USB VID/PID/product
//! device-reporter sniff COM5      # dump whatever a port sends (RealTerm-style)
//! device-reporter set-password --password-file FILE   # reset the settings password
//! device-reporter retry-rejected  # requeue readings the EMR rejected
//! ```
//!
//! Runtime settings (the EMR destination and key, port assignments, scale
//! thresholds, the optional settings password) live in a JSON file and are
//! edited on the web page; the flags and `DR_*` environment variables below
//! only **seed** that file the first time. See `settings.rs`.

mod demo;
mod driver;
mod drivers;
mod fhir;
mod forward;
mod manager;
mod model;
mod serial;
mod settings;
mod sniff;
mod state;
mod storage;
mod web;

use clap::{Args, Parser, Subcommand};
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
    /// Requeue rejected readings after correcting the cause. Stop the service first.
    RetryRejected,
    /// Set or reset the settings password from a private UTF-8 file (forgotten-password recovery).
    /// Stop the service first.
    SetPassword {
        #[arg(long)]
        password_file: std::path::PathBuf,
    },
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

    /// Settings file, edited from the web page. Flags seed only a missing file.
    #[arg(
        long,
        env = "DR_SETTINGS",
        default_value = "device-reporter-settings.json"
    )]
    settings: std::path::PathBuf,

    /// Seed: force a driver onto a port, `--assign /dev/ttyUSB0=healthometer_scale`. Repeatable.
    #[arg(long, env = "DR_ASSIGN", value_delimiter = ',', value_parser = parse_assignment)]
    assign: Vec<(String, String)>,

    /// Seed: driver for /dev/ttyUSB* or /dev/ttyACM* ports that expose no USB descriptors.
    #[arg(long, env = "DR_FALLBACK_DRIVER")]
    fallback_driver: Option<String>,

    /// Seed: browser origins allowed to call the API cross-origin (`*` for any). Repeatable.
    #[arg(long, env = "DR_CORS_ORIGIN", value_delimiter = ',')]
    cors_origin: Vec<String>,

    /// How many observations to keep in memory.
    #[arg(long, env = "DR_HISTORY", default_value_t = 100)]
    history: usize,

    /// Seed: name reported as `host` and used in device IDs. Defaults to the machine hostname.
    #[arg(long, env = "DR_HOST")]
    host: Option<String>,

    /// Seconds between serial port scans (hot-plug detection).
    #[arg(long, env = "DR_SCAN_SECS", default_value_t = 3)]
    scan_secs: u64,

    /// Simulate a scale instead of opening serial ports.
    #[arg(long, env = "DR_DEMO")]
    demo: bool,

    /// Seed: FHIR base URL to forward observations to, e.g. `http://emr:8000/fhir/v5`.
    #[arg(long, env = "DR_FORWARD_URL")]
    forward_url: Option<String>,

    /// Seed: bearer token for the forwarder (a SMART client-credentials access token).
    #[arg(long, env = "DR_FORWARD_TOKEN", hide_env_values = true)]
    forward_token: Option<String>,

    /// Seed: API key for the forwarder, sent as `X-API-Key`.
    #[arg(long, env = "DR_FORWARD_API_KEY", hide_env_values = true)]
    forward_api_key: Option<String>,

    /// Durable queue of observations awaiting forwarding.
    #[arg(
        long,
        env = "DR_QUEUE_FILE",
        default_value = "device-reporter-queue.json"
    )]
    queue_file: std::path::PathBuf,

    /// Seed: scale, milliseconds of silence that end a weigh-in.
    #[arg(long, env = "DR_SCALE_QUIET_MS")]
    scale_quiet_ms: Option<u64>,

    /// Seed: scale, weights below this many kilograms are flagged `below_minimum`.
    #[arg(long, env = "DR_SCALE_MIN_WEIGHT_KG")]
    scale_min_weight_kg: Option<f64>,
}

impl ServeArgs {
    /// The flags as a settings seed: used only when the file does not exist.
    fn seed(&self) -> settings::Settings {
        settings::Settings {
            forward_url: self.forward_url.clone(),
            forward_api_key: self.forward_api_key.clone(),
            forward_token: self.forward_token.clone(),
            cors_origins: self.cors_origin.clone(),
            host: self.host.clone(),
            assignments: self.assign.iter().cloned().collect(),
            fallback_driver: self.fallback_driver.clone(),
            scale_quiet_ms: self.scale_quiet_ms,
            scale_min_weight_kg: self.scale_min_weight_kg,
            password_hash: None,
        }
    }

    /// Open (or seed) the settings file with the driver names the page may reference.
    fn open_settings(&self) -> anyhow::Result<settings::SettingsStore> {
        // Driver kinds are needed to validate settings, and the settings are
        // needed to build the drivers (scale thresholds): resolve the names first.
        let known_drivers = driver::registry(&driver::DriverOptions::default())
            .iter()
            .map(|d| d.kind().to_owned())
            .collect();
        Ok(settings::SettingsStore::open(
            self.settings.clone(),
            &self.seed(),
            known_drivers,
        )?)
    }
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
    for var in ["HOSTNAME", "COMPUTERNAME"] {
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
        Some(Command::RetryRejected) => {
            let outbox = forward::Outbox::open(cli.serve.queue_file)?;
            outbox.retry_rejected()?;
            tracing::info!("rejected readings requeued; restart the service");
            Ok(())
        }
        Some(Command::SetPassword { password_file }) => {
            let password = std::fs::read_to_string(password_file)?;
            let store = cli.serve.open_settings()?;
            store.provision_password(password.trim_end_matches(['\r', '\n']))?;
            tracing::info!("settings password saved; restart the service if it is running");
            Ok(())
        }
        Some(Command::List) => sniff::list(),
        Some(Command::Sniff(args)) => sniff::sniff(&args),
        None => tokio::runtime::Runtime::new()?.block_on(serve(cli.serve)),
    }
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let store = Arc::new(args.open_settings()?);
    let current = store.snapshot();

    let host = current.host.clone().unwrap_or_else(hostname);
    let opts = driver::DriverOptions {
        scale_quiet_ms: current.scale_quiet_ms(),
        scale_min_weight_kg: current.scale_min_weight_kg(),
    };
    let registry = driver::registry(&opts);
    tracing::info!(
        host = %host,
        drivers = ?registry.iter().map(|d| d.kind()).collect::<Vec<_>>(),
        settings = %args.settings.display(),
        queue = %args.queue_file.display(),
        password_set = current.password_hash.is_some(),
        forwarding = current.forward_url.is_some(),
        "device-reporter {} starting",
        env!("CARGO_PKG_VERSION")
    );
    if current.password_hash.is_none() {
        tracing::warn!(
            "no settings password: anyone who can reach the page can change settings (set one on the page)"
        );
    }
    if args.demo && current.forward_url.is_some() {
        tracing::warn!("demo mode with a forward URL: simulated readings will be sent to the EMR");
    }

    let state = Arc::new(state::AppState::new(host.clone(), args.history));
    let outbox = Arc::new(forward::Outbox::open(args.queue_file)?);
    let manager_cfg = manager::ManagerConfig {
        host,
        scan_interval: Duration::from_secs(args.scan_secs.max(1)),
        demo: args.demo,
    };
    tokio::spawn(manager::run(
        manager_cfg,
        registry,
        Arc::clone(&store),
        Arc::clone(&state),
        Some(Arc::clone(&outbox)),
    ));
    tokio::spawn(forward::run(
        Arc::clone(&store),
        outbox,
        Duration::from_secs(20),
        Arc::clone(&state),
    ));

    web::serve(
        web::WebConfig {
            bind: args.bind,
            cors_origins: current.cors_origins,
        },
        web::WebState {
            app: state,
            settings: store,
            limits: Arc::new(web::Limits::default()),
        },
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
        let cli = Cli::try_parse_from(["device-reporter", "set-password", "--password-file", "pw"])
            .unwrap();
        assert!(matches!(cli.command, Some(Command::SetPassword { .. })));
    }

    #[test]
    fn flags_become_the_settings_seed() {
        let cli = Cli::try_parse_from([
            "device-reporter",
            "--forward-url",
            "http://emr:8000/fhir/v5",
            "--assign",
            "COM3=healthometer_scale",
            "--scale-quiet-ms",
            "3000",
        ])
        .unwrap();
        let seed = cli.serve.seed();
        assert_eq!(seed.forward_url.as_deref(), Some("http://emr:8000/fhir/v5"));
        assert_eq!(
            seed.assignments.get("COM3").map(String::as_str),
            Some("healthometer_scale")
        );
        assert_eq!(seed.scale_quiet_ms, Some(3000));
        assert!(
            seed.scale_min_weight_kg.is_none(),
            "unset flags leave the seed empty"
        );
        assert!(
            seed.password_hash.is_none(),
            "a password is never seeded from flags"
        );
    }
}
