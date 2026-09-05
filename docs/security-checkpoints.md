# Security checkpoints

What was reviewed, what was kept and what was reverted, after an outside review of the settings
and forwarding code (2026-09-05). No production credentials or EMR writes were used in tests.

## Kept (hardening)

- **Private, atomic, locked state files**: `storage::write_private` (temp file, fsync, rename,
  directory fsync, mode 0600/0700) and a lock file that refuses a second writer.
- **Fail closed on a damaged settings file**: the service stops instead of starting with no
  password and no destination.
- **Changing the destination clears its credentials**, so a saved key cannot be pointed at
  another server.
- **Durable ingestion**: completed readings go to the queue before they are broadcast, storage
  failures pause capture and are shown on the page, the queue refuses new items at its cap
  instead of dropping old ones, and unreadable queues stop the service.
- **No overwrite after a lost response**: the attempt is persisted before each send, and any
  retry checks `GET /Observation/{id}` before it will `PUT`.
- **Credential and redirect handling**: 401/403 keep readings pending with a clear message,
  redirects are never followed, response bodies are not logged.
- **Web limits**: 16 KB request bodies, 16 WebSocket clients, 1 KB frames, a 5 s send timeout,
  one Argon2 check at a time, one EMR probe at a time.
- **Headers**: `Cache-Control: no-store`, `X-Frame-Options: DENY`, `nosniff`, and a
  same-origin Content-Security-Policy on every response.
- **Local password reset** (`set-password --password-file`) and `retry-rejected` subcommands.

## Reverted (design)

- Login sessions and cookies. The page is read-open on the tailnet; changes carry the optional
  password with each request, which needs no session and cannot be ridden by CSRF.
- Mandatory local provisioning before first use. A fresh install is usable from the page.
- HTTPS-only browser origin and the `--web-origin` / `--http-forward-origin` flags. Tailscale
  already encrypts the hop; the reporter should not refuse to work on it.
- Conditional `POST` with `If-None-Exist` gated on `GET /metadata`. `/metadata` is a valid FHIR
  endpoint (the `capabilities` interaction), but the server this feeds does not implement it or
  `If-None-Exist`, and its `identifier` search is unreliable, so that path could not deliver
  anything. `PUT` by client id with the GET-before-retry check gives the same guarantee
  against overwriting a clinician's accepted result.
- Disabling the queue in demo mode. The demo now exercises the real path and warns at startup
  when a forward URL is set.

## Evidence

- `cargo clippy --all-targets` clean under the strict lint set; 78 tests pass.
- Live smoke test against a stub FHIR server: settings saved without a password, probe
  answered with the key, password set from the page, then 401 without it, 401 with a wrong one,
  423 with the retry delay, 200 after the delay; readings delivered as `PUT` with `X-API-Key`;
  after a server outage the retried reading was checked with `GET` and not re-`PUT`, while the
  untried reading was `PUT` normally.
