//! The blocking connection loop shared by every serial driver.
//!
//! Runs on a plain OS thread per device because `serialport` is blocking.
//! It owns the transport and the driver's [`DeviceSession`], turns the
//! session's [`Output`]s into [`DeviceEvent`]s for the manager, and writes
//! any `Output::Send` bytes back to the device. It returns when the port
//! fails; the manager re-spawns it on the next scan that sees the port.

use crate::driver::{DeviceSession, Output};
use crate::model::{DeviceInfo, Observation, Reading};
use jiff::Timestamp;
use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use uuid::Uuid;

/// How long one `read` blocks before returning so the loop can tick the session.
pub const READ_TIMEOUT: Duration = Duration::from_millis(500);
/// After this much silence, ask `still_present` whether the port still exists.
/// Windows can keep returning empty reads from an unplugged `CP210x` instead of an error.
const LIVENESS_CHECK_AFTER: Duration = Duration::from_secs(10);

/// What a connection thread reports to the manager.
#[derive(Debug, Clone)]
pub struct DeviceEvent {
    pub device: DeviceInfo,
    pub kind: DeviceEventKind,
}

#[derive(Debug, Clone)]
pub enum DeviceEventKind {
    Connected,
    Disconnected {
        reason: String,
    },
    /// Bytes arrived (drives `last_data_at`).
    Data {
        at: Timestamp,
    },
    Reading(Reading),
    Observation(Observation),
    Rejected {
        reason: String,
    },
    /// The session's `is_active` changed.
    Active(bool),
}

/// Drive one open connection until it fails.
///
/// Returns the failure reason, or `None` if the manager went away and the
/// thread should simply exit. `still_present` is consulted after prolonged
/// silence; return `true` from it if there is no way to know.
pub fn run_connection<T: Read + Write>(
    device: &DeviceInfo,
    transport: &mut T,
    session: &mut dyn DeviceSession,
    tx: &mpsc::Sender<DeviceEvent>,
    still_present: &dyn Fn() -> bool,
) -> Option<String> {
    let send = |kind: DeviceEventKind| {
        tx.blocking_send(DeviceEvent {
            device: device.clone(),
            kind,
        })
        .is_ok()
    };

    if !send(DeviceEventKind::Connected) {
        return None;
    }
    let mut active = false;
    if let Err(reason) = handle_outputs(device, transport, session.on_connect(), &send) {
        return Some(reason);
    }

    let mut buf = [0u8; 512];
    let mut last_data = Instant::now();
    loop {
        match transport.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                last_data = Instant::now();
                let bytes = buf.get(..n).unwrap_or_default();
                tracing::trace!(device = %device.id, rx = ?String::from_utf8_lossy(bytes), "serial rx");
                let wall = Timestamp::now();
                if !send(DeviceEventKind::Data { at: wall }) {
                    return None;
                }
                let outputs = session.on_bytes(bytes, Instant::now(), wall);
                if let Err(reason) = handle_outputs(device, transport, outputs, &send) {
                    return Some(reason);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                ) => {}
            Err(e) => return Some(e.to_string()),
        }

        let outputs = session.on_tick(Instant::now(), Timestamp::now());
        if let Err(reason) = handle_outputs(device, transport, outputs, &send) {
            return Some(reason);
        }

        let now_active = session.is_active();
        if now_active != active {
            active = now_active;
            if !send(DeviceEventKind::Active(active)) {
                return None;
            }
        }

        if last_data.elapsed() >= LIVENESS_CHECK_AFTER {
            last_data = Instant::now();
            if !still_present() {
                return Some("port no longer present".to_owned());
            }
        }
    }
}

