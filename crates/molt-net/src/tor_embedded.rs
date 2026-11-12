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
//! * **One [`TorClient`] at a time, reused across every dial** (concept §4).
//!   It lives in a process-global [`ArtiShared`] ([`shared`]) so multiple
//!   workspace opens / config re-resolves do not bootstrap several Tor
//!   clients (each would fragment the directory cache and pay the bootstrap
//!   cost again).
//! * **Lazy bootstrap, on the first dial.** Bootstrapping the Tor directory is
//!   slow (tens of seconds, minutes on a cold cache). [`Dialer::resolve`] is
//!   synchronous and may run on the engine actor, so it must not block; it only
//!   mints a cheap [`ArtiShared`] handle. The actual bootstrap happens inside
//!   [`ArtiShared::connect`] on first use, one at a time, and is *not* cached
//!   on failure — a transient bootstrap error is retried on the next dial.
//!   It runs in a task of its own that every waiting dial shares, so a dial
//!   that gives up (a caller's deadline) does not take it down.
//! * **Per-host stream isolation.** Each remote host gets its own
//!   [`IsolationToken`] (minted once, stable thereafter), so arti puts each on
//!   its own circuit — two of our connections never share an exit / timing
//!   fingerprint (concept §4/§5). The same host reuses one circuit.
//! * **State dir `~/.moltrepublic/arti`** (concept §4), split into `state` and
//!   `cache` subdirs, created on bootstrap.
//! * **The client is swappable.** A client that stays unusable
//!   ([`crate::tor_health`]) is dropped and rebuilt from the SAME directories,
//!   so the entry guards survive. Only [`ArtiShared::reset`], the operator's
//!   own act, removes them: a fresh guard set on every hiccup is what guards
//!   exist to prevent. A dial in flight keeps the client it started on.
//!
//! [`Dialer::resolve`]: crate::dial::Dialer::resolve

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use arti_client::config::TorClientConfigBuilder;
use arti_client::{DataStream, ErrorKind, HasKind, IsolationToken, StreamPrefs, TorClient, TorClientConfig};
use tor_rtcompat::PreferredRuntime;

use crate::tor_health::{decide, Evidence, Health, Reason, Verdict};
use crate::NetError;

/// The process-global embedded-Tor state (one shared client + isolation map).
static SHARED: OnceLock<Arc<ArtiShared>> = OnceLock::new();

type Client = Arc<TorClient<PreferredRuntime>>;
type BootFuture = Pin<Box<dyn Future<Output = Result<TorClient<PreferredRuntime>, String>> + Send>>;
type Boot = Box<dyn Fn(TorClientConfig) -> BootFuture + Send + Sync>;

/// How long a swap waits for dials still holding the old client, so two
/// clients do not contend for one state lock.
const REBUILD_DRAIN: Duration = Duration::from_secs(30);
const RESET_DRAIN: Duration = Duration::from_secs(5);
/// A bootstrap that neither ends nor fails is given up after this.
const BOOT_LIMIT: Duration = Duration::from_secs(30 * 60);
const TIMED_OUT: &str = "no result within 30 minutes";

/// What a bootstrap task leaves for the dials waiting on it.
type Booted = Option<Result<Client, String>>;

enum Slot {
    Empty,
    /// A bootstrap in its own task. Every dial waits on this one, and none
    /// of them giving up ends it.
    Booting(tokio::sync::watch::Receiver<Booted>),
    Ready(Client),
    Resetting,
}

/// The client a rebuild replaces, and what its log lines carry.
struct Rebuild {
    old: Client,
    reason: Reason,
    count: u32,
}

