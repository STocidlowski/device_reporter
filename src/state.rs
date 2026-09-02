//! Shared application state and the event stream every WebSocket client sees.
//!
//! The manager task writes; HTTP handlers read. Locks are `std::sync`
//! because no critical section awaits. Events fan out through a
//! `tokio::sync::broadcast` channel; a client that lags misses old events
//! and resyncs from the snapshot endpoints.

use crate::model::{DeviceInfo, DeviceStatus, Event, Observation, Reading, ServerStatus};
use jiff::Timestamp;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use tokio::sync::broadcast;

/// How many events a slow WebSocket client may fall behind before it starts missing some.
const BROADCAST_CAPACITY: usize = 64;

/// Process-wide state shared by the manager and the web layer.
#[derive(Debug)]
pub struct AppState {
    host: String,
    started_at: Timestamp,
    devices: RwLock<BTreeMap<String, DeviceStatus>>,
    latest: RwLock<HashMap<String, Observation>>,
    history: RwLock<VecDeque<Observation>>,
    history_cap: usize,
    events: broadcast::Sender<Event>,
}

fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

impl AppState {
    /// Fresh state with no devices and no observations.
    #[must_use]
    pub fn new(host: String, history_cap: usize) -> Self {
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            host,
            started_at: Timestamp::now(),
            devices: RwLock::new(BTreeMap::new()),
            latest: RwLock::new(HashMap::new()),
            history: RwLock::new(VecDeque::with_capacity(history_cap)),
            history_cap: history_cap.max(1),
            events,
        }
    }

    /// Snapshot of the process and every device it has seen since start.
    #[must_use]
    pub fn server_status(&self) -> ServerStatus {
        ServerStatus {
            host: self.host.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            started_at: self.started_at,
            devices: self.devices(),
        }
    }

    /// Every device seen since start, sorted by ID.
    #[must_use]
    pub fn devices(&self) -> Vec<DeviceStatus> {
        read(&self.devices).values().cloned().collect()
    }

    /// Most recent observation from one device, or from any device.
    #[must_use]
    pub fn latest(&self, device_id: Option<&str>) -> Option<Observation> {
        let latest = read(&self.latest);
        match device_id {
            Some(id) => latest.get(id).cloned(),
            None => latest.values().max_by_key(|o| o.completed_at).cloned(),
        }
    }

    /// Latest observation per device, for a WebSocket snapshot.
    #[must_use]
    pub fn latest_per_device(&self) -> Vec<Observation> {
        let mut v: Vec<Observation> = read(&self.latest).values().cloned().collect();
        v.sort_by_key(|o| o.completed_at);
        v
    }

    /// Recent observations, newest first, optionally for one device.
    #[must_use]
    pub fn history(&self, device_id: Option<&str>, limit: usize) -> Vec<Observation> {
        read(&self.history)
            .iter()
            .rev()
            .filter(|o| device_id.is_none_or(|id| o.device_id == id))
            .take(limit)
            .cloned()
            .collect()
    }

    /// Subscribe to live events. Subscribe *before* reading a snapshot so nothing is missed.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    fn broadcast(&self, event: Event) {
        // Err only means nobody is listening right now, which is fine.
        let _ = self.events.send(event);
    }

    /// Mutate one device's status; broadcast if anything changed.
    fn update_device(&self, id: &str, f: impl FnOnce(&mut DeviceStatus)) {
        let changed = {
            let mut devices = write(&self.devices);
            let Some(status) = devices.get_mut(id) else {
                tracing::warn!(device = %id, "event for unknown device");
                return;
            };
            let before = status.clone();
            f(status);
            (*status != before).then(|| status.clone())
        };
        if let Some(status) = changed {
            self.broadcast(Event::Device(status));
        }
    }

    /// A connection thread opened the port.
    pub fn device_connected(&self, info: DeviceInfo) {
        let status = {
            let mut devices = write(&self.devices);
            let status = devices
                .entry(info.id.clone())
                .or_insert_with(|| DeviceStatus {
                    info: info.clone(),
                    connected: false,
                    last_error: None,
                    last_data_at: None,
                    active: false,
                });
            status.info = info;
            status.connected = true;
            status.last_error = None;
            status.active = false;
            status.clone()
        };
        self.broadcast(Event::Device(status));
    }

    /// A connection thread ended.
    pub fn device_disconnected(&self, id: &str, reason: String) {
        self.update_device(id, |s| {
            s.connected = false;
            s.active = false;
            s.last_error = Some(reason);
        });
    }

    /// Bytes arrived. Does not broadcast: a device sending once per second
    /// would otherwise flood clients with status events; `reading` carries the time.
    pub fn device_data(&self, id: &str, at: Timestamp) {
        if let Some(s) = write(&self.devices).get_mut(id) {
            s.last_data_at = Some(at);
        }
    }

    /// The device entered or left the middle of a result.
    pub fn device_active(&self, id: &str, active: bool) {
        self.update_device(id, |s| s.active = active);
    }

    /// A provisional value: fan out, do not store.
    pub fn publish_reading(&self, reading: Reading) {
        self.broadcast(Event::Reading(reading));
    }

    /// A completed result: store it and tell everyone.
    pub fn publish_observation(&self, o: Observation) {
        {
            let mut h = write(&self.history);
            if h.len() >= self.history_cap {
                h.pop_front();
            }
            h.push_back(o.clone());
        }
        write(&self.latest).insert(o.device_id.clone(), o.clone());
        self.broadcast(Event::Observation(o));
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
    use crate::model::Component;
    use uuid::Uuid;

    fn info(id: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.to_owned(),
            kind: "k".to_owned(),
            display_name: "K".to_owned(),
            port: "COM1".to_owned(),
        }
    }

    fn obs(device: &str, weight: f64, secs: i64) -> Observation {
        Observation {
            id: Uuid::nil(),
            device_id: device.to_owned(),
            device_kind: "k".to_owned(),
            captured_at: Timestamp::from_second(secs).unwrap(),
            completed_at: Timestamp::from_second(secs).unwrap(),
            subject_hint: None,
            components: vec![Component::quantity("29463-7", "Body weight", weight, "kg")],
            flags: vec![],
            packets: 1,
        }
    }

    #[test]
    fn history_is_capped_newest_first_and_filterable() {
        let s = AppState::new("h".to_owned(), 3);
        s.publish_observation(obs("a", 1.0, 1));
        s.publish_observation(obs("b", 2.0, 2));
        s.publish_observation(obs("a", 3.0, 3));
        s.publish_observation(obs("a", 4.0, 4));
        let all: Vec<f64> = s
            .history(None, 10)
            .iter()
            .map(|o| o.components[0].clone().value)
            .map(|v| match v {
                crate::model::Value::Quantity(q) => q,
                crate::model::Value::Text(_) => unreachable!(),
            })
            .collect();
        assert_eq!(all, vec![4.0, 3.0, 2.0]);
        assert_eq!(s.history(Some("b"), 10).len(), 1);
        assert_eq!(
            s.latest(Some("b")).map(|o| o.completed_at),
            Some(Timestamp::from_second(2).unwrap())
        );
        assert_eq!(
            s.latest(None).map(|o| o.completed_at),
            Some(Timestamp::from_second(4).unwrap())
        );
        assert_eq!(s.latest_per_device().len(), 2);
    }

    #[test]
    fn device_lifecycle_broadcasts_only_changes() {
        let s = AppState::new("h".to_owned(), 3);
        let mut rx = s.subscribe();
        s.device_connected(info("d"));
        s.device_active("d", true);
        s.device_active("d", true); // no change, no event
        s.device_data("d", Timestamp::UNIX_EPOCH); // never broadcasts
        s.device_disconnected("d", "unplugged".to_owned());
        s.device_active("ghost", true); // unknown device is ignored

        let Ok(Event::Device(d)) = rx.try_recv() else {
            panic!("expected connected")
        };
        assert!(d.connected);
        let Ok(Event::Device(d)) = rx.try_recv() else {
            panic!("expected active")
        };
        assert!(d.active);
        let Ok(Event::Device(d)) = rx.try_recv() else {
            panic!("expected disconnected")
        };
        assert!(!d.connected && !d.active);
        assert_eq!(d.last_error.as_deref(), Some("unplugged"));
        assert_eq!(d.last_data_at, Some(Timestamp::UNIX_EPOCH));
        assert!(rx.try_recv().is_err());
        assert_eq!(s.server_status().devices.len(), 1);
    }
}
