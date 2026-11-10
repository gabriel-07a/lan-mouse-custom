//! Dev-only utilities shared by the lan-mouse test suites.
//!
//! This crate is deliberately **not** part of the shipped binary: it is only
//! listed under `[dev-dependencies]` of the workspace crates. Keeping the
//! helpers here avoids the copy-pasted `unique_cert` / `free_udp_port` /
//! listener fixtures that used to drift between test modules.
//!
//! Rules for anything added here:
//!
//! - no `sleep`, no fixed ports, no DNS/portal/network side effects;
//! - helpers must be deterministic and safe to use from parallel tests;
//! - production types are reused (`ProtoEvent`, `ClientState`, ...) instead of
//!   duplicated test-only structs.

pub mod assertions;
pub mod clock;
pub mod env;
pub mod transport;
pub mod wire;
