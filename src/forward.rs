//! Durable observation delivery to the EMR's FHIR server.
//!
//! Completed readings enter a private **outbox** on disk (see [`Outbox`])
//! before anything else happens to them. The manager writes them there
//! directly, not through the WebSocket event stream, so a burst of readings
//! during a slow request or a retry sleep cannot be lost, and a storage
//! failure is reported rather than logged and forgotten. The delivery loop
//! then drains the outbox in order.
//!
//! Each reading becomes one or more FHIR Observations (see [`crate::fhir`])
//! sent as `PUT /Observation/{id}` with a client-chosen id, which FHIR servers
//! treat as update-as-create. That makes the first delivery idempotent, but PUT
//! replaces, so a naive retry after a *lost response* could overwrite a result
//! a clinician had already accepted into a chart. The rule that prevents it:
//! an attempt is recorded on disk **before** the request is sent, and any item
//! that already carries an attempt is first checked with `GET`; if the server
//! has it, it is treated as delivered and never written again.
//!
//! Server rejections that will never succeed (400, 422) move to a rejected
//! list in the same file for local review (`device-reporter retry-rejected`
//! requeues them). Everything else, including a bad credential, is retried
//! with backoff. Response bodies are never logged.
//!
//! The destination and credentials come from the [`SettingsStore`] on every
//! attempt, so a change on the settings page applies to the next request.

use crate::fhir::to_fhir;
use crate::settings::SettingsStore;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Most items (pending plus rejected) kept on disk; beyond this ingestion fails loudly.
const QUEUE_CAP: usize = 2_000;
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// How often to re-check the settings while forwarding is unconfigured but the queue has items.
const UNCONFIGURED_POLL: Duration = Duration::from_secs(5);
/// Largest queue file the loader will read.
const QUEUE_FILE_LIMIT: usize = 16 * 1024 * 1024;

/// The destination as configured right now.
#[derive(Debug, Clone)]
struct Target {
    base_url: String,
    api_key: Option<String>,
    token: Option<String>,
    timeout: Duration,
}

fn target(settings: &SettingsStore, timeout: Duration) -> Option<Target> {
    let s = settings.snapshot();
    let base_url = s.forward_url?;
    Some(Target {
        base_url,
        api_key: s.forward_api_key,
        token: s.forward_token,
        timeout,
    })
}

/// One pending write.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueueItem {
    /// The resource `id`: the PUT target and the de-duplication key.
    pub key: String,
    pub body: Json,
    /// Requests started for this item, recorded on disk *before* each send.
    #[serde(default)]
    pub attempts: u32,
}

/// Pending and rejected items, one file so a rejection can never be lost by
/// a failed secondary write.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Queue {
    items: VecDeque<QueueItem>,
    #[serde(default)]
    rejected: VecDeque<QueueItem>,
}

impl Queue {
    fn load(path: &Path) -> std::io::Result<Self> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Disk {
            Current(Queue),
            Legacy(VecDeque<QueueItem>),
        }
        use std::io::Read;
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.take(QUEUE_FILE_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > QUEUE_FILE_LIMIT {
            return Err(std::io::Error::other(
                "queue exceeds the 16 MiB safety limit",
            ));
        }
        let disk: Disk = serde_json::from_slice(&bytes)
            .map_err(|_| std::io::Error::other("queue is invalid; restore it before starting"))?;
        Ok(match disk {
            Disk::Current(q) => q,
            Disk::Legacy(items) => Self {
                items,
                rejected: VecDeque::new(),
            },
        })
    }

    fn save(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        if bytes.len() > QUEUE_FILE_LIMIT {
            return Err(std::io::Error::other(
                "queue exceeds the 16 MiB safety limit",
            ));
        }
        crate::storage::write_private(path, &bytes)
    }

    fn push(&mut self, item: QueueItem) -> std::io::Result<()> {
        if self
            .items
            .iter()
            .chain(self.rejected.iter())
            .any(|i| i.key == item.key)
        {
            return Ok(());
        }
        if self.items.len().saturating_add(self.rejected.len()) >= QUEUE_CAP {
            return Err(std::io::Error::other(
                "outbox full; delivery or rejected readings require attention",
            ));
        }
        self.items.push_back(item);
        Ok(())
    }

    fn front(&self) -> Option<&QueueItem> {
        self.items.front()
    }

    fn len(&self) -> usize {
        self.items.len()
    }
}

