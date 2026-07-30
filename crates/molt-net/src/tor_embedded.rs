// SPDX-License-Identifier: GPL-3.0-or-later

//! Embedded, in-process Tor via **arti** (transport concept §4, T4 §P3 / §B2).
//!
//! Only compiled under `--features embedded-tor` (the default build never pulls
//! arti — see the `[features]` note in `Cargo.toml`; the opt-in feature accepts
//! one C dependency, `libsqlite3-sys` via `tor-dirmgr`, per the 2026-07-11
//! decision).
//!
//! Design:
//!
//! * **One [`TorClient`], reused across every dial** (concept §4: "a single
//!   `TorClient` reused across dials"). It lives as a process-global singleton
//!   ([`shared`]) so multiple workspace opens / config re-resolves do not
//!   bootstrap several Tor clients (each would fragment the directory cache and
//!   pay the bootstrap cost again).
//! * **Lazy bootstrap, on the first dial.** Bootstrapping the Tor directory is
//!   slow (tens of seconds, minutes on a cold cache). [`Dialer::resolve`] is
//!   synchronous and may run on the engine actor, so it must not block; it only
//!   mints a cheap [`ArtiShared`] handle. The actual bootstrap happens inside
//!   [`ArtiShared::connect`] on first use ([`tokio::sync::OnceCell`]), and is
//!   *not* cached on failure — a transient bootstrap error is retried on the
//!   next dial.
//! * **Per-host stream isolation.** Each remote host gets its own
//!   [`IsolationToken`] (minted once, stable thereafter), so arti puts each on
//!   its own circuit — two of our connections never share an exit / timing
//!   fingerprint (concept §4/§5). The same host reuses one circuit.
//! * **State dir `~/.moltrepublic/arti`** (concept §4), split into `state` and
//!   `cache` subdirs, created on bootstrap.
//!
//! [`Dialer::resolve`]: crate::dial::Dialer::resolve

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use arti_client::config::TorClientConfigBuilder;
use arti_client::{DataStream, IsolationToken, StreamPrefs, TorClient, TorClientConfig};
use tokio::sync::OnceCell;
use tor_rtcompat::PreferredRuntime;

use crate::NetError;

/// The process-global embedded-Tor state (one shared client + isolation map).
static SHARED: OnceLock<Arc<ArtiShared>> = OnceLock::new();

/// The process-wide embedded-Tor state: one lazily-bootstrapped [`TorClient`]
/// reused across every dial, plus a per-host [`IsolationToken`] map so each
/// remote host rides its own Tor circuit (stream isolation, concept §4/§5).
pub struct ArtiShared {
    /// The bootstrapped arti client, created lazily on the first dial (bootstrap
    /// is slow, so `resolve` stays synchronous). `get_or_try_init` caches only
    /// success, so a transient bootstrap failure is retried on the next dial.
    client: OnceCell<TorClient<PreferredRuntime>>,
    /// host → isolation token. Same host reuses one circuit (pooling-friendly);
    /// distinct hosts never share an exit / timing fingerprint.
    iso: Mutex<HashMap<String, IsolationToken>>,
}

impl ArtiShared {
    /// A fresh, unbootstrapped shared state (used by [`shared`] and tests).
    pub fn new() -> ArtiShared {
        ArtiShared {
            client: OnceCell::new(),
            iso: Mutex::new(HashMap::new()),
        }
    }

    /// The isolation token for `host`, minted on first use and stable
    /// thereafter — so every remote host gets its own Tor circuit and the
    /// same host reuses one (concept §4/§5).
    pub fn token_for(&self, host: &str) -> IsolationToken {
        let mut map = self.iso.lock().expect("arti isolation map mutex poisoned");
        *map.entry(host.to_string()).or_insert_with(IsolationToken::new)
    }

    /// The lazily-bootstrapped shared [`TorClient`]. The first call bootstraps
    /// the Tor directory (slow); later calls reuse it. A bootstrap error is not
    /// cached, so it is retried on the next dial.
    async fn client(&self) -> Result<&TorClient<PreferredRuntime>, NetError> {
        self.client.get_or_try_init(bootstrap_client).await
    }