/// The process-wide embedded-Tor state: one lazily-bootstrapped [`TorClient`]
/// reused across every dial, plus a per-host [`IsolationToken`] map so each
/// remote host rides its own Tor circuit (stream isolation, concept §4/§5).
pub struct ArtiShared {
    /// Only success is stored: a failed bootstrap leaves the slot empty and
    /// the next dial starts a fresh one.
    slot: Mutex<Slot>,
    /// Bumped on every swap; an outcome from an older client is not evidence
    /// about the current one.
    generation: AtomicU64,
    /// host → isolation token. Same host reuses one circuit (pooling-friendly);
    /// distinct hosts never share an exit / timing fingerprint.
    iso: Mutex<HashMap<String, IsolationToken>>,
    health: Mutex<Health>,
    /// A reset interrupts a bootstrap that may never return.
    resets: tokio::sync::watch::Sender<u64>,
    reset_serial: tokio::sync::Mutex<()>,
    /// `None` = `~/.moltrepublic/arti`.
    base: Option<PathBuf>,
    boot: Boot,
    clock: Instant,
}

impl ArtiShared {
    /// A fresh, unbootstrapped shared state (used by [`shared`] and tests).
    pub fn new() -> ArtiShared {
        ArtiShared::with(None, Box::new(|config| Box::pin(async move {
            TorClient::create_bootstrapped(config).await.map_err(|e| e.to_string())
        })))
    }

    fn with(base: Option<PathBuf>, boot: Boot) -> ArtiShared {
        install_crypto_provider();
        ArtiShared {
            slot: Mutex::new(Slot::Empty),
            generation: AtomicU64::new(0),
            iso: Mutex::new(HashMap::new()),
            health: Mutex::new(Health::default()),
            resets: tokio::sync::watch::Sender::new(0),
            reset_serial: tokio::sync::Mutex::new(()),
            base,
            boot,
            clock: Instant::now(),
        }
    }

    /// The isolation token for `(lane, host)`, minted on first use and stable
    /// thereafter — so every remote host gets its own Tor circuit per lane
    /// and the same pair reuses one (concept §4/§5, [`crate::dial::Dialer::isolated`]).
    pub fn token_for(&self, lane: &str, host: &str) -> IsolationToken {
        let mut map = self.iso.lock().expect("arti isolation map mutex poisoned");
        *map.entry(format!("{lane}\u{0}{host}"))
            .or_insert_with(IsolationToken::new)
    }

