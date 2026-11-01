//! The `mare` program, as a library its binary and tests share.
//!
//! `mare devices` lists the audio devices; `mare run` runs a hub on one
//! (with a capture device beside it, and other devices joined to it as
//! followers), until Ctrl+C; `mare probe` shows a running hub. `mare run
//! --null` runs a hub on no device at all, on any system, for trying
//! programs without a sound card. See [`args::USAGE`].

pub mod args;
pub mod session;
pub mod show;

pub use args::{Command, DeviceChoice, RunOptions, UsageError, parse};
pub use session::{AnyDevice, FollowerReport, Report, Session, StartError};
