//! One module per supported device. Register new drivers in [`crate::driver::registry`].
//!
//! Notes on devices still being worked out live in `docs/devices.md`; use
//! `device-reporter list` and `device-reporter sniff PORT` to capture them.

pub mod consult120;
pub mod healthometer;