    fn health(&self) -> std::sync::MutexGuard<'_, Health> {
        self.health.lock().expect("arti health mutex poisoned")
    }

    /// Lock order: the slot before the health record, never the reverse.
    fn slot(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot.lock().expect("arti client slot mutex poisoned")
    }

    /// The embedded-Tor `(state_dir, cache_dir)` (concept §4). Uses `$HOME`;
    /// a missing `$HOME` is a clean config error.
    fn dirs(&self) -> Result<(PathBuf, PathBuf), NetError> {
        let base = match &self.base {
            Some(base) => base.clone(),
            None => std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty())
                .ok_or_else(|| {
                    NetError::TorMisconfigured(
                        "cannot locate the home directory ($HOME unset) for the arti state dir".into(),
                    )
                })?
                .join(".moltrepublic")
                .join("arti"),
        };
        Ok((base.join("state"), base.join("cache")))
    }

    /// The shared [`TorClient`] and its generation. The first call starts
    /// the bootstrap of the Tor directory (slow) and waits for it; a call
    /// during a bootstrap waits for the same one; later calls reuse the
    /// client. A bootstrap error is not cached.
    async fn client(self: &Arc<Self>) -> Result<(Client, u64), NetError> {
        let (mut booting, generation) = {
            let mut slot = self.slot();
            let generation = self.generation.load(Ordering::SeqCst);
            let booting = match &*slot {
                Slot::Ready(client) => return Ok((client.clone(), generation)),
                Slot::Resetting => return Err(NetError::TorUnavailable("arti: reset in progress".into())),
                Slot::Booting(booting) => booting.clone(),
                Slot::Empty => self.start_boot(&mut slot, &tokio::runtime::Handle::current(), None)?,
            };
            (booting, generation)
        };
        let mut waiter = Waiter(true);
        let booted = booting.wait_for(Option::is_some).await.map(|booted| booted.clone());
        waiter.0 = false;
        match booted {
            Ok(Some(Ok(client))) => Ok((client, generation)),
            Ok(Some(Err(error))) => Err(NetError::TorUnavailable(format!("arti bootstrap failed: {error}"))),
            Ok(None) | Err(_) => Err(NetError::TorUnavailable("arti bootstrap failed: task ended".into())),
        }
    }

    /// Start a bootstrap against the on-disk state / cache dirs in a task of
    /// its own and put it into the slot. A dial that gives up does not end
    /// it: the relay runtime allows a connect 20 s, a stale directory takes
    /// longer to reload, and a bootstrap tied to its first dial never ended.
    fn start_boot(
        self: &Arc<Self>,
        slot: &mut Slot,
        runtime: &tokio::runtime::Handle,
        rebuild: Option<Rebuild>,
    ) -> Result<tokio::sync::watch::Receiver<Booted>, NetError> {
        let (state_dir, cache_dir) = self.dirs()?;
        for dir in [&state_dir, &cache_dir] {
            std::fs::create_dir_all(dir).map_err(|e| {
                NetError::TorMisconfigured(format!("arti dir {}: {e}", dir.display()))
            })?;
        }
        let config = build_config(&state_dir, &cache_dir)?;
        let (done, booting) = tokio::sync::watch::channel(None);
        // subscribed under the slot lock: a reset from here on is seen
        let mut resets = self.resets.subscribe();
        let generation = self.generation.load(Ordering::SeqCst);
        *slot = Slot::Booting(booting.clone());
        let this = self.clone();
        runtime.spawn(async move {
            let started = Instant::now();
            let label = rebuild.as_ref().map(|r| (r.count, r.reason.as_str()));
            match label {
                Some((count, reason)) => tracing::warn!(tor = "embedded", event = "rebuild_start", count, reason),
                None => tracing::info!(tor = "embedded", event = "bootstrap_start"),
            }
            let booted = tokio::select! {
                booted = async {
                    if let Some(rebuild) = rebuild {
                        drain(rebuild.old, REBUILD_DRAIN).await;
                    }
                    (this.boot)(config).await
                } => booted,
                _ = resets.changed() => Err("interrupted by a reset".to_string()),
                _ = tokio::time::sleep(BOOT_LIMIT) => Err(TIMED_OUT.to_string()),
            };
            let booted = this.settle(generation, booted.map(Arc::new));
            let reason = match &booted {
                Err(error) if error == TIMED_OUT => "timeout",
                _ => "error",
            };
            let ms = started.elapsed().as_millis();
            match (&booted, label) {
                (Ok(_), None) => tracing::info!(tor = "embedded", event = "bootstrap_ok", ms),
                (Ok(_), Some((count, _))) => tracing::info!(tor = "embedded", event = "rebuild_ok", count, ms),
                (Err(error), None) => {
                    tracing::warn!(tor = "embedded", event = "bootstrap_failed", reason, ms, error = %error);
                }
                (Err(error), Some((count, _))) => {
                    tracing::warn!(tor = "embedded", event = "rebuild_failed", count, reason, ms, error = %error);
                }
            }
            let _ = done.send(Some(booted));
        });
        Ok(booting)
    }

    /// Put a finished bootstrap into the slot, unless a reset took the slot
    /// away meanwhile.
    fn settle(&self, generation: u64, booted: Result<Client, String>) -> Result<Client, String> {
        let mut slot = self.slot();
        if generation != self.generation.load(Ordering::SeqCst) {
            return Err("interrupted by a reset".to_string());
        }
        match &booted {
            Ok(client) => *slot = Slot::Ready(client.clone()),
            Err(_) => {
                *slot = Slot::Empty;
                self.health().record(self.clock.elapsed(), "", Evidence::Client);
            }
        }
        booted
    }

    /// Dial `host:port` over the embedded Tor client, on the host's own circuit.
    /// The host is resolved **in-circuit** (no local DNS). The returned
    /// [`DataStream`] is `AsyncRead + AsyncWrite + Unpin + Send`, so a TLS
    /// handshake rides straight over it.
    pub async fn connect(self: &Arc<Self>, lane: &str, host: &str, port: u16) -> Result<DataStream, NetError> {
        let (client, generation) = self.client().await?;
        let token = self.token_for(lane, host);
        let mut prefs = StreamPrefs::new();
        prefs.set_isolation(token);
        // a caller's deadline dropping this dial is a failure nobody else reports
        let mut cut = CutShort(Some((self.clone(), generation, host.to_string())));
        let out = client.connect_with_prefs((host, port), &prefs).await;
        cut.0 = None;
        let (evidence, error) = match &out {
            Ok(_) => (Some(Evidence::Alive), String::new()),
            Err(e) => (evidence_of(e.kind()), e.to_string()),
        };
        if let Some(evidence) = evidence {
            let status = client.bootstrap_status();
            let blocked = status.blocked().map(|b| b.to_string());
            let ready = status.ready_for_traffic() && blocked.is_none();
            self.observe(self.clock.elapsed(), generation, host, evidence, Some(ready), &blocked.unwrap_or(error));
        }
        out.map_err(|e| NetError::TorUnavailable(format!("arti dial {host}:{port}: {e}")))
    }

    /// Feed one dial outcome to the health record and act on the verdict:
    /// a rebuild drops the client and bootstraps a new one from the same
    /// directories.
    fn observe(
        self: &Arc<Self>,
        now: Duration,
        generation: u64,
        host: &str,
        evidence: Evidence,
        ready: Option<bool>,
        detail: &str,
    ) {
        if generation != self.generation.load(Ordering::SeqCst) {
            return;
        }
        let reason = {
            let mut health = self.health();
            health.record(now, host, evidence);
            // a stream that opened outranks arti's own estimate
            if evidence != Evidence::Alive {
                if let Some(ready) = ready {
                    health.record_ready(now, ready);
                }
            }
            let verdict = decide(&health.input(now));
            let (Verdict::Wait(reason) | Verdict::Rebuild(reason)) = verdict else {
                return;
            };
            if health.flag() {
                tracing::warn!(tor = "embedded", event = "health_bad", reason = reason.as_str(), host, detail);
            }
            if matches!(verdict, Verdict::Wait(_)) {
                return;
            }
            reason
        };
        // a dial dropped at runtime shutdown has nowhere to rebuild
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut slot = self.slot();
        // only a client that is there can be rebuilt, and only once
        if generation != self.generation.load(Ordering::SeqCst) || !matches!(*slot, Slot::Ready(_)) {
            return;
        }
        let Slot::Ready(old) = std::mem::replace(&mut *slot, Slot::Empty) else {
            return;
        };
        self.generation.fetch_add(1, Ordering::SeqCst);
        let count = self.health().rebuilt(now);
        if let Err(error) = self.start_boot(&mut slot, &runtime, Some(Rebuild { old, reason, count })) {
            tracing::warn!(tor = "embedded", event = "rebuild_failed", count, ms = 0, error = %error);
        }
    }

    /// The operator's reset: drop the client and REMOVE its state and cache
    /// directories, entry guards included. The next dial bootstraps from
    /// scratch. Returns what was removed.
    pub async fn reset(&self) -> Result<Vec<PathBuf>, NetError> {
        let _serial = self.reset_serial.lock().await;
        let prior = {
            let mut slot = self.slot();
            self.generation.fetch_add(1, Ordering::SeqCst);
            std::mem::replace(&mut *slot, Slot::Resetting)
        };
        // no dial may bootstrap into directories about to be removed
        let _reopen = Reopen(self);
        self.resets.send_modify(|n| *n += 1);
        match prior {
            Slot::Ready(old) => drain(old, RESET_DRAIN).await,
            Slot::Booting(mut booting) => {
                let _ = tokio::time::timeout(RESET_DRAIN, booting.wait_for(Option::is_some)).await;
            }
            Slot::Empty | Slot::Resetting => {}
        }
        self.iso.lock().expect("arti isolation map mutex poisoned").clear();
        *self.health() = Health::default();
        let (state_dir, cache_dir) = self.dirs()?;
        let removed = tokio::task::spawn_blocking(move || {
            let mut removed = Vec::new();
            for dir in [state_dir, cache_dir] {
                match std::fs::remove_dir_all(&dir) {
                    Ok(()) => removed.push(dir),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(NetError::TorMisconfigured(format!("arti dir {}: {e}", dir.display()))),
                }
            }
            Ok(removed)
        })
        .await
        .map_err(|e| NetError::TorMisconfigured(format!("arti reset: {e}")))??;
        let listed: Vec<String> = removed.iter().map(|p| p.display().to_string()).collect();
        tracing::info!(tor = "embedded", event = "reset", removed = %listed.join(","));
        Ok(removed)
    }
}

