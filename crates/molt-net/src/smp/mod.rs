// SPDX-License-Identifier: GPL-3.0-or-later

//! The real SMP transport (transport concept §3, milestone T3).
//!
//! Built incrementally and verified against a live SimpleX server:
//!
//! * [`server`] — `smp://<fp>@host` addressing.
//! * [`tls`] — pinned-fingerprint TLS 1.3 + ALPN `smp/1` (pure-Rust rustls).
//! * (next) the SMP transport handshake and the `NEW`/`KEY`/`SUB`/`SEND`/
//!   `ACK` command layer, then `SmpTransport: Transport`.
//!
//! The `Transport` trait is unchanged, so the engine and the founding
//! ritual run over `SmpTransport` exactly as they run over
//! `LoopbackTransport` today.

pub mod conn;
pub mod ed448;
pub mod server;
pub mod tls;
pub mod transport;

pub use conn::SmpConn;
pub use server::{SmpServer, SMP_PORT};
pub use tls::test_connection;
pub use transport::SmpTransport;
