// SPDX-License-Identifier: GPL-3.0-or-later

//! The Tor connectivity probe: "is Tor actually there, and does it work?"
//!
//! # The honest ladder
//!
//! A connectivity test that lights up green when a socket answered is worse
//! than no test at all — it teaches the operator to trust a signal that means
//! nothing. So this probe reports the RUNG it reached, never a boolean:
//!
//! 1. [`TorTestState::Off`] / [`TorTestState::Misconfigured`] — nothing was
//!    probed at all (Tor is not selected, or the fail-closed dialer refused
//!    to resolve the configuration).
//! 2. [`TorTestState::NoProxy`] — nothing is listening at the configured
//!    SOCKS address, so there is no Tor daemon there.
//! 3. [`TorTestState::ProxyOnly`] — a socket answered there. That is
//!    consistent with a running Tor daemon and is **all** it proves: no
//!    traffic was routed through it, so there is no evidence of a circuit.
//! 4. [`TorTestState::CircuitFailed`] — the proxy answered but the dial
//!    through it failed. Explicitly *not* a working Tor.
//! 5. [`TorTestState::Circuit`] — a relay was reached END TO END through
//!    Tor: the SOCKS5 negotiation, the CONNECT and the circuit all
//!    completed. The only rung that means "Tor works".
//!
//! # Why the proxy rung is only a TCP reachability check
//!
//! Completing the SOCKS5 method negotiation would be stronger evidence, but
//! every SOCKS client worth using — `tokio-socks`, the one this crate already
//! dials Tor with — performs the negotiation and the CONNECT as one
//! operation, because SOCKS5 has no "negotiate only" exchange. Splitting it
//! would mean hand-rolling the SOCKS wire format, exactly the layer that was
//! *replaced* by `tokio-socks` for correctness reasons
//! (`mdk_evaluation.md` §7.7); driving the full exchange would need a target
//! host, and inventing one violates ADR-0004 (nothing is pre-configured —
//! the app dials no host its operator did not name). So the middle rung
//! claims only what a TCP connect proves, and the copy on every surface must
//! say exactly that. The strong evidence comes from rung 5, which uses the
//! REAL dialer against the operator's OWN confirmed relay.
//!
//! Every rung is deadline-bounded ([`PROXY_TIMEOUT`], [`CIRCUIT_TIMEOUT`]),
//! so a hung Tor daemon can never wedge the caller.

use std::time::Duration;

use molt_core::relay::{dialable, relay_kind, PoolGap, RelayEntry, RelayKind};
use molt_core::{TorTest, TorTestState};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::dial::Dialer;

/// Deadline for the proxy reachability rung. A local (or Whonix-gateway)
/// SOCKS listener either answers immediately or is not there.
pub const PROXY_TIMEOUT: Duration = Duration::from_secs(5);

/// Deadline for the circuit rung. Generous enough for a cold Tor circuit
/// (the dialer's own `CONNECT_TIMEOUT` fires first on the SOCKS path) and
/// for a partial embedded-arti bootstrap, but bounded — a probe that never
/// returns is a probe that lies about being in progress.
pub const CIRCUIT_TIMEOUT: Duration = Duration::from_secs(45);

/// What each rung of the probe observed. `None` means the rung did not run.
///
/// Kept as data so the ladder itself ([`verdict`]) is a pure, exhaustively
/// testable function — the honesty of this feature lives in that mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RungReport {
    /// The SOCKS address the proxy rung addressed. Empty when this Tor mode
    /// has no SOCKS listener (the embedded in-process client).
    pub proxy: String,
    /// `Some(Ok(()))` = a socket answered at [`Self::proxy`];
    /// `Some(Err(reason))` = it did not; `None` = the rung did not run.
    pub proxy_answered: Option<Result<(), String>>,
    /// The relay URL the circuit rung dialed. Empty when none was dialable.
    pub target: String,
    /// `Some(Ok(ms))` = the dial through Tor completed;
    /// `Some(Err(reason))` = it failed; `None` = the rung did not run.
    pub circuit: Option<Result<u32, String>>,
}

