//! Runtime settings: a JSON file the service owns, editable from the web page.
//!
//! Provisioning a Pi used to mean `ssh`, a root-only environment file and a
//! restart. Now the page has a Settings section. The rules:
//!
//! * **The file wins; flags seed.** `--forward-url`, `DR_FORWARD_API_KEY` and
//!   friends are used only to create the file the first time. Once the file
//!   exists it is the sole source of truth, so an explicitly cleared value
//!   stays cleared across restarts.
//! * **Optional password.** With no password set, anyone who can reach the
//!   page can change settings, and the page says so. Once one is set, every
//!   change must carry it. There is no login session: the password travels
//!   with each save, which also means there is no cookie for a hostile page
//!   to ride (no CSRF).
//! * **Only a hash is stored** (Argon2id). Forgot it? `ssh` in and run
//!   `device-reporter set-password --password-file …` with the service
//!   stopped, or delete the `password_hash` line from the file.
//! * **One global lockout.** Wrong passwords double the delay each time
//!   (1 s, 2 s, 4 s, ...) up to two hours; a correct password resets it.
//!   Behind nginx every request looks like the same client and there is one
//!   admin, so per-client accounting would only add a way to get it wrong.
//! * **Secrets never come back out.** The API key is reported as its last
//!   four characters; the bearer token as set/unset. Changing the destination
//!   clears both, so a saved credential can never be redirected to another
//!   server.
//! * **A damaged file stops the service** rather than silently starting with
//!   no password and no destination; the operator restores or deletes it.
//!
//! Which settings take effect live and which need a restart is reported per
//! save (`restart_required`), because the scan loop and forwarder read the
//! store on every use but the driver registry, CORS layer and host name are
//! built once at startup.

use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

/// Longest lockout after repeated wrong passwords (two hours).
#[allow(clippy::duration_suboptimal_units)]
pub const LOCKOUT_MAX: Duration = Duration::from_secs(2 * 60 * 60);
/// Shortest password accepted when setting one.
pub const MIN_PASSWORD_LEN: usize = 8;
/// Longest password accepted, in bytes; keeps Argon2 work bounded.
pub const MAX_PASSWORD_BYTES: usize = 1024;
/// Bounds for the scale's quiet timeout, in milliseconds.
pub const SCALE_QUIET_MS_RANGE: (u64, u64) = (500, 60_000);

/// Everything the page can edit, as stored on disk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// FHIR base, e.g. `http://emr:8000/fhir/v5`. `None` disables forwarding.
    pub forward_url: Option<String>,
    /// Sent as `X-API-Key`.
    pub forward_api_key: Option<String>,
    /// Sent as `Authorization: Bearer`.
    pub forward_token: Option<String>,
    /// Browser origins allowed to call the API cross-origin. Restart to apply.
    pub cors_origins: Vec<String>,
    /// Name reported as `host` and used in device IDs. Restart to apply.
    pub host: Option<String>,
    /// Port name to driver kind, forcing a pairing.
    pub assignments: BTreeMap<String, String>,
    /// Driver for USB-looking ports with no descriptors.
    pub fallback_driver: Option<String>,
    /// Scale: silence that ends a weigh-in. Restart to apply.
    pub scale_quiet_ms: Option<u64>,
    /// Scale: below this many kilograms a result is flagged. Restart to apply.
    pub scale_min_weight_kg: Option<f64>,
    /// Argon2id PHC string. Never seeded from flags.
    pub password_hash: Option<String>,
}

impl Settings {
    /// Effective scale quiet timeout.
    #[must_use]
    pub fn scale_quiet_ms(&self) -> u64 {
        self.scale_quiet_ms.unwrap_or(2_500)
    }

    /// Effective scale minimum weight.
    #[must_use]
    pub fn scale_min_weight_kg(&self) -> f64 {
        self.scale_min_weight_kg.unwrap_or(1.0)
    }

    /// Settings that only take effect at startup.
    fn restart_relevant(&self) -> (Option<&String>, &[String], Option<u64>, Option<f64>) {
        (
            self.host.as_ref(),
            &self.cors_origins,
            self.scale_quiet_ms,
            self.scale_min_weight_kg,
        )
    }
}

