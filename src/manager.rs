//! Device manager: finds ports, pairs them with drivers, runs one connection
//! thread per device, and applies their events to shared state.
//!
//! Every `scan_interval` the manager enumerates serial ports. A port that is
//! not already running gets a driver by, in order:
//!
//! 1. an explicit `--assign PORT=KIND`,
//! 2. the first driver whose `matches` accepts the port's USB descriptors,
//! 3. `--fallback-driver KIND` for `/dev/ttyUSB*`/`/dev/ttyACM*` ports that
//!    expose no USB descriptors at all (rare; sysfs normally provides them).
//!
//! When a connection thread ends (unplug, read error) the port is forgotten
//! and picked up again on a later scan if it reappears. Hot-plug therefore
//! costs nothing beyond the scan.

use crate::demo::DemoTransport;
use crate::driver::{Driver, PortCandidate, find_driver};
use crate::model::DeviceInfo;
use crate::serial::{DeviceEvent, DeviceEventKind, READ_TIMEOUT, run_connection};
use crate::state::AppState;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc;

/// How the manager is configured from the CLI.
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    /// This machine's name; used to build device IDs when USB has no serial number.
    pub host: String,
    /// Port name to driver kind, from `--assign`.
    pub assignments: HashMap<String, String>,
    /// Driver for descriptor-less ports, from `--fallback-driver`.
    pub fallback_kind: Option<String>,
    pub scan_interval: Duration,
    /// Run a simulated scale instead of touching hardware.
    pub demo: bool,
}

/// Which driver to use for a port, or why none.
#[derive(Debug, PartialEq, Eq)]
pub enum Pairing {
    Assigned(String),
    Matched(String),
    Fallback(String),
    Unassigned,
}

/// Pure pairing logic, separated so it can be tested without serial hardware.
pub fn pair(cfg: &ManagerConfig, registry: &[Arc<dyn Driver>], port: &PortCandidate) -> Pairing {
    if let Some(kind) = cfg
        .assignments
        .iter()
        .find(|(p, _)| p.eq_ignore_ascii_case(&port.name))
        .map(|(_, k)| k)
    {
        return if let Some(d) = find_driver(registry, kind) {
            Pairing::Assigned(d.kind().to_owned())
        } else {
            tracing::warn!(port = %port.name, %kind, "--assign names an unknown driver; ignoring the port");
            Pairing::Unassigned
        };
    }
    if port.has_usb_info() {
        if let Some(d) = registry.iter().find(|d| d.matches(port)) {
            return Pairing::Matched(d.kind().to_owned());
        }
        return Pairing::Unassigned;
    }
    // No descriptors: only guess for USB-looking nodes, never for on-board UARTs.
    if (port.name.starts_with("/dev/ttyUSB") || port.name.starts_with("/dev/ttyACM"))
        && let Some(kind) = &cfg.fallback_kind
        && let Some(d) = find_driver(registry, kind)
    {
        return Pairing::Fallback(d.kind().to_owned());
    }
    Pairing::Unassigned
}

/// Stable device ID: `{host}-{usb serial}` when the OS exposes a serial number,
/// else `{host}-{port}`. The host prefix matters: `CP210x` bridges ship with the
/// generic serial `0001`, so the bare serial would collide across clinic rooms.
#[must_use]
pub fn device_id(host: &str, port: &PortCandidate) -> String {
    let suffix = match port
        .serial_number
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(sn) => sn.to_owned(),
        None => port
            .name
            .trim_start_matches('/')
            .replace(['/', '\\', ' '], "-"),
    };
    format!("{host}-{suffix}")
}

/// [`device_id`], made unique against IDs already in use on this host. Two
/// `CP210x` bridges both report serial `0001`; the second one gets its port
/// appended so the two devices never share an identity.
#[must_use]
pub fn unique_device_id<'a>(
    host: &str,
    port: &PortCandidate,
    taken: impl Iterator<Item = &'a String>,
) -> String {
    let id = device_id(host, port);
    let mut taken = taken;
    if taken.any(|t| *t == id) {
        let suffix = port
            .name
            .trim_start_matches('/')
            .replace(['/', '\\', ' '], "-");
        tracing::warn!(%id, port = %port.name, "duplicate USB serial number; appending port to device id");
        return format!("{id}-{suffix}");
    }
    id
}