/// THE ladder: map what the rungs observed onto the strongest claim that is
/// actually supported — and no further. Pure and total.
pub fn verdict(r: &RungReport) -> TorTest {
    match (&r.proxy_answered, &r.circuit) {
        // the end-to-end proof: a real relay, reached through Tor.
        (_, Some(Ok(ms))) => TorTest {
            state: TorTestState::Circuit,
            detail: String::new(),
            proxy: r.proxy.clone(),
            target: r.target.clone(),
            ms: *ms,
        },
        // a DEADLINE is not a refusal — say so separately.
        (_, Some(Err(why))) if why.starts_with(TIMEOUT_MARK) => TorTest {
            state: TorTestState::CircuitTimeout,
            detail: why.clone(),
            proxy: r.proxy.clone(),
            target: r.target.clone(),
            ms: 0,
        },
        // the dial through Tor failed. This does NOT single out Tor: the
        // relay may simply be down, firewalled or misconfigured, and the
        // surfaces must say so (review finding 2026-07-31).
        (_, Some(Err(why))) => TorTest {
            state: TorTestState::CircuitFailed,
            detail: why.clone(),
            proxy: r.proxy.clone(),
            target: r.target.clone(),
            ms: 0,
        },
        // nothing is listening at the SOCKS address.
        (Some(Err(why)), None) => TorTest {
            state: TorTestState::NoProxy,
            detail: why.clone(),
            proxy: r.proxy.clone(),
            target: String::new(),
            ms: 0,
        },
        // a socket answered — and that is ALL that was established.
        (Some(Ok(())), None) => TorTest {
            state: TorTestState::ProxyOnly,
            // states only what this rung observed. It has no pool to look
            // at, so it must NOT assert "nothing is confirmed" — that was the
            // same misdiagnosis the join/founding refusals carried (an
            // operator whose relays ARE confirmed, blocked by the non-onion
            // switch, told they had confirmed none). TODO: thread
            // `TargetGap` in so this can name the actual cause.
            detail: "nothing was routed through the proxy, so no circuit was proven — \
                     no relay from the pool was reachable through Tor (see the relay \
                     settings for which of them this node may dial)"
                .to_string(),
            proxy: r.proxy.clone(),
            target: String::new(),
            ms: 0,
        },
        // no proxy to probe and nothing to dial: not a single rung ran.
        (None, None) => TorTest {
            state: TorTestState::NoTarget,
            detail: "no SOCKS proxy to probe and no relay this node may dial through Tor \
                     — nothing about Tor could be established"
                .to_string(),
            proxy: r.proxy.clone(),
            target: String::new(),
            ms: 0,
        },
    }
}

/// The relay this probe may dial, or `None`.
///
/// It is the pool's OWN dial policy ([`dialable`] — confirmed, and clearnet
/// or local only once non-onion dialing is switched on), minus one rule that
/// is specific to this test: a [`RelayKind::Local`] relay is reached
/// DIRECTLY and never through Tor (`relay_ws::dialer_for`), so dialing it
/// could never prove a circuit. Pool ORDER is the priority, here as
/// everywhere. Nothing is ever invented: an empty result means "the operator
/// has given us no destination", which the verdict states plainly.
pub fn probe_target(relays: &[RelayEntry], clearnet_session: bool) -> Option<String> {
    dialable(relays, clearnet_session)
        .into_iter()
        .find(|url| relay_kind(url) != RelayKind::Local)
}

/// WHY no relay could be dialed — the difference matters to the operator,
/// because the fixes are different (review finding 2026-07-31: telling
/// someone to "add and confirm a relay" for one they already confirmed is
/// advice that cannot help).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetGap {
    /// No relay in the pool at all.
    EmptyPool,
    /// Relays exist, none confirmed.
    Unconfirmed,
    /// Confirmed, but non-onion dialing is switched off — which is ALSO the
    /// reason they would never prove a circuit. (Name kept from when the
    /// switch was session-scoped; since the ADR-0004 amendment it is
    /// persisted, so this is a standing state, not a per-start one.)
    SessionLocked,
    /// Only local relays, which bypass Tor by design.
    LocalOnly,
}

/// Classify why [`probe_target`] found nothing. Only called when it did.
///
/// The "why can this node dial nothing" part is [`molt_core::relay::pool_gap`]
/// — the same classifier the founding and join refusals render — so the Tor
/// panel and a refused founding can never describe one pool two ways. This
/// adds only the rung that is specific to the probe: relays that ARE dialable
/// but are all local, which bypass Tor by design and so could never prove a
/// circuit.
pub fn target_gap(relays: &[RelayEntry], clearnet_session: bool) -> TargetGap {
    match molt_core::relay::pool_gap(relays, clearnet_session) {
        Some(PoolGap::Empty) => TargetGap::EmptyPool,
        Some(PoolGap::Unconfirmed) => TargetGap::Unconfirmed,
        Some(PoolGap::NonOnionOff) => TargetGap::SessionLocked,
        None => TargetGap::LocalOnly,
    }
}

/// The SOCKS address this dialer routes through, or `""` when it has none
/// (clearnet, or the embedded in-process client). Lets a caller show what is
/// about to be probed before the probe finishes.
pub fn proxy_of(dialer: &Dialer) -> String {
    match dialer {
        Dialer::Socks5 { proxy, .. } => proxy.clone(),
        _ => String::new(),
    }
}