/// A change from the page. `None` keeps the current value; `Some("")` clears it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SettingsPatch {
    pub forward_url: Option<String>,
    pub forward_api_key: Option<String>,
    pub forward_token: Option<String>,
    pub cors_origins: Option<Vec<String>>,
    pub host: Option<String>,
    pub assignments: Option<BTreeMap<String, String>>,
    pub fallback_driver: Option<String>,
    pub scale_quiet_ms: Option<u64>,
    pub scale_min_weight_kg: Option<f64>,
}

/// Three states on purpose: outer `None` keeps the current value, `Some(None)` clears it.
#[allow(clippy::option_option)]
fn clean(value: Option<String>) -> Option<Option<String>> {
    value.map(|v| {
        let v = v.trim().to_owned();
        if v.is_empty() { None } else { Some(v) }
    })
}

/// What the page is allowed to see.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RedactedSettings {
    pub forward_url: Option<String>,
    /// Last four characters of the key, or `None`.
    pub forward_api_key_hint: Option<String>,
    pub forward_token_set: bool,
    pub cors_origins: Vec<String>,
    pub host: Option<String>,
    pub assignments: BTreeMap<String, String>,
    pub fallback_driver: Option<String>,
    pub scale_quiet_ms: u64,
    pub scale_min_weight_kg: f64,
    pub password_set: bool,
    /// Seconds until wrong-password attempts are accepted again.
    pub locked_for_secs: Option<u64>,
    /// A saved change needs a service restart to take effect.
    pub restart_required: bool,
    pub settings_path: String,
    pub known_drivers: Vec<String>,
}

/// Why a change was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsError {
    /// Another password check or save is in progress (one at a time keeps Argon2 bounded).
    Busy,
    /// Too many wrong passwords; try again after this long.
    Locked(Duration),
    WrongPassword,
    /// A password is set and none was supplied.
    PasswordRequired,
    Invalid(String),
    Io(String),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => write!(f, "another settings operation is in progress; try again"),
            Self::Locked(d) => write!(
                f,
                "locked for {} s after repeated wrong passwords",
                secs_ceil(*d)
            ),
            Self::WrongPassword => write!(f, "wrong password"),
            Self::PasswordRequired => write!(f, "the settings password is required"),
            Self::Invalid(m) => write!(f, "{m}"),
            Self::Io(m) => write!(f, "could not save settings: {m}"),
        }
    }
}

impl std::error::Error for SettingsError {}

/// Wrong-password accounting. Pure, so the timing is testable.
#[derive(Debug, Default)]
struct Lockout {
    failures: u32,
    locked_until: Option<Instant>,
}

impl Lockout {
    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.locked_until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|d| !d.is_zero())
    }

    fn failed(&mut self, now: Instant) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let delay =
            Duration::from_secs(1u64 << self.failures.saturating_sub(1).min(13)).min(LOCKOUT_MAX);
        self.locked_until = now.checked_add(delay);
        delay
    }

    fn succeeded(&mut self) {
        self.failures = 0;
        self.locked_until = None;
    }
}

/// Whole seconds, rounded up, so a lock never reads as `0 s`.
fn secs_ceil(d: Duration) -> u64 {
    d.as_secs().saturating_add(u64::from(d.subsec_nanos() > 0))
}

/// Argon2id tuned for a Raspberry Pi Zero: about a second per hash, 16 MiB.
fn hasher() -> Argon2<'static> {
    let params = Params::new(16 * 1024, 2, 1, None).unwrap_or_default();
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

fn hash_password(password: &str) -> Result<String, SettingsError> {
    let salt = SaltString::generate(&mut OsRng);
    hasher()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| SettingsError::Io(format!("hashing failed: {e}")))
}