/// Run forever: scan, spawn, apply events. Exits only when the runtime shuts down.
pub async fn run(cfg: ManagerConfig, registry: Vec<Arc<dyn Driver>>, state: Arc<AppState>) {
    let (tx, mut rx) = mpsc::channel::<DeviceEvent>(256);
    // Port name -> device id, for every port that has a live connection thread.
    let mut running: HashMap<String, String> = HashMap::new();
    let mut scan = tokio::time::interval(cfg.scan_interval);
    let mut unpaired_logged: HashSet<String> = HashSet::new();
    let mut last_failure: HashMap<String, String> = HashMap::new();

    if cfg.demo {
        spawn_demo(&cfg, &registry, &tx, &mut running);
    }

    loop {
        tokio::select! {
            _ = scan.tick() => {
                if !cfg.demo {
                    scan_and_spawn(&cfg, &registry, &tx, &mut running, &mut unpaired_logged, &last_failure);
                }
            }
            event = rx.recv() => {
                let Some(event) = event else { break };
                match &event.kind {
                    DeviceEventKind::Disconnected { reason } => {
                        running.remove(&event.device.port);
                        // A port that is busy or absent fails identically every scan;
                        // say so once at WARN and then only at DEBUG.
                        let repeat = last_failure.get(&event.device.port) == Some(reason);
                        if repeat {
                            tracing::debug!(device = %event.device.id, port = %event.device.port, %reason, "device still unavailable");
                        } else {
                            tracing::warn!(device = %event.device.id, port = %event.device.port, %reason, "device disconnected");
                            last_failure.insert(event.device.port.clone(), reason.clone());
                        }
                    }
                    DeviceEventKind::Connected => {
                        last_failure.remove(&event.device.port);
                    }
                    _ => {}
                }
                apply(&state, event);
            }
        }
    }
}

fn scan_and_spawn(
    cfg: &ManagerConfig,
    registry: &[Arc<dyn Driver>],
    tx: &mpsc::Sender<DeviceEvent>,
    running: &mut HashMap<String, String>,
    unpaired_logged: &mut HashSet<String>,
    retrying: &HashMap<String, String>,
) {
    let ports = match serialport::available_ports() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "could not enumerate serial ports");
            return;
        }
    };
    let present: HashSet<String> = ports.iter().map(|p| p.port_name.clone()).collect();
    unpaired_logged.retain(|p| present.contains(p));

    for info in &ports {
        let port = PortCandidate::from(info);
        if running.contains_key(&port.name) {
            continue;
        }
        let kind = match pair(cfg, registry, &port) {
            Pairing::Assigned(k) | Pairing::Matched(k) | Pairing::Fallback(k) => k,
            Pairing::Unassigned => {
                if unpaired_logged.insert(port.name.clone()) {
                    tracing::info!(
                        port = %port.name,
                        vid = port.vid.map(|v| format!("{v:04x}")).unwrap_or_default(),
                        pid = port.pid.map(|v| format!("{v:04x}")).unwrap_or_default(),
                        product = port.product.clone().unwrap_or_default(),
                        "serial port with no matching driver (use --assign PORT=KIND or `sniff` to investigate)"
                    );
                }
                continue;
            }
        };
        let Some(driver) = find_driver(registry, &kind) else {
            continue;
        };
        let device = DeviceInfo {
            id: unique_device_id(&cfg.host, &port, running.values()),
            kind: driver.kind().to_owned(),
            display_name: driver.display_name().to_owned(),
            port: port.name.clone(),
        };
        if retrying.contains_key(&port.name) {
            tracing::debug!(device = %device.id, port = %device.port, driver = %device.kind, "retrying device");
        } else {
            tracing::info!(device = %device.id, port = %device.port, driver = %device.kind, "opening device");
        }
        running.insert(port.name.clone(), device.id.clone());
        spawn_serial(device, Arc::clone(driver), tx.clone());
    }
}

fn spawn_serial(device: DeviceInfo, driver: Arc<dyn Driver>, tx: mpsc::Sender<DeviceEvent>) {
    let spawned = thread::Builder::new()
        .name(format!("dev-{}", device.port))
        .spawn(move || {
            let settings = driver.serial_settings();
            let opened = serialport::new(&device.port, settings.baud)
                .data_bits(settings.data_bits)
                .parity(settings.parity)
                .stop_bits(settings.stop_bits)
                .timeout(READ_TIMEOUT)
                .open();
            let reason = match opened {
                Ok(mut port) => {
                    let mut session = driver.open_session();
                    let name = device.port.clone();
                    let still_present = move || match serialport::available_ports() {
                        Ok(ports) => ports
                            .iter()
                            .any(|p| p.port_name.eq_ignore_ascii_case(&name)),
                        Err(_) => true, // enumeration broken; do not tear down a working link
                    };
                    run_connection(&device, &mut port, session.as_mut(), &tx, &still_present)
                }
                Err(e) => Some(format!("open failed: {e}")),
            };
            if let Some(reason) = reason {
                let _ = tx.blocking_send(DeviceEvent {
                    device,
                    kind: DeviceEventKind::Disconnected { reason },
                });
            }
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "could not spawn device thread");
    }
}

fn spawn_demo(
    cfg: &ManagerConfig,
    registry: &[Arc<dyn Driver>],
    tx: &mpsc::Sender<DeviceEvent>,
    running: &mut HashMap<String, String>,
) {
    let Some(driver) = find_driver(registry, crate::drivers::healthometer::KIND) else {
        return;
    };
    let driver = Arc::clone(driver);
    let device = DeviceInfo {
        id: format!("{}-demo", cfg.host),
        kind: driver.kind().to_owned(),
        display_name: format!("{} (demo)", driver.display_name()),
        port: "demo".to_owned(),
    };
    tracing::info!(device = %device.id, "demo mode: simulating a scale; no serial ports will be opened");
    running.insert(device.port.clone(), device.id.clone());
    let tx = tx.clone();
    let spawned = thread::Builder::new()
        .name("dev-demo".to_owned())
        .spawn(move || {
            let mut transport = DemoTransport::new();
            let mut session = driver.open_session();
            let reason = run_connection(&device, &mut transport, session.as_mut(), &tx, &|| true);
            if let Some(reason) = reason {
                let _ = tx.blocking_send(DeviceEvent {
                    device,
                    kind: DeviceEventKind::Disconnected { reason },
                });
            }
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "could not spawn demo thread");
    }
}