/// Let go of a replaced client once the dials still holding it are done or
/// `limit` is over.
async fn drain(old: Client, limit: Duration) {
    let until = Instant::now() + limit;
    while Arc::strong_count(&old) > 1 && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Opens the slot again when a reset ends, however it ends.
struct Reopen<'a>(&'a ArtiShared);

impl Drop for Reopen<'_> {
    fn drop(&mut self) {
        let mut slot = self.0.slot();
        if matches!(*slot, Slot::Resetting) {
            *slot = Slot::Empty;
        }
    }
}

/// A dial that stopped waiting for the bootstrap; the bootstrap goes on.
struct Waiter(bool);

impl Drop for Waiter {
    fn drop(&mut self) {
        if self.0 {
            tracing::debug!(tor = "embedded", event = "waiter_left");
        }
    }
}

/// Records a dial dropped before arti answered.
struct CutShort(Option<(Arc<ArtiShared>, u64, String)>);

impl Drop for CutShort {
    fn drop(&mut self) {
        if let Some((shared, generation, host)) = self.0.take() {
            shared.observe(shared.clock.elapsed(), generation, &host, Evidence::Ambiguous, None, "dial dropped by its caller");
        }
    }
}

/// What a failed dial's kind says about the client. The far end answering,
/// in whatever way, proves the client works; a timeout inside the Tor
/// network is what one dead onion service looks like too; an address arti
/// refuses to dial says nothing.
fn evidence_of(kind: ErrorKind) -> Option<Evidence> {
    use ErrorKind as K;
    Some(match kind {
        K::InvalidStreamTarget
        | K::ForbiddenStreamTarget
        | K::OnionServiceAddressInvalid
        | K::BadApiUsage
        | K::FeatureDisabled
        | K::NotImplemented => return None,
        K::RemoteStreamClosed
        | K::RemoteStreamReset
        | K::RemoteStreamError
        | K::RemoteConnectionRefused
        | K::ExitPolicyRejected
        | K::ExitTimeout
        | K::RemoteNetworkFailed
        | K::RemoteNetworkTimeout
        | K::RemoteHostNotFound
        | K::RemoteHostResolutionFailed
        | K::RemoteProtocolViolation
        | K::OnionServiceNotFound
        | K::OnionServiceNotRunning
        | K::OnionServiceProtocolViolation
        | K::OnionServiceConnectionFailed
        | K::OnionServiceMissingClientAuth
        | K::OnionServiceWrongClientAuth => Evidence::Alive,
        K::TorNetworkTimeout
        | K::TransientFailure
        | K::CircuitCollapse
        | K::CircuitRefused
        | K::RelayTooBusy
        | K::NoExit => Evidence::Ambiguous,
        _ => Evidence::Client,
    })
}