/// Forward outputs. `Err` carries a disconnect reason (a failed write);
/// a closed manager channel is reported as `Err("manager gone")` too, which
/// the caller treats the same way since the process is shutting down.
fn handle_outputs<T: Write>(
    device: &DeviceInfo,
    transport: &mut T,
    outputs: Vec<Output>,
    send: &dyn Fn(DeviceEventKind) -> bool,
) -> Result<(), String> {
    for out in outputs {
        let ok = match out {
            Output::Live {
                subject_hint,
                components,
            } => send(DeviceEventKind::Reading(Reading {
                device_id: device.id.clone(),
                device_kind: device.kind.clone(),
                at: Timestamp::now(),
                subject_hint,
                components,
            })),
            Output::Complete(d) => {
                tracing::info!(device = %device.id, components = d.components.len(), flags = ?d.flags, "observation complete");
                send(DeviceEventKind::Observation(Observation {
                    id: Uuid::new_v4(),
                    device_id: device.id.clone(),
                    device_kind: device.kind.clone(),
                    captured_at: d.captured_at,
                    completed_at: d.completed_at,
                    subject_hint: d.subject_hint,
                    components: d.components,
                    flags: d.flags,
                    packets: d.packets,
                }))
            }
            Output::Rejected(reason) => {
                tracing::debug!(device = %device.id, %reason, "frame rejected");
                send(DeviceEventKind::Rejected { reason })
            }
            Output::Send(bytes) => {
                tracing::trace!(device = %device.id, tx = ?bytes, "serial tx");
                transport
                    .write_all(&bytes)
                    .and_then(|()| transport.flush())
                    .map_err(|e| format!("write failed: {e}"))?;
                true
            }
        };
        if !ok {
            return Err("manager gone".to_owned());
        }
    }
    Ok(())
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
    use crate::driver::ObservationDraft;
    use std::collections::VecDeque;

    /// A transport that hands out scripted chunks, then times out forever.
    struct Scripted {
        chunks: VecDeque<Vec<u8>>,
        written: Vec<u8>,
        timeouts_left: u32,
    }

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(chunk) = self.chunks.pop_front() {
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                return Ok(n);
            }
            if self.timeouts_left == 0 {
                return Err(std::io::Error::other("unplugged"));
            }
            self.timeouts_left -= 1;
            Err(std::io::Error::from(ErrorKind::TimedOut))
        }
    }

    impl Write for Scripted {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Echo-style session: greets on connect, completes on any byte, active for one tick.
    struct Echo {
        active: bool,
    }

    impl DeviceSession for Echo {
        fn on_connect(&mut self) -> Vec<Output> {
            vec![Output::Send(b"HELLO".to_vec())]
        }
        fn on_bytes(&mut self, bytes: &[u8], _: Instant, wall: Timestamp) -> Vec<Output> {
            self.active = true;
            vec![
                Output::Live {
                    subject_hint: None,
                    components: vec![],
                },
                Output::Complete(ObservationDraft {
                    captured_at: wall,
                    completed_at: wall,
                    subject_hint: None,
                    components: vec![],
                    flags: vec![String::from_utf8_lossy(bytes).into_owned()],
                    packets: 1,
                }),
            ]
        }
        fn on_tick(&mut self, _: Instant, _: Timestamp) -> Vec<Output> {
            self.active = false;
            vec![]
        }
        fn is_active(&self) -> bool {
            self.active
        }
    }

    fn device() -> DeviceInfo {
        DeviceInfo {
            id: "d1".to_owned(),
            kind: "echo".to_owned(),
            display_name: "Echo".to_owned(),
            port: "test".to_owned(),
        }
    }

    #[test]
    fn connection_reports_lifecycle_and_writes_outputs() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut transport = Scripted {
            chunks: VecDeque::from([b"abc".to_vec()]),
            written: vec![],
            timeouts_left: 1,
        };
        let mut session = Echo { active: false };

        let reason = run_connection(&device(), &mut transport, &mut session, &tx, &|| true);
        assert_eq!(reason.as_deref(), Some("unplugged"));
        assert_eq!(
            transport.written, b"HELLO",
            "on_connect bytes were written to the device"
        );

        let mut kinds = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            assert_eq!(ev.device.id, "d1");
            kinds.push(ev.kind);
        }
        assert!(matches!(kinds[0], DeviceEventKind::Connected));
        assert!(matches!(kinds[1], DeviceEventKind::Data { .. }));
        assert!(matches!(kinds[2], DeviceEventKind::Reading(_)));
        let DeviceEventKind::Observation(obs) = &kinds[3] else {
            panic!("expected observation, got {:?}", kinds[3])
        };
        assert_eq!(obs.flags, vec!["abc"]);
        assert_eq!(obs.device_kind, "echo");
        // Active flips false on the tick right after the bytes, so no Active(true) is ever seen
        // in this scripted run: the session reports active only between bytes and the next tick.
        assert!(
            kinds
                .iter()
                .all(|k| !matches!(k, DeviceEventKind::Active(true)))
        );
    }

    #[test]
    fn a_dropped_manager_ends_the_loop_quietly() {
        let (tx, rx) = mpsc::channel(32);
        drop(rx);
        let mut transport = Scripted {
            chunks: VecDeque::new(),
            written: vec![],
            timeouts_left: 5,
        };
        let mut session = Echo { active: false };
        assert_eq!(
            run_connection(&device(), &mut transport, &mut session, &tx, &|| true),
            None
        );
    }
}
