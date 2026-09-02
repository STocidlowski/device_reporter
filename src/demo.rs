//! A fake scale for developing without hardware (`--demo`).
//!
//! Implements `Read + Write` so it runs through the exact same connection
//! loop, framer, parser and session logic as a real port. Every 15 seconds
//! someone "steps on": a locked weight is streamed once per second for a few
//! seconds, then silence. Weights and unit systems cycle so the page shows
//! variety, and one visitor enters a height so BMI appears.

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use crate::serial::READ_TIMEOUT;

const BETWEEN_VISITORS: Duration = Duration::from_secs(15);
const PACKET_INTERVAL: Duration = Duration::from_secs(1);

/// (weight, metric?, height) scenarios, cycled forever.
const VISITORS: &[(f64, bool, Option<f64>, u32)] = &[
    (184.5, false, None, 6),
    (45.0, false, None, 1), // a child hops on for one second: flagged single_packet
    (72.4, true, Some(178.0), 5),
    (210.8, false, Some(70.0), 4),
];

/// Scripted byte source standing in for a serial port.
#[derive(Debug)]
pub struct DemoTransport {
    pending: VecDeque<u8>,
    visitor: usize,
    packets_left: u32,
    next_packet_at: Instant,
}

impl DemoTransport {
    /// The first visitor arrives three seconds after start.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            visitor: 0,
            packets_left: 0,
            next_packet_at: Instant::now() + Duration::from_secs(3),
        }
    }

    fn packet(weight: f64, metric: bool, height: Option<f64>) -> Vec<u8> {
        let h = height.unwrap_or(0.0);
        let bmi = match (height, metric) {
            (Some(h), true) => weight / (h / 100.0).powi(2),
            (Some(h), false) => 703.0 * weight / (h * h),
            (None, _) => 0.0,
        };
        let units = if metric { 'm' } else { 'c' };
        format!(
            "\x1bR\x1bI0000000000\x1bW{weight:.1}\x1bH{h:.1}\x1bB{bmi:.1}\x1bT0.0\x1bN{units}\x1bE"
        )
        .into_bytes()
    }

    fn advance(&mut self) {
        let now = Instant::now();
        if now < self.next_packet_at {
            return;
        }
        let idx = self.visitor % VISITORS.len();
        let (weight, metric, height, packets) = VISITORS
            .get(idx)
            .copied()
            .unwrap_or((100.0, false, None, 3));
        if self.packets_left == 0 {
            self.packets_left = packets;
        }
        self.pending.extend(Self::packet(weight, metric, height));
        self.packets_left = self.packets_left.saturating_sub(1);
        if self.packets_left == 0 {
            self.visitor = self.visitor.wrapping_add(1);
            self.next_packet_at = now + BETWEEN_VISITORS;
        } else {
            self.next_packet_at = now + PACKET_INTERVAL;
        }
    }
}

impl Default for DemoTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Read for DemoTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.advance();
        if self.pending.is_empty() {
            // Behave like a serial port with a read timeout.
            let wait = self
                .next_packet_at
                .saturating_duration_since(Instant::now())
                .min(READ_TIMEOUT);
            std::thread::sleep(wait);
            return Err(io::Error::from(ErrorKind::TimedOut));
        }
        let mut n: usize = 0;
        for slot in buf.iter_mut() {
            match self.pending.pop_front() {
                Some(b) => {
                    *slot = b;
                    n = n.saturating_add(1);
                }
                None => break,
            }
        }
        Ok(n)
    }
}

impl Write for DemoTransport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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
    use crate::drivers::healthometer::protocol::{UnitSystem, parse_packet};

    #[test]
    fn demo_packets_parse_with_the_real_parser() {
        let p = parse_packet(&DemoTransport::packet(72.4, true, Some(178.0))).unwrap();
        assert_eq!(p.weight, 72.4);
        assert_eq!(p.units, UnitSystem::Metric);
        assert_eq!(p.height, Some(178.0));
        assert!((p.bmi.unwrap() - 22.9).abs() < 0.1);

        let p = parse_packet(&DemoTransport::packet(184.5, false, None)).unwrap();
        assert_eq!(p.units, UnitSystem::Imperial);
        assert_eq!(p.height, None);
        assert_eq!(p.bmi, None);
    }

    #[test]
    fn nothing_arrives_before_the_first_visitor() {
        let mut t = DemoTransport::new();
        t.next_packet_at = Instant::now() + Duration::from_millis(50);
        let mut buf = [0u8; 64];
        assert_eq!(t.read(&mut buf).unwrap_err().kind(), ErrorKind::TimedOut);
        std::thread::sleep(Duration::from_millis(60));
        let n = t.read(&mut buf).unwrap();
        assert!(n > 0);
        assert!(parse_packet(&buf[..n]).is_ok());
    }
}