impl Default for ArtiShared {
    fn default() -> ArtiShared {
        ArtiShared::new()
    }
}

/// The process-global shared arti state — the **single** [`ArtiShared`] every
/// embedded dial goes through (concept §4). Cheap: returns an [`Arc`] clone,
/// no bootstrap.
pub fn shared() -> Arc<ArtiShared> {
    SHARED
        .get_or_init(|| Arc::new(ArtiShared::new()))
        .clone()
}

/// arti's TLS (tor-rtcompat) builds its `ClientConfig` on the PROCESS-level
/// rustls provider, and this ring-free graph has none to auto-select - the
/// first embedded dial would panic (v0.0.2 field report). Install the
/// crate's X25519 rustcrypto provider; an already-installed one wins.
fn install_crypto_provider() {
    let _ = crate::dial::x25519_provider().install_default();
}

/// Build the arti client config pinned to the given state and cache dirs. Split
/// out so the state-dir wiring is unit-testable without bootstrapping over
/// the network.
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
    use std::sync::atomic::AtomicUsize;

    /// arti's TLS (tor-rtcompat) builds its `ClientConfig` on the PROCESS
    /// default provider. Our graph is ring-free, so nothing auto-installs
    /// one: v0.0.2 aborted on the first embedded dial. Creating the shared
    /// state must install the crate's own provider first.
    #[test]
    fn the_shared_state_installs_the_rustcrypto_provider_as_process_default() {
        let _shared = ArtiShared::new();
        let provider = rustls::crypto::CryptoProvider::get_default()
            .expect("a process-level provider is installed before arti runs");
        let groups: Vec<_> = provider.kx_groups.iter().map(|g| g.name()).collect();
        assert_eq!(groups, vec![rustls::NamedGroup::X25519], "the crate's X25519 rustcrypto provider");
    }

    #[test]
    fn per_server_isolation_yields_distinct_circuits() {
        // distinct remote hosts, and distinct lanes to one host, get distinct
        // isolation tokens (⇒ distinct Tor circuits); the same lane+host
        // reuses one (concept §4/§5, 83d72c39). Pure — no network, no bootstrap.
        let shared = ArtiShared::new();
        let a1 = shared.token_for("relay", "a.example");
        let a2 = shared.token_for("relay", "a.example");
        let b = shared.token_for("relay", "b.example");
        let c = shared.token_for("s3", "a.example");
        assert_eq!(a1, a2, "same lane and host must reuse one isolation token/circuit");
        assert_ne!(a1, b, "distinct hosts must get distinct isolation tokens/circuits");
        assert_ne!(a1, c, "distinct lanes to one host must not share a circuit");
    }

    /// A shared state on a tempdir whose "bootstrap" builds a client
    /// without touching the network; the `fail`-th attempts error out.
    fn offline(base: &Path, fail: &'static [usize]) -> (Arc<ArtiShared>, Arc<AtomicUsize>) {
        let boots = Arc::new(AtomicUsize::new(0));
        let counter = boots.clone();
        let boot: Boot = Box::new(move |config| {
            let attempt = counter.fetch_add(1, Ordering::SeqCst) + 1;
            Box::pin(async move {
                if fail.contains(&attempt) {
                    return Err("no network".to_string());
                }
                TorClient::builder().config(config).create_unbootstrapped().map_err(|e| e.to_string())
            })
        });
        (Arc::new(ArtiShared::with(Some(base.to_path_buf()), boot)), boots)
    }

    fn min(m: u64) -> Duration {
        Duration::from_secs(m * 60)
    }

    async fn settled(shared: &ArtiShared) {
        tokio::time::timeout(Duration::from_secs(20), async {
            while matches!(*shared.slot(), Slot::Booting(_)) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the rebuild finishes");
    }

    #[tokio::test]
    async fn the_client_boots_on_first_use_and_a_failure_is_not_cached() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (shared, boots) = offline(dir.path(), &[1]);
        assert_eq!(boots.load(Ordering::SeqCst), 0, "nothing boots before a dial");
        assert!(matches!(shared.client().await, Err(NetError::TorUnavailable(_))));
        let (first, generation) = shared.client().await.expect("the retry boots");
        let (second, same) = shared.client().await.expect("reused");
        assert!(Arc::ptr_eq(&first, &second), "one client per process");
        assert_eq!((generation, same, boots.load(Ordering::SeqCst)), (0, 0, 2));
    }

    /// A caller's deadline (the relay runtime gives a connect 20 s) used to
    /// take the bootstrap down with it, every time.
    #[tokio::test]
    async fn a_bootstrap_outlives_the_dial_that_started_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let boots = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (counter, held) = (boots.clone(), gate.clone());
        let boot: Boot = Box::new(move |config| {
            counter.fetch_add(1, Ordering::SeqCst);
            let held = held.clone();
            Box::pin(async move {
                let _open = held.acquire().await.map_err(|e| e.to_string())?;
                TorClient::builder().config(config).create_unbootstrapped().map_err(|e| e.to_string())
            })
        });
        let shared = Arc::new(ArtiShared::with(Some(dir.path().to_path_buf()), boot));
        let dial = |shared: &Arc<ArtiShared>| {
            let shared = shared.clone();
            tokio::spawn(async move { shared.client().await.map(|(client, _)| client) })
        };
        let first = dial(&shared);
        while boots.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        first.abort();
        assert!(first.await.is_err(), "dropped mid-bootstrap");

        let (second, third) = (dial(&shared), dial(&shared));
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate.add_permits(1);
        let second = second.await.expect("joined").expect("the running bootstrap's client");
        let third = third.await.expect("joined").expect("shared by every waiter");
        assert!(Arc::ptr_eq(&second, &third));
        assert_eq!(boots.load(Ordering::SeqCst), 1, "one bootstrap, whoever gave up on it");
    }

    #[tokio::test(start_paused = true)]
    async fn a_bootstrap_that_never_ends_is_given_up_after_the_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let boots = Arc::new(AtomicUsize::new(0));
        let counter = boots.clone();
        let boot: Boot = Box::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        });
        let shared = Arc::new(ArtiShared::with(Some(dir.path().to_path_buf()), boot));
        let dial = |shared: &Arc<ArtiShared>| {
            let shared = shared.clone();
            tokio::spawn(async move { shared.client().await.map(|_| ()) })
        };
        let first = dial(&shared);
        tokio::time::sleep(BOOT_LIMIT - Duration::from_secs(60)).await;
        let second = dial(&shared);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(boots.load(Ordering::SeqCst), 1, "inside the bound a dial joins the running bootstrap");
        assert_eq!(shared.health().input(Duration::ZERO).failures, 0);

        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(first.is_finished() && second.is_finished(), "the waiters are told");
        assert!(matches!(first.await.expect("joined"), Err(NetError::TorUnavailable(_))));
        assert_eq!(shared.health().input(Duration::ZERO).failures, 1, "one failure against the client");
        let _third = dial(&shared);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(boots.load(Ordering::SeqCst), 2, "the next dial starts a fresh bootstrap");
    }

    #[tokio::test]
    async fn a_sustained_failure_swaps_the_client_and_keeps_its_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (shared, boots) = offline(dir.path(), &[]);
        let (old, generation) = shared.client().await.expect("boots");
        let token = shared.token_for("relay", "a.example");
        let guards = dir.path().join("state").join("marker");
        std::fs::write(&guards, b"kept").expect("marker");
        for m in 0..10 {
            shared.observe(min(m), generation, "a.example", Evidence::Client, Some(true), "x");
        }
        assert_eq!(boots.load(Ordering::SeqCst), 1, "below the threshold nothing is rebuilt");
        drop(old);
        shared.observe(min(10), generation, "a.example", Evidence::Client, Some(true), "x");
        let (_new, swapped) = shared.client().await.expect("a dial joins the rebuild");
        assert_eq!((swapped, boots.load(Ordering::SeqCst)), (generation + 1, 2));
        assert_eq!(std::fs::read(&guards).expect("still there"), b"kept", "a rebuild removes nothing");
        assert_eq!(token, shared.token_for("relay", "a.example"), "circuits stay isolated as before");

        // a dial that started on the old client says nothing about the new one
        shared.observe(min(11), generation, "a.example", Evidence::Client, Some(false), "x");
        assert_eq!(shared.health().input(min(11)).failures, 0);
        // and the next rebuild waits out the backoff
        for m in 11..30 {
            shared.observe(min(m), swapped, "a.example", Evidence::Client, Some(true), "x");
        }
        settled(&shared).await;
        assert_eq!(boots.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn one_dead_host_never_swaps_the_client() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (shared, boots) = offline(dir.path(), &[]);
        let (_client, generation) = shared.client().await.expect("boots");
        for m in 0..120 {
            shared.observe(min(m), generation, "dead.example", Evidence::Ambiguous, Some(true), "x");
            shared.observe(min(m), generation, "live.example", Evidence::Alive, Some(true), "");
        }
        settled(&shared).await;
        assert_eq!((boots.load(Ordering::SeqCst), shared.generation.load(Ordering::SeqCst)), (1, 0));
    }

    #[tokio::test]
    async fn a_reset_drops_the_client_and_removes_state_and_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (shared, boots) = offline(dir.path(), &[]);
        let (old, _) = shared.client().await.expect("boots");
        drop(old);
        let token = shared.token_for("relay", "a.example");
        let (state, cache) = (dir.path().join("state"), dir.path().join("cache"));
        std::fs::write(state.join("marker"), b"guards").expect("marker");
        let beside = dir.path().join("beside");
        std::fs::write(&beside, b"not ours").expect("neighbour");

        let removed = shared.reset().await.expect("reset");
        assert_eq!(removed, vec![state.clone(), cache.clone()]);
        assert!(!state.exists() && !cache.exists(), "state and cache are gone");
        assert!(beside.exists(), "nothing else is touched");
        assert_ne!(token, shared.token_for("relay", "a.example"), "fresh circuits");
        assert_eq!(boots.load(Ordering::SeqCst), 1, "the reset itself boots nothing");

        let (_new, generation) = shared.client().await.expect("the next dial boots from scratch");
        assert_eq!((generation, boots.load(Ordering::SeqCst)), (1, 2));
        assert!(state.exists());
        assert_eq!(shared.reset().await.expect("again").len(), 2);
        assert!(shared.reset().await.expect("nothing left").is_empty());
    }

    #[tokio::test]
    async fn a_reset_interrupts_a_bootstrap_that_never_returns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let boot: Boot = Box::new(|_| Box::pin(std::future::pending()));
        let shared = Arc::new(ArtiShared::with(Some(dir.path().to_path_buf()), boot));
        let dial = tokio::spawn({
            let shared = shared.clone();
            async move { shared.client().await.map(|_| ()) }
        });
        while !matches!(*shared.slot(), Slot::Booting(_)) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::timeout(Duration::from_secs(10), shared.reset())
            .await
            .expect("the reset does not wait for the bootstrap")
            .expect("reset");
        assert!(matches!(dial.await.expect("joined"), Err(NetError::TorUnavailable(_))));
    }

    #[test]
    fn only_a_tor_level_failure_counts_against_the_client() {
        use ErrorKind as K;
        for kind in [K::RemoteConnectionRefused, K::OnionServiceNotFound, K::OnionServiceNotRunning, K::RemoteHostNotFound] {
            assert_eq!(evidence_of(kind), Some(Evidence::Alive), "{kind:?}");
        }
        assert_eq!(evidence_of(K::TorNetworkTimeout), Some(Evidence::Ambiguous));
        assert_eq!(evidence_of(K::InvalidStreamTarget), None, "a refused address is no evidence");
        for kind in [K::TorAccessFailed, K::TorDirectoryUnusable, K::DirectoryExpired, K::NoPath, K::LocalNetworkError] {
            assert_eq!(evidence_of(kind), Some(Evidence::Client), "{kind:?}");
        }
    }

    #[test]
    fn arti_bootstraps_a_client_to_a_state_dir() {
        // the bootstrap-to-state-dir wiring constructs without a live Tor
        // network: a config pinned to a tempdir state/cache builds cleanly.
        // (The full bootstrap + dial is the #[ignore]d `tor_embedded_smoke` test.)
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

}