/// Shared durable outbox. File writes and memory publication are one
/// transaction under a mutex; network I/O never holds that lock.
pub struct Outbox {
    path: PathBuf,
    queue: std::sync::Mutex<Queue>,
    changed: tokio::sync::Notify,
    _lock: std::fs::File,
}

impl Outbox {
    /// Open (and lock) the queue file, migrating an older single-list file.
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let lock = crate::storage::lock_exclusive(&path.with_extension("queue.lock"))?;
        let queue = Queue::load(&path)?;
        queue.save(&path)?;
        Ok(Self {
            path,
            queue: std::sync::Mutex::new(queue),
            changed: tokio::sync::Notify::new(),
            _lock: lock,
        })
    }

    fn mutate(
        &self,
        change: impl FnOnce(&mut Queue) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let mut current = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = current.clone();
        change(&mut next)?;
        next.save(&self.path)?;
        *current = next;
        self.changed.notify_one();
        Ok(())
    }

    /// Persist a completed reading. Fails (loudly, to the caller) when the
    /// disk is unwritable or the outbox is full.
    pub fn ingest(&self, observation: &crate::model::Observation) -> std::io::Result<()> {
        let bodies = to_fhir(observation);
        self.mutate(|queue| {
            for body in bodies {
                let key = queue_key(&body)
                    .ok_or_else(|| std::io::Error::other("observation has no id"))?;
                queue.push(QueueItem {
                    key,
                    body,
                    attempts: 0,
                })?;
            }
            Ok(())
        })
    }

    fn front(&self) -> Option<QueueItem> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .front()
            .cloned()
    }

    /// `(pending, rejected)`.
    pub fn counts(&self) -> (usize, usize) {
        let q = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (q.len(), q.rejected.len())
    }

    /// Record that a request for the front item is about to be sent. Written
    /// before the send so a crash mid-request leaves a trace that makes the
    /// next attempt check the server first.
    fn note_attempt(&self, key: &str) -> std::io::Result<()> {
        self.mutate(|queue| {
            if let Some(front) = queue.items.front_mut()
                && front.key == key
            {
                front.attempts = front.attempts.saturating_add(1);
            }
            Ok(())
        })
    }

    fn finish(&self, key: &str, rejected: bool) -> std::io::Result<()> {
        self.mutate(|queue| {
            if queue.front().is_some_and(|item| item.key == key)
                && let Some(item) = queue.items.pop_front()
                && rejected
            {
                queue.rejected.push_back(item);
            }
            Ok(())
        })
    }

    /// Move every rejected item back to the pending list.
    pub fn retry_rejected(&self) -> std::io::Result<()> {
        self.mutate(|queue| {
            let rejected: Vec<QueueItem> = queue.rejected.drain(..).collect();
            queue.items.extend(rejected);
            Ok(())
        })
    }
}

/// Outcome of one delivery attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum PostOutcome {
    /// The server has this observation.
    Accepted,
    /// Network error, timeout, credential problem or server error: keep and retry later.
    Retry(String),
    /// 400 or 422: the server will never take this payload.
    Rejected(String),
}

/// The resource `id`, which is both the PUT target and the de-duplication key.
fn queue_key(body: &Json) -> Option<String> {
    body.get("id")?.as_str().map(str::to_owned)
}

/// Exponential backoff from the attempt count, capped.
#[must_use]
pub fn backoff(attempts: u32) -> Duration {
    let factor = 2u32.saturating_pow(attempts.min(6));
    (BACKOFF_MIN.saturating_mul(factor)).min(BACKOFF_MAX)
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .proxy(None)
        .http_status_as_error(false)
        .build()
        .into()
}

fn auth_headers(t: &Target) -> Vec<(&'static str, String)> {
    let mut headers = Vec::new();
    if let Some(token) = &t.token {
        headers.push(("Authorization", format!("Bearer {token}")));
    }
    if let Some(key) = &t.api_key {
        headers.push(("X-API-Key", key.clone()));
    }
    headers
}

/// Does the server already hold this Observation? Its body is never read: it
/// may by now carry a patient association that belongs in the chart, not here.
enum Presence {
    Present,
    Absent,
    Unknown(String),
}

