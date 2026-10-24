//! Shared building blocks for echo-server-core.
//!
//! * [`config`] — TOML configuration types for hub and spoke
//! * [`tls`]    — mutual-TLS plumbing on top of rustls
//! * [`protocol`] — the control-plane wire format (length-prefixed JSON)
//! * [`mux`]    — a driver for `yamux` that exposes a cloneable handle
//! * [`pki`]    — certificate authority bootstrap helpers

pub mod config;
pub mod mux;
pub mod pki;
pub mod protocol;
pub mod tls;

/// ALPN protocol identifier negotiated during the TLS handshake.
///
/// A mismatched ALPN makes the handshake fail early, which gives us protocol
/// version pinning for free.
pub const ALPN: &[u8] = b"esc/1";

/// Default port the hub listens on for spoke connections.
pub const DEFAULT_CONTROL_PORT: u16 = 4433;

/// Maximum accepted size of a single control-plane frame.
///
/// Control frames only ever carry [`protocol::Hello`] / [`protocol::Ack`] /
/// [`protocol::StreamHeader`]; 64 KiB is far above what any of them need and
/// keeps a hostile peer from forcing a large allocation.
pub const MAX_FRAME_LEN: usize = 64 * 1024;