fn verify_password(password: &str, phc: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|parsed| {
        hasher()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// A FHIR base URL: `http` or `https`, host present, no credentials, query or
/// fragment. Plain HTTP is fine on the clinic tailnet, where `WireGuard`
/// already encrypts the hop.
fn validate_forward_url(url: &str) -> Result<(), SettingsError> {
    let invalid = || {
        SettingsError::Invalid(
            "forward URL must be an http:// or https:// FHIR base without credentials, query or fragment"
                .into(),
        )
    };
    let uri: axum::http::Uri = url.parse().map_err(|_| invalid())?;
    let authority = uri.authority().ok_or_else(invalid)?;
    if url.contains(['@', '#', '?']) || authority.host().is_empty() {
        return Err(invalid());
    }
    match uri.scheme_str() {
        Some("http" | "https") => Ok(()),
        _ => Err(invalid()),
    }
}

/// The settings file plus the in-memory lockout, shared by the web layer,
/// the forwarder and the device manager.
#[derive(Debug)]
pub struct SettingsStore {
    path: PathBuf,
    settings: RwLock<Settings>,
    lockout: Mutex<Lockout>,
    restart_required: Mutex<bool>,
    known_drivers: Vec<String>,
    /// One password check or save at a time: Argon2 is deliberately slow and
    /// a Pi Zero has one core to give it.
    transaction: Mutex<()>,
    _lock: std::fs::File,
}

impl SettingsStore {
    /// Load the file, or create it from `seed` when it does not exist.
    ///
    /// A file that exists but cannot be read or parsed is an error: starting
    /// with defaults would silently drop the password and the destination.
    pub fn open(
        path: PathBuf,
        seed: &Settings,
        known_drivers: Vec<String>,
    ) -> Result<Self, SettingsError> {
        let lock = crate::storage::lock_exclusive(&path.with_extension("settings.lock"))
            .map_err(|e| SettingsError::Io(e.to_string()))?;
        let (settings, fresh) = match std::fs::read(&path) {
            Ok(bytes) => (
                serde_json::from_slice::<Settings>(&bytes).map_err(|_| {
                    SettingsError::Invalid(
                        "settings file is invalid; restore it or delete it to start over".into(),
                    )
                })?,
                false,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (seed.clone(), true),
            Err(e) => return Err(SettingsError::Io(e.to_string())),
        };
        if let Some(hash) = &settings.password_hash {
            let parsed = PasswordHash::new(hash)
                .map_err(|_| SettingsError::Invalid("invalid password hash".into()))?;
            if parsed.algorithm.as_str() != "argon2id" {
                return Err(SettingsError::Invalid(
                    "password hash must use Argon2id".into(),
                ));
            }
        }
        let store = Self {
            path,
            settings: RwLock::new(settings),
            lockout: Mutex::new(Lockout::default()),
            restart_required: Mutex::new(false),
            known_drivers,
            transaction: Mutex::new(()),
            _lock: lock,
        };
        store.validate(&store.snapshot())?;
        if fresh {
            store.persist(&store.snapshot())?;
        }
        Ok(store)
    }

    fn begin(&self) -> Result<MutexGuard<'_, ()>, SettingsError> {
        self.transaction.try_lock().map_err(|_| SettingsError::Busy)
    }

    /// Local CLI: set or reset the password without the current one. The web
    /// routes never expose this.
    pub fn provision_password(&self, password: &str) -> Result<(), SettingsError> {
        let _transaction = self.begin()?;
        self.replace_password(Some(password))
    }

    fn replace_password(&self, password: Option<&str>) -> Result<(), SettingsError> {
        let hash = match password.map(str::trim).filter(|p| !p.is_empty()) {
            Some(p) if p.chars().count() < MIN_PASSWORD_LEN || p.len() > MAX_PASSWORD_BYTES => {
                return Err(SettingsError::Invalid(format!(
                    "password must be at least {MIN_PASSWORD_LEN} characters and at most {MAX_PASSWORD_BYTES} bytes"
                )));
            }
            Some(p) => Some(hash_password(p)?),
            None => None,
        };
        let cleared = hash.is_none();
        let mut next = self.snapshot();
        next.password_hash = hash;
        self.persist(&next)?;
        *self
            .settings
            .write()
            .unwrap_or_else(PoisonError::into_inner) = next;
        self.lockout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .succeeded();
        if cleared {
            tracing::warn!(
                "settings password cleared; the page is open to anyone who can reach it"
            );
        } else {
            tracing::info!("settings password set");
        }
        Ok(())
    }

    /// A copy of the current settings, secrets included (for the forwarder and manager).
    #[must_use]
    pub fn snapshot(&self) -> Settings {
        self.settings
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// What the page may see.
    #[must_use]
    pub fn redacted(&self) -> RedactedSettings {
        let s = self.snapshot();
        RedactedSettings {
            forward_url: s.forward_url.clone(),
            forward_api_key_hint: s.forward_api_key.as_deref().map(|k| {
                let tail: String = k
                    .chars()
                    .rev()
                    .take(4)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                format!("…{tail}")
            }),
            forward_token_set: s.forward_token.is_some(),
            cors_origins: s.cors_origins.clone(),
            host: s.host.clone(),
            assignments: s.assignments.clone(),
            fallback_driver: s.fallback_driver.clone(),
            scale_quiet_ms: s.scale_quiet_ms(),
            scale_min_weight_kg: s.scale_min_weight_kg(),
            password_set: s.password_hash.is_some(),
            locked_for_secs: self.locked_for(Instant::now()).map(secs_ceil),
            restart_required: *self
                .restart_required
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            settings_path: self.path.display().to_string(),
            known_drivers: self.known_drivers.clone(),
        }
    }

    /// Remaining lockout, if any.
    #[must_use]
    pub fn locked_for(&self, now: Instant) -> Option<Duration> {
        self.lockout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remaining(now)
    }

    /// Check the supplied password against the stored hash, with lockout
    /// accounting. Passes trivially when no password is set.
    fn authorise(&self, password: Option<&str>, now: Instant) -> Result<(), SettingsError> {
        let hash = self.snapshot().password_hash;
        let Some(hash) = hash else {
            return Ok(());
        };
        if let Some(remaining) = self.locked_for(now) {
            return Err(SettingsError::Locked(remaining));
        }
        let Some(password) = password.filter(|p| !p.is_empty()) else {
            return Err(SettingsError::PasswordRequired);
        };
        if password.len() > MAX_PASSWORD_BYTES {
            return Err(SettingsError::WrongPassword);
        }
        // Hashing takes a moment; do it without holding the lockout lock.
        let ok = verify_password(password, &hash);
        let mut lockout = self.lockout.lock().unwrap_or_else(PoisonError::into_inner);
        if ok {
            lockout.succeeded();
            Ok(())
        } else {
            let delay = lockout.failed(Instant::now());
            tracing::warn!(
                failures = lockout.failures,
                delay_secs = delay.as_secs(),
                "wrong settings password"
            );
            Err(SettingsError::WrongPassword)
        }
    }

    fn validate(&self, s: &Settings) -> Result<(), SettingsError> {
        if let Some(url) = &s.forward_url {
            validate_forward_url(url)?;
        }
        for secret in [&s.forward_api_key, &s.forward_token].into_iter().flatten() {
            if secret.len() > 4096 || !secret.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(SettingsError::Invalid(
                    "credential must be printable ASCII without whitespace, at most 4096 bytes"
                        .into(),
                ));
            }
        }
        if let Some(q) = s.scale_quiet_ms
            && !(SCALE_QUIET_MS_RANGE.0..=SCALE_QUIET_MS_RANGE.1).contains(&q)
        {
            return Err(SettingsError::Invalid(format!(
                "scale quiet time must be {}–{} ms",
                SCALE_QUIET_MS_RANGE.0, SCALE_QUIET_MS_RANGE.1
            )));
        }
        if let Some(w) = s.scale_min_weight_kg
            && !(w.is_finite() && (0.0..=50.0).contains(&w))
        {
            return Err(SettingsError::Invalid(
                "scale minimum weight must be 0–50 kg".to_owned(),
            ));
        }
        let unknown = |kind: &str| {
            !self
                .known_drivers
                .iter()
                .any(|k| k.eq_ignore_ascii_case(kind))
        };
        if let Some(k) = &s.fallback_driver
            && unknown(k)
        {
            return Err(SettingsError::Invalid(format!(
                "unknown fallback driver {k:?}"
            )));
        }
        for (port, kind) in &s.assignments {
            if port.trim().is_empty() {
                return Err(SettingsError::Invalid(
                    "assignment with an empty port".to_owned(),
                ));
            }
            if unknown(kind) {
                return Err(SettingsError::Invalid(format!(
                    "unknown driver {kind:?} for port {port}"
                )));
            }
        }
        Ok(())
    }

    /// Apply a change from the page. Requires the password when one is set.
    pub fn update(
        &self,
        patch: SettingsPatch,
        password: Option<&str>,
    ) -> Result<RedactedSettings, SettingsError> {
        let _transaction = self.begin()?;
        self.authorise(password, Instant::now())?;
        let mut next = self.snapshot();
        if let Some(v) = clean(patch.forward_url) {
            // A destination change never carries a saved credential to a new server.
            if next.forward_url != v {
                next.forward_api_key = None;
                next.forward_token = None;
            }
            next.forward_url = v;
        }
        if let Some(v) = clean(patch.forward_api_key) {
            next.forward_api_key = v;
        }
        if let Some(v) = clean(patch.forward_token) {
            next.forward_token = v;
        }
        if let Some(v) = patch.cors_origins {
            next.cors_origins = v
                .into_iter()
                .map(|o| o.trim().to_owned())
                .filter(|o| !o.is_empty())
                .collect();
        }
        if let Some(v) = clean(patch.host) {
            next.host = v;
        }
        if let Some(v) = patch.assignments {
            next.assignments = v
                .into_iter()
                .map(|(p, k)| (p.trim().to_owned(), k.trim().to_owned()))
                .filter(|(p, k)| !p.is_empty() && !k.is_empty())
                .collect();
        }
        if let Some(v) = clean(patch.fallback_driver) {
            next.fallback_driver = v;
        }
        if let Some(v) = patch.scale_quiet_ms {
            next.scale_quiet_ms = Some(v);
        }
        if let Some(v) = patch.scale_min_weight_kg {
            next.scale_min_weight_kg = Some(v);
        }
        self.validate(&next)?;
        // Persist first: a failed write must not publish settings the disk does not have.
        self.persist(&next)?;
        let restart = {
            let current = self.settings.read().unwrap_or_else(PoisonError::into_inner);
            current.restart_relevant() != next.restart_relevant()
        };
        *self
            .settings
            .write()
            .unwrap_or_else(PoisonError::into_inner) = next;
        if restart {
            *self
                .restart_required
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = true;
        }
        tracing::info!(restart_required = restart, "settings updated from the page");
        Ok(self.redacted())
    }

    /// Set, change or clear the password. `current` must verify when one is
    /// set; `new` of `None`/empty clears it.
    pub fn set_password(
        &self,
        current: Option<&str>,
        new: Option<&str>,
    ) -> Result<RedactedSettings, SettingsError> {
        let _transaction = self.begin()?;
        self.authorise(current, Instant::now())?;
        self.replace_password(new)?;
        Ok(self.redacted())
    }

    /// Write the file atomically, owner-readable only on Unix.
    fn persist(&self, settings: &Settings) -> Result<(), SettingsError> {
        let bytes =
            serde_json::to_vec_pretty(&settings).map_err(|e| SettingsError::Io(e.to_string()))?;
        crate::storage::write_private(&self.path, &bytes)
            .map_err(|e| SettingsError::Io(e.to_string()))
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

    fn temp_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dr-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("settings.json")
    }

    fn drivers() -> Vec<String> {
        vec![
            "healthometer_scale".to_owned(),
            "consult120_urinalysis".to_owned(),
        ]
    }

    fn patch(url: &str) -> SettingsPatch {
        SettingsPatch {
            forward_url: Some(url.to_owned()),
            ..SettingsPatch::default()
        }
    }

    fn open(path: PathBuf, seed: &Settings) -> SettingsStore {
        SettingsStore::open(path, seed, drivers()).unwrap()
    }

    #[test]
    fn flags_seed_a_missing_file_and_the_file_wins_afterwards() {
        let path = temp_path();
        let seed = Settings {
            forward_url: Some("http://seed/fhir/v5".to_owned()),
            forward_api_key: Some("seedkey1".to_owned()),
            ..Settings::default()
        };
        {
            let store = open(path.clone(), &seed);
            assert_eq!(
                store.snapshot().forward_url.as_deref(),
                Some("http://seed/fhir/v5")
            );
            assert!(path.exists(), "the seed is written back");
            store.update(patch("http://page/fhir/v5"), None).unwrap();
        }
        let reopened = open(path, &seed);
        assert_eq!(
            reopened.snapshot().forward_url.as_deref(),
            Some("http://page/fhir/v5"),
            "file beats flag"
        );
        assert!(
            reopened.snapshot().forward_api_key.is_none(),
            "a cleared credential stays cleared even though the flag still supplies one"
        );
    }

    #[test]
    fn corrupt_or_unreadable_settings_fail_closed() {
        let path = temp_path();
        std::fs::write(&path, b"{broken").unwrap();
        assert!(SettingsStore::open(path.clone(), &Settings::default(), drivers()).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{broken",
            "the damaged file is left for the operator"
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(SettingsStore::open(path, &Settings::default(), drivers()).is_err());
    }

    #[test]
    fn a_second_process_cannot_open_the_same_file() {
        let path = temp_path();
        let _first = open(path.clone(), &Settings::default());
        assert!(matches!(
            SettingsStore::open(path, &Settings::default(), drivers()),
            Err(SettingsError::Io(_))
        ));
    }

    #[test]
    fn changing_destination_clears_old_credentials() {
        let store = open(
            temp_path(),
            &Settings {
                forward_url: Some("http://first/fhir".into()),
                forward_api_key: Some("firstkey".into()),
                forward_token: Some("firsttoken".into()),
                ..Settings::default()
            },
        );
        store.update(patch("http://second/fhir"), None).unwrap();
        let s = store.snapshot();
        assert!(s.forward_api_key.is_none() && s.forward_token.is_none());
        // Same destination keeps them.
        let store = open(
            temp_path(),
            &Settings {
                forward_url: Some("http://same/fhir".into()),
                forward_api_key: Some("key".into()),
                ..Settings::default()
            },
        );
        store.update(patch("http://same/fhir"), None).unwrap();
        assert!(store.snapshot().forward_api_key.is_some());
    }

    #[test]
    fn empty_string_clears_and_none_keeps() {
        let store = open(temp_path(), &Settings::default());
        store
            .update(
                SettingsPatch {
                    forward_api_key: Some("abcd1234".to_owned()),
                    ..SettingsPatch::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(
            store.redacted().forward_api_key_hint.as_deref(),
            Some("…1234")
        );
        store
            .update(
                SettingsPatch {
                    host: Some("h".to_owned()),
                    ..SettingsPatch::default()
                },
                None,
            )
            .unwrap();
        assert!(
            store.snapshot().forward_api_key.is_some(),
            "None keeps the key"
        );
        store
            .update(
                SettingsPatch {
                    forward_api_key: Some(String::new()),
                    ..SettingsPatch::default()
                },
                None,
            )
            .unwrap();
        assert!(
            store.snapshot().forward_api_key.is_none(),
            "empty clears the key"
        );
    }

    #[test]
    fn validation_rejects_bad_values_and_changes_nothing() {
        let store = open(temp_path(), &Settings::default());
        for bad in [
            "emr:8000",
            "ftp://emr/fhir",
            "http://user:pw@emr/fhir",
            "http://emr/fhir?key=x",
            "http://emr/fhir#frag",
        ] {
            assert!(
                matches!(
                    store.update(patch(bad), None),
                    Err(SettingsError::Invalid(_))
                ),
                "{bad}"
            );
        }
        assert!(
            validate_forward_url("http://100.64.0.10:8000/fhir/v5").is_ok(),
            "plain http on the tailnet is allowed"
        );
        assert!(validate_forward_url("https://emr.example/fhir/v5").is_ok());
        let bad_driver = SettingsPatch {
            assignments: Some(BTreeMap::from([("COM3".to_owned(), "nope".to_owned())])),
            ..SettingsPatch::default()
        };
        assert!(matches!(
            store.update(bad_driver, None),
            Err(SettingsError::Invalid(_))
        ));
        let bad_quiet = SettingsPatch {
            scale_quiet_ms: Some(10),
            ..SettingsPatch::default()
        };
        assert!(matches!(
            store.update(bad_quiet, None),
            Err(SettingsError::Invalid(_))
        ));
        let bad_secret = SettingsPatch {
            forward_api_key: Some("has space".to_owned()),
            ..SettingsPatch::default()
        };
        assert!(matches!(
            store.update(bad_secret, None),
            Err(SettingsError::Invalid(_))
        ));
        assert!(
            store.snapshot().forward_url.is_none(),
            "a rejected patch changes nothing"
        );
    }

    #[test]
    fn restart_required_flags_only_startup_settings() {
        let store = open(temp_path(), &Settings::default());
        store.update(patch("http://x/fhir/v5"), None).unwrap();
        assert!(!store.redacted().restart_required, "forwarding is live");
        store
            .update(
                SettingsPatch {
                    host: Some("lab-pi".to_owned()),
                    ..SettingsPatch::default()
                },
                None,
            )
            .unwrap();
        assert!(store.redacted().restart_required);
    }

    #[test]
    fn password_gates_changes_and_locks_out_with_backoff() {
        let store = open(temp_path(), &Settings::default());
        assert!(matches!(
            store.set_password(None, Some("short")),
            Err(SettingsError::Invalid(_))
        ));
        store.set_password(None, Some("correct horse")).unwrap();
        assert!(store.redacted().password_set);
        assert!(
            store
                .snapshot()
                .password_hash
                .unwrap()
                .starts_with("$argon2id$")
        );

        assert_eq!(
            store.update(patch("http://x/fhir/v5"), None).unwrap_err(),
            SettingsError::PasswordRequired
        );
        assert_eq!(
            store
                .update(patch("http://x/fhir/v5"), Some("wrong"))
                .unwrap_err(),
            SettingsError::WrongPassword
        );
        // Now locked for 1 s; even the right password is refused until it lapses.
        assert!(matches!(
            store.update(patch("http://x/fhir/v5"), Some("correct horse")),
            Err(SettingsError::Locked(_))
        ));
        std::thread::sleep(Duration::from_millis(1100));
        store
            .update(patch("http://x/fhir/v5"), Some("correct horse"))
            .unwrap();
        assert!(
            store.locked_for(Instant::now()).is_none(),
            "success resets the lockout"
        );

        // Changing needs the current one; clearing too.
        assert_eq!(
            store
                .set_password(Some("nope"), Some("another one"))
                .unwrap_err(),
            SettingsError::WrongPassword
        );
        std::thread::sleep(Duration::from_millis(1100));
        store.set_password(Some("correct horse"), None).unwrap();
        assert!(
            !store.redacted().password_set,
            "the page may remove the password"
        );
        store.update(patch("http://y/fhir/v5"), None).unwrap();
    }

    #[test]
    fn local_provisioning_resets_without_the_old_password() {
        let store = open(temp_path(), &Settings::default());
        store.set_password(None, Some("correct horse")).unwrap();
        store.provision_password("replacement one").unwrap();
        assert_eq!(
            store
                .update(patch("http://x/fhir/v5"), Some("correct horse"))
                .unwrap_err(),
            SettingsError::WrongPassword
        );
        std::thread::sleep(Duration::from_millis(1100));
        store
            .update(patch("http://x/fhir/v5"), Some("replacement one"))
            .unwrap();
    }

    #[test]
    fn lockout_doubles_and_caps() {
        let mut l = Lockout::default();
        let t0 = Instant::now();
        assert_eq!(l.failed(t0), Duration::from_secs(1));
        assert_eq!(l.failed(t0), Duration::from_secs(2));
        assert_eq!(l.failed(t0), Duration::from_secs(4));
        for _ in 0..20 {
            l.failed(t0);
        }
        assert_eq!(l.failed(t0), LOCKOUT_MAX);
        assert!(l.remaining(t0).is_some());
        assert!(
            l.remaining(t0 + LOCKOUT_MAX + Duration::from_secs(1))
                .is_none()
        );
        l.succeeded();
        assert!(l.remaining(t0).is_none());
        assert_eq!(
            l.failed(t0),
            Duration::from_secs(1),
            "reset restarts the ladder"
        );
    }

    #[test]
    fn redaction_never_leaks_secrets() {
        let store = open(temp_path(), &Settings::default());
        store
            .update(
                SettingsPatch {
                    forward_api_key: Some("supersecretkey".to_owned()),
                    forward_token: Some("bearer-token".to_owned()),
                    ..SettingsPatch::default()
                },
                None,
            )
            .unwrap();
        let json = serde_json::to_string(&store.redacted()).unwrap();
        assert!(!json.contains("supersecretkey"));
        assert!(!json.contains("bearer-token"));
        assert!(json.contains("…tkey"));
        assert!(json.contains("\"forward_token_set\":true"));
    }
}
