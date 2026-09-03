# Device notes

Working notes per device: what is known, what was tried, what is next. The driver code in
`src/drivers/` is the source of truth once a device works; this file is for the ones that do not
yet, so the next session does not repeat the archaeology.

## Health o meter 1100L scale (done)

Driver `healthometer_scale`. CP210x USB-to-UART, 9600 8N1, packets framed on `<ESC>E`.
Detected by VID:PID `10c4:ea60`. Deployed on the Pi `bmpc-kent-scale`. See
`src/drivers/healthometer/`.

## Welch Allyn Spot Vital Signs LXi (blood pressure, temperature, SpO2)

**Status:** protocol path identified, not yet captured. Needs a USB-to-RS-232 adapter and a
DB9 null-modem adapter.

**What the USB port is not.** The mini-USB port on the LXi enumerates as `0770:0450`
(Welch Allyn), vendor-specific class `FF`, one interface with bulk IN `0x81`, bulk OUT `0x02`,
interrupt IN `0x83`, no string descriptors. On this PC it is bound to `libusb0` (left from an
earlier Zadig experiment). Claiming the interface and listening on both IN endpoints for
15 minutes while pressing Send produced nothing; the device reports "transmit failed". The
service manual shows this port is for the *Spot LXi Service Tool* (calibration and tests),
so it is not the data path. Do not spend more time here.

**Where the data goes.** The LXi has two DB9 RS-232 ports under the handle (Port I, Port II),
each with an isolated 9 V supply on pin 9 for accessories. Welch Allyn's wired-connectivity
accessories are:

| Part | Description |
|---|---|
| 4500-925 | USB 2.0 Cable for Wired Connectivity |
| 4500-926 | Cable for Wired Connectivity, **Keyspan (USB to serial adapter)** |
| 4500-927 | Spot Vital Signs LXi USB/serial Cable Kit |

The optional Wi-Fi radio is a serial-to-TCP bridge on the same port; its LED table describes
"serial traffic across the wired serial port" during a send. So Send emits a serial stream, and
`device-reporter sniff` can capture it.

**Wiring.** The radio connects through a DB9 null-modem adapter (Welch Allyn MA 0051), so a
PC needs one too. Pinout from the service manual, male to female: 2-3, 3-2, 5-5, 4-(1+6),
(1+6)-4, 7-8, 8-7, 9-9.

**Device configuration.** Internal Configuration: *Information System* on and *Barcode
Patient ID* on. The Directions for Use says the latter "must" be enabled to send readings wired
or wirelessly. The radio setting picks which serial port the radio uses; for wired send, try
Port II first, then Port I.

**Serial settings.** Not stated in either manual. Start at 9600 8N1 and scan 19200, 38400,
57600, 115200 if the dump is garbage. (A web search summary claimed "9600 8N1"; that text is
not in the manuals, so it is a guess.)

**Protocol.** WACP, Welch Allyn Communications Protocol, a binary format with CRCs. The frame
layout is public in Welch Allyn's patents US8543999, US8402161 and US20080082683: session
preamble, packet length, port/application ID, sequence number, UUID, data length, data buffer,
header CRC; payloads are typed objects with class ID, size, version, bit field, payload and
object CRC. The device most likely expects an acknowledgement and will retry a few times, which
yields several clean copies of the frame for decoding.

**Next.** Capture a Send with `sniff --capture`, decode against the patent layout, then write
`src/drivers/welchallyn_lxi/` with the handshake. Transport is plain serial, so it slots into
the existing connection loop with no new dependencies.

**Not a path.** An Aprima EHR distribution containing the Welch Allyn Connectivity SDK
(`WAConnSDK.msi`) turned up on a third party's misconfigured download server. Not licensed to
us; not used. The SDK's existence confirms the product name to request if one is ever needed:
"Welch Allyn Connectivity SDK Core".