    /// Dial `host:port` over the embedded Tor client, on the host's own circuit.
    /// The host is resolved **in-circuit** (no local DNS). The returned
    /// [`DataStream`] is `AsyncRead + AsyncWrite + Unpin + Send`, so a TLS
    /// handshake rides straight over it.
    pub async fn connect(&self, host: &str, port: u16) -> Result<DataStream, NetError> {
        let client = self.client().await?;
        let token = self.token_for(host);
        let mut prefs = StreamPrefs::new();
        prefs.set_isolation(token);
        client
            .connect_with_prefs((host, port), &prefs)
            .await
            .map_err(|e| NetError::TorUnavailable(format!("arti dial {host}:{port}: {e}")))
    }
}

impl Default for ArtiShared {
    fn default() -> ArtiShared {
        ArtiShared::new()
    }
}

/// The process-global shared arti state — the **single** [`TorClient`] reused
/// across every embedded dial (concept §4). Cheap: returns an [`Arc`] clone,
/// no bootstrap.
pub fn shared() -> Arc<ArtiShared> {
    SHARED
        .get_or_init(|| Arc::new(ArtiShared::new()))
        .clone()
}

/// Bootstrap a fresh arti [`TorClient`] against the on-disk state / cache dirs
/// under `~/.moltrepublic/arti`. Slow (Tor directory bootstrap over the
/// network); called once, lazily, on the first dial.
async fn bootstrap_client() -> Result<TorClient<PreferredRuntime>, NetError> {
    let (state_dir, cache_dir) = arti_dirs()?;
    for dir in [&state_dir, &cache_dir] {
        std::fs::create_dir_all(dir).map_err(|e| {
            NetError::TorMisconfigured(format!("arti dir {}: {e}", dir.display()))
        })?;
    }
    let config = build_config(&state_dir, &cache_dir)?;
    TorClient::create_bootstrapped(config)
        .await
        .map_err(|e| NetError::TorUnavailable(format!("arti bootstrap failed: {e}")))
}

/// The embedded-Tor `(state_dir, cache_dir)` under `~/.moltrepublic/arti`
/// (concept §4). Uses `$HOME`; a missing `$HOME` is a clean config error.
fn arti_dirs() -> Result<(PathBuf, PathBuf), NetError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            NetError::TorMisconfigured(
                "cannot locate the home directory ($HOME unset) for the arti state dir".into(),
            )
        })?;
    let base = home.join(".moltrepublic").join("arti");
    Ok((base.join("state"), base.join("cache")))
}

/// Build the arti client config pinned to the given state and cache dirs. Split
/// out from [`bootstrap_client`] so the state-dir wiring is unit-testable
/// without bootstrapping over the network.
pub(crate) fn build_config(
    state_dir: &Path,
    cache_dir: &Path,
) -> Result<TorClientConfig, NetError> {
    TorClientConfigBuilder::from_directories(state_dir, cache_dir)
        .build()
        .map_err(|e| NetError::TorMisconfigured(format!("arti config: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_server_isolation_yields_distinct_circuits() {
        // distinct remote hosts get distinct isolation tokens (⇒ distinct
        // Tor circuits); the same host reuses one (concept §4/§5). Pure — no
        // network, no bootstrap.
        let shared = ArtiShared::new();
        let a1 = shared.token_for("smp.a.example");
        let a2 = shared.token_for("smp.a.example");
        let b = shared.token_for("smp.b.example");
        assert_eq!(a1, a2, "same host must reuse one isolation token/circuit");
        assert_ne!(
            a1, b,
            "distinct hosts must get distinct isolation tokens/circuits"
        );
    }

    #[test]
    fn arti_bootstraps_a_client_to_a_state_dir() {
        // the bootstrap-to-state-dir wiring constructs without a live Tor
        // network: a config pinned to a tempdir state/cache builds cleanly.
        // (The full bootstrap + dial is the #[ignore]d live-tor test below.)
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state");
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&state).expect("state dir");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let config = build_config(&state, &cache);
        assert!(
            config.is_ok(),
            "arti config must build to the state dir: {config:?}"
        );
    }

    #[tokio::test]
    #[ignore = "live tor: full bootstrap + dial needs the real Tor network"]
    async fn dialer_arti_dials_an_smp_host() {
        // end-to-end: bootstrap the embedded client and open a Tor circuit to a
        // known live host. Ignored by default (needs live Tor + network egress).
        let shared = ArtiShared::new();
        let stream = shared.connect("smp.konkin.io", 5223).await;
        assert!(stream.is_ok(), "arti dial failed: {stream:?}");
    }
}
