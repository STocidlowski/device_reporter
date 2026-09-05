# Secure deployment and recovery

What protects a Pi running the reporter, in order of how much it matters:

1. **The tailnet is the perimeter.** Tailscale plus the UFW rules in the README mean only
   enrolled devices can reach the page or SSH at all. WireGuard encrypts every hop, so plain
   HTTP between the Pi and the EMR is acceptable inside the tailnet.
2. **The settings password** gates changes to the EMR destination, credentials and port
   assignments. It is optional: the page works without one and says so, and it is set from the
   page. Readings are visible to anyone who can reach the page, which is the point of the page.
3. **Private state files** with conservative permissions, atomic writes and a lock against a
   second writer.
4. **Delivery that cannot overwrite a clinician's work** (see below).

## Settings password

- Set it from the page (Settings → *Set a settings password*). Eight characters or more, up to
  1024 bytes. Only an Argon2id hash is stored.
- Every change then carries the password with the request. There is no login session and no
  cookie, so nothing can be replayed by another site.
- Wrong passwords lock changes for 1 s, then 2 s, 4 s, ... up to two hours. A correct password
  resets the delay. The lockout is global, not per client, because behind nginx every client
  looks the same.
- Forgot it? Stop the service and either run
  `device-reporter --settings <file> set-password --password-file <private-file>` (the file
  holds only the new password; delete it afterwards) or delete the `password_hash` line from
  the settings file. Both need SSH, which the tailnet already gates.

## State files

`settings.json` holds the EMR key in cleartext and the password hash. `queue.json` holds
readings waiting to be delivered and the ones the EMR rejected. Both are created with mode
`0600` in a `0700` directory, written to a temporary file, synced, renamed, and the directory
synced, so a power cut leaves either the old file or the new one. A file lock refuses a second
writer. A damaged file stops the service rather than starting with defaults; restore it from a
backup or delete it and re-enter the settings.

The service account and root can read those files. A removed SD card bypasses file permissions.
If a Pi is lost, revoke its EMR key and its Tailscale node.

## Delivery

- Completed readings are written to the queue before they are announced on the page or the
  WebSocket, so a slow browser cannot lose a reading. If the queue cannot be written (disk full,
  2000 items or 16 MiB reached) the page shows a storage error and capture pauses.
- Delivery is `PUT /Observation/{id}` with a client-chosen id. Before each attempt the attempt
  is recorded on disk. If the attempt count shows an earlier try, the reporter first does
  `GET /Observation/{id}`: a `200` means the earlier try landed and the item is done, a `404`
  means it is safe to `PUT`. A response lost on the wire therefore never overwrites a reading a
  clinician has already accepted into a chart.
- `401`/`403` keep the reading pending and the page says the EMR rejected the credentials.
  Redirects are never followed. `400`/`422` move the reading to the rejected list; after fixing
  the cause, stop the service and run `device-reporter --queue-file <file> retry-rejected`.
- The page shows pending and rejected counts and the last delivery message, refreshed every
  15 s from `/api/status`.
- `--demo` uses the same queue, so a demo with a forward URL configured sends simulated
  readings to that EMR. The service logs a warning at startup when that is the case.

## Service unit

A hardened unit for a dedicated account. The simpler unit in the README, running as your own
user, is fine on a tailnet-only Pi; this one costs nothing extra once set up.

```systemd
[Unit]
Description=Device Reporter
After=network-online.target
Wants=network-online.target

[Service]
User=device-reporter
Group=device-reporter
SupplementaryGroups=dialout
ExecStart=/usr/local/bin/device-reporter
WorkingDirectory=/var/lib/device-reporter
StateDirectory=device-reporter
StateDirectoryMode=0700
UMask=0077
Environment=DR_BIND=127.0.0.1:8080
Environment=DR_SETTINGS=/var/lib/device-reporter/settings.json
Environment=DR_QUEUE_FILE=/var/lib/device-reporter/queue.json
Environment=RUST_LOG=info
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/device-reporter
PrivateTmp=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
LockPersonality=true
CapabilityBoundingSet=
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6

[Install]
WantedBy=multi-user.target
```

Do not add `PrivateDevices=true`: it hides the serial ports. Argon2 allocates 16 MiB per
password check, so leave memory limits alone on a Pi Zero. Stop the service before replacing the
binary, resetting the password or editing state files.

## Upgrading a Pi from the pre-settings build

1. Stop the service and back up the old queue file privately.
2. Copy the new binary over (the service must be stopped; see the README).
3. Start it. The old JSON-array queue is migrated on first start. Flags and `DR_*` variables
   from the old environment file seed the new settings file once; from then on the page is the
   source of truth, and clearing a value on the page keeps it cleared across restarts.
4. Open the page, set a settings password, enter the EMR base URL and key, and press *Test
   EMR connection*. Then take a synthetic reading and confirm it arrives on the FHIR server as a
   preliminary Observation.
