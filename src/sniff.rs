//! `list` and `sniff`: the tools for figuring out a new device's protocol.
//!
//! `list` shows every serial port with its USB vendor/product IDs so a new
//! device can be told apart from the others. `sniff` opens a port with the
//! line settings you give it, optionally sends a probe, and prints whatever
//! comes back as a timestamped hex dump with the printable text beside it,
//! which is what `RealTerm` does. Press the device's send/print button, or
//! take a measurement, and watch what arrives.

use crate::driver::PortCandidate;
use clap::Args;
use jiff::Zoned;
use serialport::{DataBits, Parity, StopBits};
use std::io::{ErrorKind, Read, Write};
use std::time::Duration;

/// Arguments for `device-reporter sniff`.
#[derive(Debug, Args)]
pub struct SniffArgs {
    /// Port to open: `COM5`, `/dev/ttyUSB1`.
    pub port: String,

    #[arg(long, default_value_t = 9600)]
    pub baud: u32,

    /// 5, 6, 7 or 8.
    #[arg(long, default_value_t = 8)]
    pub data_bits: u8,

    /// none, odd or even.
    #[arg(long, default_value = "none")]
    pub parity: String,

    /// 1 or 2.
    #[arg(long, default_value_t = 1)]
    pub stop_bits: u8,

    /// Bytes to send once after opening, as hex (`1b 52 0d` or `1B520D`).
    #[arg(long)]
    pub send_hex: Option<String>,

    /// Text to send once after opening. Escapes `\r`, `\n`, `\t` and `\xHH` are honoured.
    #[arg(long)]
    pub send: Option<String>,

    /// Also write every received byte, raw, to this file for later replay.
    #[arg(long)]
    pub capture: Option<std::path::PathBuf>,
}

/// Print every serial port the OS knows about.
pub fn list() -> anyhow::Result<()> {
    let ports = serialport::available_ports()?;
    if ports.is_empty() {
        println!("No serial ports found.");
        return Ok(());
    }
    println!(
        "{:<14} {:<9} {:<14} {:<28} PRODUCT",
        "PORT", "VID:PID", "SERIAL", "MANUFACTURER"
    );
    for info in &ports {
        let p = PortCandidate::from(info);
        let ids = match (p.vid, p.pid) {
            (Some(v), Some(i)) => format!("{v:04x}:{i:04x}"),
            _ => "-".to_owned(),
        };
        println!(
            "{:<14} {:<9} {:<14} {:<28} {}",
            p.name,
            ids,
            p.serial_number.unwrap_or_else(|| "-".to_owned()),
            p.manufacturer.unwrap_or_else(|| "-".to_owned()),
            p.product.unwrap_or_else(|| "-".to_owned()),
        );
    }
    Ok(())
}

/// Open a port and dump what it sends until Ctrl-C.
pub fn sniff(args: &SniffArgs) -> anyhow::Result<()> {
    let data_bits = match args.data_bits {
        5 => DataBits::Five,
        6 => DataBits::Six,
        7 => DataBits::Seven,
        8 => DataBits::Eight,
        other => anyhow::bail!("data bits must be 5-8, got {other}"),
    };
    let parity = match args.parity.to_ascii_lowercase().as_str() {
        "none" | "n" => Parity::None,
        "odd" | "o" => Parity::Odd,
        "even" | "e" => Parity::Even,
        other => anyhow::bail!("parity must be none, odd or even, got {other:?}"),
    };
    let stop_bits = match args.stop_bits {
        1 => StopBits::One,
        2 => StopBits::Two,
        other => anyhow::bail!("stop bits must be 1 or 2, got {other}"),
    };

    let mut port = serialport::new(&args.port, args.baud)
        .data_bits(data_bits)
        .parity(parity)
        .stop_bits(stop_bits)
        .timeout(Duration::from_millis(200))
        .open()?;
    println!(
        "Opened {} at {} {}{}{}. Waiting for data; Ctrl-C to stop.",
        args.port,
        args.baud,
        args.data_bits,
        args.parity
            .chars()
            .next()
            .map_or('N', |c| c.to_ascii_uppercase()),
        args.stop_bits
    );

    let mut capture = match &args.capture {
        Some(path) => Some(std::fs::File::create(path)?),
        None => None,
    };

    let probe: Option<Vec<u8>> = match (&args.send_hex, &args.send) {
        (Some(hex), _) => Some(parse_hex(hex)?),
        (None, Some(text)) => Some(unescape(text)),
        (None, None) => None,
    };
    if let Some(bytes) = probe {
        port.write_all(&bytes)?;
        port.flush()?;
        println!("{}  TX  {}", stamp(), dump(&bytes));
    }

    let mut buf = [0u8; 512];
    loop {
        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                let bytes = buf.get(..n).unwrap_or_default();
                println!("{}  RX  {}", stamp(), dump(bytes));
                if let Some(f) = capture.as_mut() {
                    f.write_all(bytes)?;
                    f.flush()?;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                ) => {}
            Err(e) => anyhow::bail!("read failed: {e}"),
        }
    }
}