/// Run the probe. `target` is the relay URL to dial through Tor (from
/// [`probe_target`]); `None` means the operator has no dialable relay, and
/// the probe then stops at the partial rung rather than picking a host.
///
/// Fail-closed: a non-Tor dialer makes this send **nothing** and report
/// [`TorTestState::Off`] — a connectivity test must never be the one place
/// that opens a clearnet connection.
pub async fn probe(dialer: &Dialer, target: Option<&str>) -> TorTest {
    if !dialer.tor_on() {
        return TorTest {
            state: TorTestState::Off,
            detail: "Tor is not enabled — nothing was sent".to_string(),
            ..TorTest::default()
        };
    }
    // Defence in depth against a caller that did not use `probe_target`: a
    // local relay bypasses Tor by design, so it cannot be evidence here.
    let target = target.filter(|url| relay_kind(url) != RelayKind::Local);

    let mut report = RungReport {
        proxy: proxy_of(dialer),
        target: target.unwrap_or_default().to_string(),
        ..RungReport::default()
    };

    // Rung: is anything listening at the SOCKS address? (Skipped by the
    // embedded in-process client, which has no proxy.)
    if !report.proxy.is_empty() {
        let answered = proxy_rung(&report.proxy).await;
        let failed = answered.is_err();
        report.proxy_answered = Some(answered);
        if failed {
            // no daemon there: dialing a relay through it would only produce
            // the same error a second time.
            report.target.clear();
            return verdict(&report);
        }
    }

    // Rung: a real dial THROUGH Tor to a relay the operator confirmed.
    if let Some(url) = target {
        report.circuit = Some(circuit_rung(dialer, url).await);
    }
    verdict(&report)
}

/// Open (and immediately drop) a TCP connection to the SOCKS address. Proves
/// a socket is listening there — deliberately nothing more, see the module
/// docs.
async fn proxy_rung(proxy: &str) -> Result<(), String> {
    match timeout(PROXY_TIMEOUT, TcpStream::connect(proxy)).await {
        Ok(Ok(stream)) => {
            drop(stream);
            Ok(())
        }
        Ok(Err(e)) => Err(format!("no socket answered at {proxy}: {e}")),
        Err(_) => Err(format!(
            "no socket answered at {proxy}: timed out after {}s",
            PROXY_TIMEOUT.as_secs()
        )),
    }
}

/// Dial `url`'s host through Tor with the REAL dialer — the same call the
/// transport makes, so a success here is a circuit the transport can use
/// too. Returns the round-trip in milliseconds.
async fn circuit_rung(dialer: &Dialer, url: &str) -> Result<u32, String> {
    let (host, _port) = relay_host_port(url)?;
    let started = std::time::Instant::now();
    // NOT a bare SOCKS CONNECT (review finding 2026-07-31): a proxy that
    // answers `0x00` while connecting to nothing would then read as "Tor
    // works" — a fake SOCKS server passes that bar, as this module's own
    // test showed. Completing the WebSocket upgrade proves the RELAY
    // answered, through Tor, on the exact path the transport uses.
    let dialed = timeout(CIRCUIT_TIMEOUT, crate::relay_ws::RelayWs::connect(dialer, url)).await;
    let ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
    match dialed {
        Ok(Ok(ws)) => {
            ws.close().await;
            Ok(ms)
        }
        Ok(Err(e)) => Err(format!("no relay handshake through Tor to {host}: {e}")),
        Err(_) => Err(format!(
            "{TIMEOUT_MARK} {host}: no answer within {}s",
            CIRCUIT_TIMEOUT.as_secs()
        )),
    }
}

/// Prefix that marks a DEADLINE rather than a refusal, so the surfaces can
/// say "no answer yet" instead of "not working" — a cold embedded Tor
/// bootstrap legitimately takes minutes (`dial.rs` deliberately puts no
/// deadline on it).
pub const TIMEOUT_MARK: &str = "timed out reaching";

/// The dial coordinates of a relay URL, read with the SAME WHATWG parser the
/// pool policy classified it with (`relay_ws::connect` does the identical
/// read) — the host dialed can never differ from the host classified.
fn relay_host_port(url: &str) -> Result<(String, u16), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("relay url {url}: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("relay url {url}: no host"))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| format!("relay url {url}: no port"))?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The proxy rung's failure must not smuggle a target into the verdict:
    /// nothing was dialed, so nothing is claimed about a relay.
    #[test]
    fn a_failed_proxy_rung_reports_no_target() {
        let v = verdict(&RungReport {
            proxy: "127.0.0.1:9050".to_string(),
            proxy_answered: Some(Err("refused".to_string())),
            target: "wss://relay.example.test".to_string(),
            circuit: None,
        });
        assert_eq!(v.state, TorTestState::NoProxy);
        assert!(v.target.is_empty(), "no relay was dialed: {v:?}");
    }

    #[test]
    fn relay_urls_resolve_to_dial_coordinates() {
        assert_eq!(
            relay_host_port("wss://relay.example.test").expect("wss default port"),
            ("relay.example.test".to_string(), 443)
        );
        assert_eq!(
            relay_host_port("ws://relay.example.test:7777").expect("explicit port"),
            ("relay.example.test".to_string(), 7777)
        );
        // an IPv6 literal loses its brackets for the dialer
        assert_eq!(
            relay_host_port("ws://[::1]:8080").expect("v6"),
            ("::1".to_string(), 8080)
        );
        assert!(relay_host_port("not a url").is_err());
    }

    #[test]
    fn only_a_socks_dialer_has_a_proxy_address() {
        assert_eq!(proxy_of(&Dialer::Direct), "");
        assert_eq!(
            proxy_of(&Dialer::resolve("tor", "local", 9050).expect("tor+local")),
            "127.0.0.1:9050"
        );
        assert_eq!(
            proxy_of(&Dialer::resolve("tor", "whonix", 9050).expect("whonix")),
            "10.152.152.10:9050"
        );
    }
}