fn exists(t: &Target, id: &str) -> Presence {
    let url = format!("{}/Observation/{id}", t.base_url.trim_end_matches('/'));
    let mut req = agent(t.timeout)
        .get(&url)
        .header("Accept", "application/fhir+json");
    for (name, value) in auth_headers(t) {
        req = req.header(name, &value);
    }
    match req.call() {
        Ok(resp) => match resp.status().as_u16() {
            200 => Presence::Present,
            404 | 410 => Presence::Absent,
            status => Presence::Unknown(format!("HTTP {status} checking for an earlier delivery")),
        },
        Err(_) => Presence::Unknown("transport failure checking for an earlier delivery".into()),
    }
}

fn put(t: &Target, item: &QueueItem) -> PostOutcome {
    let url = format!(
        "{}/Observation/{}",
        t.base_url.trim_end_matches('/'),
        item.key
    );
    let mut req = agent(t.timeout)
        .put(&url)
        .header("Prefer", "return=minimal")
        .header("Content-Type", "application/fhir+json")
        .header("Accept", "application/fhir+json");
    for (name, value) in auth_headers(t) {
        req = req.header(name, &value);
    }
    // Serialise up front so the request carries a Content-Length rather than
    // a chunked body: simpler for the receiver and for the tests to read.
    let body = match serde_json::to_vec(&item.body) {
        Ok(b) => b,
        Err(e) => return PostOutcome::Rejected(format!("unserialisable body: {e}")),
    };
    match req.send(&body[..]) {
        Ok(resp) => match resp.status().as_u16() {
            200..=299 => PostOutcome::Accepted,
            // Only payload validation failures are permanent. Credentials,
            // routing, redirects and server errors need the operator, not a drop.
            400 | 422 => PostOutcome::Rejected(format!("HTTP {}", resp.status().as_u16())),
            401 | 403 => PostOutcome::Retry("the EMR rejected the credentials".into()),
            status => PostOutcome::Retry(format!("HTTP {status}")),
        },
        Err(_) => PostOutcome::Retry("transport failure".to_owned()),
    }
}

/// Deliver one item. An item with an earlier attempt on record is checked
/// with `GET` first so a lost response never turns into an overwrite.
fn deliver(t: &Target, item: &QueueItem) -> PostOutcome {
    if item.attempts > 1 {
        match exists(t, &item.key) {
            Presence::Present => return PostOutcome::Accepted,
            Presence::Absent => {}
            Presence::Unknown(reason) => return PostOutcome::Retry(reason),
        }
    }
    put(t, item)
}

/// Try the configured destination with the configured credentials: a `GET`
/// of an Observation id that cannot exist. `404` proves the URL is a FHIR
/// base and the credentials are accepted; `401`/`403` means they are not.
/// No clinical data is requested or returned.
pub fn probe(settings: &SettingsStore, timeout: Duration) -> Result<u16, String> {
    let t = target(settings, timeout).ok_or_else(|| "no forward URL configured".to_owned())?;
    let url = format!(
        "{}/Observation/probe-{}",
        t.base_url.trim_end_matches('/'),
        uuid::Uuid::new_v4()
    );
    let mut req = agent(t.timeout)
        .get(&url)
        .header("Accept", "application/fhir+json");
    for (name, value) in auth_headers(&t) {
        req = req.header(name, &value);
    }
    req.call()
        .map(|resp| resp.status().as_u16())
        .map_err(|_| "could not reach the EMR".to_owned())
}