fn stamp() -> String {
    Zoned::now().strftime("%H:%M:%S%.3f").to_string()
}

/// `1b 52 0d 0a  |.R..|`
fn dump(bytes: &[u8]) -> String {
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let text: String = bytes
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    format!("{}  |{}|", hex.join(" "), text)
}

/// Accepts `1b 52`, `1B52`, `1b,52` or `0x1b 0x52`.
fn parse_hex(s: &str) -> anyhow::Result<Vec<u8>> {
    let cleaned: String = s
        .replace("0x", "")
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ',')
        .collect();
    if !cleaned.len().is_multiple_of(2) {
        anyhow::bail!("hex string has an odd number of digits: {s:?}");
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| {
            let pair = cleaned.get(i..i.saturating_add(2)).unwrap_or_default();
            u8::from_str_radix(pair, 16).map_err(|e| anyhow::anyhow!("bad hex byte {pair:?}: {e}"))
        })
        .collect()
}

/// Turn `\r`, `\n`, `\t`, `\\` and `\xHH` into bytes; everything else passes through as UTF-8.
fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut tmp = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
            continue;
        }
        match chars.next() {
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('x') => {
                let hi = chars.next().and_then(|c| c.to_digit(16));
                let lo = chars.next().and_then(|c| c.to_digit(16));
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push(u8::try_from(h.saturating_mul(16).saturating_add(l)).unwrap_or(0));
                    }
                    _ => out.extend_from_slice(b"\\x"),
                }
            }
            Some(other) if other != '\\' => {
                out.push(b'\\');
                let mut tmp = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut tmp).as_bytes());
            }
            // `\\` is a literal backslash; a trailing lone backslash is kept as-is.
            Some(_) | None => out.push(b'\\'),
        }
    }
    out
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
    fn hex_parsing_accepts_common_spellings() {
        assert_eq!(parse_hex("1b 52 0d").unwrap(), vec![0x1b, 0x52, 0x0d]);
        assert_eq!(parse_hex("1B520D").unwrap(), vec![0x1b, 0x52, 0x0d]);
        assert_eq!(parse_hex("0x1b,0x52").unwrap(), vec![0x1b, 0x52]);
        assert!(parse_hex("1b5").is_err());
        assert!(parse_hex("zz").is_err());
    }

    #[test]
    fn unescape_handles_control_bytes() {
        assert_eq!(unescape(r"\x1bR\r\n"), b"\x1bR\r\n");
        assert_eq!(unescape(r"plain"), b"plain");
        assert_eq!(unescape(r"a\\b"), b"a\\b");
        assert_eq!(unescape(r"\xZZ"), b"\\x");
        assert_eq!(unescape(r"\q"), b"\\q");
        assert_eq!(unescape("tail\\"), b"tail\\");
    }

    #[test]
    fn dump_shows_hex_and_printable_text() {
        assert_eq!(dump(b"\x1bR1"), "1b 52 31  |.R1|");
    }
}
