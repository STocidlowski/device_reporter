//! One module per supported device. Register new drivers in [`crate::driver::registry`].
//!
//! Protocol notes for devices we still have to reverse-engineer (use
//! `device-reporter list` and `device-reporter sniff PORT` to capture them):
//! urinalysis strip reader, automatic blood-pressure cuff, Detecto sonar
//! stadiometer, hemoglobin meter.

pub mod healthometer;
