mod audit;
mod capture;
pub mod capture_test;
pub mod client;
mod clipboard;
mod coalesce;
pub mod config;
mod connect;
mod crypto;
mod cursor_hide;
#[cfg(all(test, unix))]
mod desk_sim;
mod devices;
mod discovery;
mod dns;
pub mod doctor;
mod emulation;
pub mod emulation_test;
mod handoff_trace;
mod latency;
mod listen;
mod lock_sync;
mod net_tuning;
mod reset;
pub mod service;
mod service_core;

/// This app's semantic version, exchanged in the peer Hello handshake.
pub(crate) fn local_app_version() -> lan_mouse_proto::AppVersion {
    lan_mouse_proto::AppVersion::parse(env!("CARGO_PKG_VERSION")).unwrap_or_default()
}

pub mod trusted;