/// Drain the outbox to the server, forever.
pub async fn run(
    settings: Arc<SettingsStore>,
    outbox: Arc<Outbox>,
    timeout: Duration,
    state: Arc<AppState>,
) {
    let mut consecutive_failures = 0u32;
    loop {
        let (pending, rejected) = outbox.counts();
        let Some(item) = outbox.front() else {
            state.forward_status(
                pending,
                rejected,
                if rejected == 0 {
                    "ready"
                } else {
                    "rejected readings need review"
                },
            );
            // Bounded wait also picks up settings changes.
            let _ = tokio::time::timeout(Duration::from_secs(2), outbox.changed.notified()).await;
            continue;
        };
        let Some(t) = target(&settings, timeout) else {
            state.forward_status(
                pending,
                rejected,
                "paused: set the EMR destination in Settings",
            );
            tokio::time::sleep(UNCONFIGURED_POLL).await;
            continue;
        };
        if t.api_key.is_none() && t.token.is_none() {
            state.forward_status(pending, rejected, "sending without a credential");
        } else {
            state.forward_status(pending, rejected, "sending");
        }

        // The attempt is on disk before the request leaves the machine.
        let key = item.key.clone();
        let q = Arc::clone(&outbox);
        let noted = tokio::task::spawn_blocking(move || q.note_attempt(&key)).await;
        if !matches!(noted, Ok(Ok(()))) {
            state.forward_status(
                pending,
                rejected,
                "storage failure: attempt not recorded; waiting",
            );
            tokio::time::sleep(BACKOFF_MAX).await;
            continue;
        }
        let Some(item) = outbox.front() else { continue };

        let send_item = item.clone();
        let outcome = tokio::task::spawn_blocking(move || deliver(&t, &send_item))
            .await
            .unwrap_or_else(|_| PostOutcome::Retry("delivery task failed".into()));

        match outcome {
            PostOutcome::Accepted | PostOutcome::Rejected(_) => {
                let rejected_item = matches!(outcome, PostOutcome::Rejected(_));
                if let PostOutcome::Rejected(reason) = &outcome {
                    tracing::error!(key = %item.key, %reason, "server rejected observation; kept for review");
                } else {
                    tracing::info!(key = %item.key, attempts = item.attempts, "observation forwarded");
                }
                let q = Arc::clone(&outbox);
                let result =
                    tokio::task::spawn_blocking(move || q.finish(&item.key, rejected_item)).await;
                if matches!(result, Ok(Ok(()))) {
                    consecutive_failures = 0;
                } else {
                    // The server has it; the outbox still lists it. The next
                    // pass will GET, see it, and finish without writing again.
                    state.forward_status(
                        pending,
                        rejected,
                        "storage failure: receipt not saved; will re-check",
                    );
                    tokio::time::sleep(BACKOFF_MAX).await;
                }
            }
            PostOutcome::Retry(reason) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                state.forward_status(pending, rejected, &reason);
                if consecutive_failures == 1 || consecutive_failures.is_multiple_of(10) {
                    tracing::warn!(pending, rejected, %reason, "delivery paused; readings retained");
                } else {
                    tracing::debug!(pending, rejected, %reason, "delivery paused; readings retained");
                }
                tokio::time::sleep(backoff(consecutive_failures)).await;
            }
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
    use std::io::{Read, Write};

    fn item(key: &str) -> QueueItem {
        QueueItem {
            key: key.to_owned(),
            body: serde_json::json!({ "resourceType": "Observation", "id": key, "status": "preliminary" }),
            attempts: 0,
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dr-outbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A one-shot HTTP server answering `responses` in order, one connection each,
    /// returning the raw requests it saw.
    fn response_server(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let task = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let count = socket.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|s| s.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                socket.write_all(response.as_bytes()).unwrap();
                seen.push(String::from_utf8_lossy(&bytes).into_owned());
            }
            seen
        });
        (format!("http://{address}"), task)
    }

    fn reply(status: &str) -> String {
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }

    fn test_target(base_url: String) -> Target {
        Target {
            base_url,
            api_key: Some("test-secret".into()),
            token: None,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn first_delivery_is_a_put_by_id_with_the_key() {
        let (url, task) = response_server(vec![reply("200 OK")]);
        let mut it = item("obs-1");
        it.attempts = 1;
        assert_eq!(deliver(&test_target(url), &it), PostOutcome::Accepted);
        let seen = task.join().unwrap();
        assert!(
            seen[0].starts_with("PUT /Observation/obs-1 HTTP/1.1"),
            "{}",
            seen[0]
        );
        assert!(
            seen[0].contains("x-api-key: test-secret")
                || seen[0].contains("X-API-Key: test-secret")
        );
        assert!(seen[0].contains("\"resourceType\":\"Observation\""));
    }

    #[test]
    fn a_retry_checks_the_server_first_and_never_overwrites_an_existing_result() {
        // Second attempt: the server already has it (a clinician may have accepted it).
        let (url, task) = response_server(vec![reply("200 OK")]);
        let mut it = item("obs-2");
        it.attempts = 2;
        assert_eq!(deliver(&test_target(url), &it), PostOutcome::Accepted);
        let seen = task.join().unwrap();
        assert_eq!(seen.len(), 1, "no PUT followed the GET");
        assert!(
            seen[0].starts_with("GET /Observation/obs-2 HTTP/1.1"),
            "{}",
            seen[0]
        );

        // Second attempt, server never got it: GET 404, then PUT.
        let (url, task) = response_server(vec![reply("404 Not Found"), reply("200 OK")]);
        assert_eq!(deliver(&test_target(url), &it), PostOutcome::Accepted);
        let seen = task.join().unwrap();
        assert!(seen[0].starts_with("GET "));
        assert!(seen[1].starts_with("PUT /Observation/obs-2 "));
    }

    #[test]
    fn outcomes_map_status_codes() {
        let mut it = item("obs-3");
        it.attempts = 1;
        for (status, expected) in [
            ("400 Bad Request", PostOutcome::Rejected("HTTP 400".into())),
            (
                "422 Unprocessable",
                PostOutcome::Rejected("HTTP 422".into()),
            ),
            (
                "401 Unauthorized",
                PostOutcome::Retry("the EMR rejected the credentials".into()),
            ),
            ("500 Server Error", PostOutcome::Retry("HTTP 500".into())),
        ] {
            let (url, task) = response_server(vec![reply(status)]);
            assert_eq!(deliver(&test_target(url), &it), expected);
            task.join().unwrap();
        }
        // A redirect is never followed and never counts as delivered.
        let (url, task) = response_server(vec![
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/login\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_owned(),
        ]);
        assert!(matches!(
            deliver(&test_target(url), &it),
            PostOutcome::Retry(_)
        ));
        task.join().unwrap();
        // Unreachable.
        assert!(matches!(
            deliver(&test_target("http://127.0.0.1:1".into()), &it),
            PostOutcome::Retry(_)
        ));
    }

    #[test]
    fn outbox_ingests_persists_and_records_attempts() {
        let dir = temp_dir();
        let path = dir.join("queue.json");
        let outbox = Outbox::open(path.clone()).unwrap();
        let o = crate::model::Observation {
            id: uuid::Uuid::nil(),
            device_id: "d".to_owned(),
            device_kind: crate::drivers::healthometer::KIND.to_owned(),
            captured_at: jiff::Timestamp::UNIX_EPOCH,
            completed_at: jiff::Timestamp::UNIX_EPOCH,
            subject_hint: None,
            components: vec![crate::model::Component::quantity(
                "29463-7",
                "Body weight",
                1.0,
                "kg",
            )],
            flags: vec![],
            packets: 1,
        };
        outbox.ingest(&o).unwrap();
        outbox.ingest(&o).unwrap();
        assert_eq!(outbox.counts(), (1, 0), "duplicate readings de-duplicate");
        let key = outbox.front().unwrap().key;
        assert_eq!(key, "00000000-0000-0000-0000-000000000000-29463-7");
        outbox.note_attempt(&key).unwrap();
        drop(outbox);

        let reopened = Outbox::open(path.clone()).unwrap();
        assert_eq!(
            reopened.front().unwrap().attempts,
            1,
            "the attempt survived a restart"
        );
        reopened.finish(&key, true).unwrap();
        assert_eq!(reopened.counts(), (0, 1));
        reopened.retry_rejected().unwrap();
        assert_eq!(reopened.counts(), (1, 0));
        reopened.finish(&key, false).unwrap();
        assert_eq!(reopened.counts(), (0, 0));
        drop(reopened);

        // A second process cannot open the same outbox.
        let _first = Outbox::open(path.clone()).unwrap();
        assert!(Outbox::open(path).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn legacy_queue_files_migrate_and_bad_files_fail_closed() {
        let dir = temp_dir();
        let path = dir.join("queue.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&vec![item("a"), item("b")]).unwrap(),
        )
        .unwrap();
        let outbox = Outbox::open(path.clone()).unwrap();
        assert_eq!(outbox.counts(), (2, 0));
        drop(outbox);
        std::fs::write(&path, b"not json").unwrap();
        assert!(
            Outbox::open(path).is_err(),
            "an unreadable queue must not start empty"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn queue_is_capped_with_an_error_not_a_drop() {
        let mut q = Queue::default();
        for i in 0..QUEUE_CAP {
            q.push(item(&i.to_string())).unwrap();
        }
        assert!(q.push(item("one-too-many")).is_err());
        assert_eq!(q.len(), QUEUE_CAP);
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(0), Duration::from_secs(2));
        assert_eq!(backoff(1), Duration::from_secs(4));
        assert_eq!(backoff(3), Duration::from_secs(16));
        assert_eq!(backoff(20), BACKOFF_MAX);
    }
}