## McKesson Consult 120 urine analyzer (working, verified with level 1 and 2 controls)

Driver `consult120_urinalysis`, `src/drivers/consult120.rs`. USB serial via a WCH CH9102
bridge (`1a86:55d4`, shows as "USB Serial Device" on Windows), 9600 8N1. Pressing Print on a
result, or auto-print, sends one plain-text report framed by STX/ETX; the capture is in the
module docs and is the unit-test fixture. Fields map to LOINC test-strip codes; the grade column
(`-`, `+`, `++`) becomes the component's `interpretation`.

**Positive layout** (level 2 control): abnormal analytes are prefixed `*`, graded results print
`1+`..`4+`, nitrite prints a bare `+` with `positive`, trace prints `+/-`, and quantitative
values carry `Leu/uL`, `Ery/uL` or `mg/dL` (mapped to UCUM `{Leu}/uL`, `{Ery}/uL`, `mg/dL`).
The starred analytes are listed in an `abnormal:` flag. Raw reports are logged at DEBUG.

**Open items.**
- The `ID:` line is whatever was keyed in; it becomes `subject_hint`. `Date:` is the analyzer's
  own clock (12- or 24-hour depending on its settings), passed through as a `device_time:`
  flag. It was months off when first connected; corrected 2026-09-03.
- The CH9102 is a generic bridge chip. If another CH9102 device appears in the clinic, pin
  ports with `--assign`. Longer term the reporter should identify text-protocol devices by
  their first frame rather than by bridge chip.

## Several devices on one Pi (the lab bench)

The manager runs one connection thread per serial port, so a powered USB hub with the Consult 120
and the hemoglobin meter is the normal case. Each observation carries `device_id` and
`device_kind`; `/api/observations?device=ID` filters. Two things to check when adding a device:

1. **Identification.** Devices are recognised by the USB bridge chip's VID:PID. Run
   `device-reporter list` with everything plugged in. If two devices share a chip (two CH9102s,
   say), pin them with `--assign PORT=KIND`, and on Linux give each a stable name first with a
   udev rule keyed on its USB serial number, e.g.
   `SUBSYSTEM=="tty", ATTRS{serial}=="56A4065083", SYMLINK+="urinalysis"`. If that becomes
   common, the next step is content-based identification: open unknown ports at 9600 8N1 and
   pick the driver from the first frame (the scale's `<ESC>R`, the Consult 120's `STX ID:`).
2. **Device IDs.** `{host}-{usb serial}`. CP210x bridges all report serial `0001`, so a second
   one on the same host gets its port appended (`pi-0001-dev-ttyUSB1`). Prefer a udev symlink
   there too so the appended port name is stable.

The Pi Zero's single OTG port carries no useful power; use a powered hub even though the
instruments are self-powered.

## Detecto sonar stadiometer (height)

Not started. Has an internal USB header; open it up and `device-reporter list` to see what
bridge chip it uses.

## HemoCue Hb 201+ (hemoglobin)

**Status:** identified, not yet captured. Waiting on a cuvette shipment to run a test.

Enumerates as an FTDI FT232 bridge, `0403:6001`, USB serial number `7`, "USB Serial Port" on
Windows (COM8 here). Distinct from the Consult 120's CH9102, so the two coexist on one hub
without `--assign`. The reporter correctly logs it once as "no matching driver" and leaves it
alone.

**Next**, once cuvettes arrive: `device-reporter sniff COM8 --baud 9600 --capture hgb.bin`, run a
measurement, and also try the meter's data-port/print function on the last result. The
operating manual describes a data port for a printer or PC (the public copies online were not
fetchable from here; the paper manual's technical-specification section should state the baud
rate). If 9600 gives garbage, scan 19200, 38400, 57600, 115200. Expected: a short ASCII line
with the g/dL value, date and time; LOINC 718-7 "Hemoglobin [Mass/volume] in Blood", UCUM `g/dL`.
