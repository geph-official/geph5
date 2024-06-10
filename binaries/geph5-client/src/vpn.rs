//! This module provides functionality for setting up a system-level VPN.
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::*;