fn apply(state: &AppState, event: DeviceEvent) {
    let id = event.device.id.clone();
    match event.kind {
        DeviceEventKind::Connected => state.device_connected(event.device),
        DeviceEventKind::Disconnected { reason } => {
            state.device_disconnected(&event.device, reason);
        }
        DeviceEventKind::Data { at } => state.device_data(&id, at),
        DeviceEventKind::Active(active) => state.device_active(&id, active),
        DeviceEventKind::Reading(r) => state.publish_reading(r),
        DeviceEventKind::Observation(o) => state.publish_observation(o),
        DeviceEventKind::Rejected { reason } => {
            tracing::debug!(device = %id, %reason, "frame rejected");
        }
    }
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
    use crate::driver::{DriverOptions, registry};
    use crate::drivers::healthometer::{CP210X_PID, CP210X_VID, KIND};

    fn cfg(assign: &[(&str, &str)], fallback: Option<&str>) -> ManagerConfig {
        ManagerConfig {
            host: "pi".to_owned(),
            assignments: assign
                .iter()
                .map(|(p, k)| ((*p).to_owned(), (*k).to_owned()))
                .collect(),
            fallback_kind: fallback.map(str::to_owned),
            scan_interval: Duration::from_secs(3),
            demo: false,
        }
    }

    fn port(name: &str, usb: Option<(u16, u16)>) -> PortCandidate {
        PortCandidate {
            name: name.to_owned(),
            vid: usb.map(|u| u.0),
            pid: usb.map(|u| u.1),
            serial_number: None,
            manufacturer: None,
            product: None,
        }
    }

    #[test]
    fn descriptors_match_the_scale_driver() {
        let reg = registry(&DriverOptions::default());
        let p = port("COM7", Some((CP210X_VID, CP210X_PID)));
        assert_eq!(
            pair(&cfg(&[], None), &reg, &p),
            Pairing::Matched(KIND.to_owned())
        );
    }

    #[test]
    fn unknown_usb_devices_stay_unassigned_even_with_a_fallback() {
        let reg = registry(&DriverOptions::default());
        let p = port("COM1", Some((0x067B, 0x2303)));
        assert_eq!(pair(&cfg(&[], Some(KIND)), &reg, &p), Pairing::Unassigned);
    }

    #[test]
    fn explicit_assignment_wins_and_is_case_insensitive() {
        let reg = registry(&DriverOptions::default());
        let p = port("com1", Some((0x067B, 0x2303)));
        assert_eq!(
            pair(&cfg(&[("COM1", KIND)], None), &reg, &p),
            Pairing::Assigned(KIND.to_owned())
        );
        assert_eq!(
            pair(&cfg(&[("COM1", "bogus")], None), &reg, &p),
            Pairing::Unassigned
        );
    }

    #[test]
    fn descriptorless_tty_usb_uses_fallback_but_onboard_uart_never_does() {
        let reg = registry(&DriverOptions::default());
        assert_eq!(
            pair(&cfg(&[], Some(KIND)), &reg, &port("/dev/ttyUSB0", None)),
            Pairing::Fallback(KIND.to_owned())
        );
        assert_eq!(
            pair(&cfg(&[], Some(KIND)), &reg, &port("/dev/ttyAMA0", None)),
            Pairing::Unassigned
        );
        assert_eq!(
            pair(&cfg(&[], None), &reg, &port("/dev/ttyUSB0", None)),
            Pairing::Unassigned
        );
    }

    #[test]
    fn device_ids_prefer_usb_serial_numbers() {
        let mut p = port("/dev/ttyUSB0", Some((1, 2)));
        assert_eq!(device_id("pi", &p), "pi-dev-ttyUSB0");
        p.serial_number = Some(" 0001 ".to_owned());
        assert_eq!(
            device_id("pi", &p),
            "pi-0001",
            "generic CP210x serial must be host-prefixed"
        );
        assert_eq!(device_id("pc", &port("COM3", None)), "pc-COM3");

        // Two CP210x scales on one Pi both report serial 0001.
        let taken = ["pi-0001".to_owned()];
        let mut q = port("/dev/ttyUSB1", Some((1, 2)));
        q.serial_number = Some("0001".to_owned());
        assert_eq!(
            unique_device_id("pi", &q, taken.iter()),
            "pi-0001-dev-ttyUSB1"
        );
        assert_eq!(unique_device_id("pi", &q, std::iter::empty()), "pi-0001");
    }
}
