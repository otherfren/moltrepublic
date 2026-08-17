// SPDX-License-Identifier: GPL-3.0-or-later

//! Operator-facing sentences for the relay gate, and the short run-failure
//! headlines rendered above them.
//!
//! The headlines ([`headline_for`], [`restore_headline_for`]) live here rather
//! than in their own module because their arms are pinned, in this module's
//! tests, against the very functions below that PRODUCE the sentences they
//! classify — the two must be read and changed together. A reworded
//! `pool_gap_reason` that silently kills a headline arm is the exact defect
//! that separation invited once already.
//!
//! `molt_core::relay` decides WHETHER a relay can be dialed and WHY NOT; this
//! module is the only place that turns those verdicts into words. The split
//! is deliberate: the sentences name a GUI tab ("Settings › Nostr relays")
//! and a config key (`clearnet_enabled`), and a contract crate with no I/O
//! must not know that a GUI or a `config.toml` exists. Keeping the classifier
//! pure also leaves the door open for a localized surface to render the same
//! `InviteRelayBlock` through molt-ui's `lexicon!` instead of this text.
//!
//! The wording exists because of a 2026-08-01 report: a relay hand-written
//! into `config.toml` as `confirmed = true` without `clearnet_enabled = true`
//! is undialable, and every refusal said "no confirmed relay on this node" —
//! telling the operator to repeat the one act they had already performed. A
//! refusal that names the wrong fix is worse than a refusal that names none.

use molt_core::relay::{InviteRelayBlock, InviteRelayVerdict, PoolGap, RelayEntry};

/// The config key that lifts the non-onion block — named once, in the summary
/// line, never repeated per relay.
const CLEARNET_KEY: &str = "[transport.nostr] clearnet_enabled";

/// The pool is about to change: a relay confirmation's probe verdict has
/// not landed yet. ONE sentence for every gate that refuses on it (found,
/// join, recovery) — hand-typed copies already drifted once.
pub(crate) fn pool_verifying_reason() -> &'static str {
    "a relay confirmation is still verifying - retry in a moment"
}

/// Why this node has nothing to dial at all — for the founding/recovery
/// prerequisites, which fail before any invite is involved.
pub(crate) fn pool_gap_reason(gap: PoolGap) -> String {
    match gap {
        PoolGap::Empty => "no relay configured".to_string(),
        PoolGap::Unconfirmed => "no relay confirmed".to_string(),
        PoolGap::NonOnionOff => {
            format!("clearnet/local dialing off ({CLEARNET_KEY})")
        }
    }
}

/// The per-relay fault — two or three words, aligned into a scannable column.
/// Not an instruction: the remedy belongs in the summary line once.
fn fault_for(blocked: Option<InviteRelayBlock>) -> &'static str {
    match blocked {
        None => "dialable",
        Some(InviteRelayBlock::NotInPool) => "not in relay pool",
        Some(InviteRelayBlock::Unconfirmed) => "not confirmed",
        Some(InviteRelayBlock::ClearnetOff) => "clearnet/local dialing off",
    }
}

/// The refusal for a join whose invite names no relay this node can dial.
///
/// One line per relay, `url` then a two-word fault, aligned into a column so
/// the eye finds the odd one out without reading. The remedy is ONE summary
/// line — repeating it per relay is what made the previous version a wall of
/// text. The `→ `/`✗ ` prefixes are the run log's tone protocol (molt-ui
/// colours each line from its first character).
pub(crate) struct JoinRelayRefusal {
    /// One line per relay the invite names, in the invite's order.
    pub(crate) detail: Vec<String>,
    /// The terminal `✗ join failed: …` summary — the only line carrying a fix.
    pub(crate) headline: String,
}

/// Render the verdicts. Takes them already computed so the caller's dial set
/// and this message can never come from two different judgements of the same
/// relay.
pub(crate) fn join_relay_refusal(
    verdicts: &[InviteRelayVerdict],
    pool: &[RelayEntry],
    clearnet_session: bool,
) -> JoinRelayRefusal {
    // align the fault column: the relay that differs should be findable
    // without reading a single word
    let width = verdicts.iter().map(|v| v.url.chars().count()).max().unwrap_or(0);
    let mut detail: Vec<String> = verdicts
        .iter()
        .map(|v| format!("→ {:<width$}  {}", v.url, fault_for(v.blocked), width = width))
        .collect();
    let dialable = molt_core::relay::dialable(pool, clearnet_session);
    if !dialable.is_empty() {
        detail.push(format!("→ dialable here: {}", dialable.join(", ")));
    }
    // "no relay in common" is only true when every named relay is unknown
    // here — a relay the operator can SEE in their settings IS in common, it
    // is merely not dialable.
    let all_unknown = verdicts
        .iter()
        .all(|v| v.blocked == Some(InviteRelayBlock::NotInPool));
    let switch_blocks = verdicts
        .iter()
        .any(|v| v.blocked == Some(InviteRelayBlock::ClearnetOff));
    let headline = if all_unknown {
        // the detail lines above name every relay this republic uses, so the
        // remedy is "one of them" — stated ONCE, here, not per relay. Relays
        // do not federate: joining means dialing a relay the others dial,
        // and nothing else will do (§10.15).
        "no relay in common with this invite — add one of them".to_string()
    } else if switch_blocks {
        // the one case the operator cannot deduce from their own config
        format!("no dialable relay — clearnet/local dialing off ({CLEARNET_KEY})")
    } else {
        // the residual mix (unconfirmed and/or unknown): both fixes are
        // acts on this node's pool — name them once, like the siblings
        "no dialable relay for this invite — add or confirm one of them".to_string()
    };
    JoinRelayRefusal { detail, headline }
}

/// Why none of a REPUBLIC's own relays can be dialed here.
///
/// Derived from the per-relay verdicts, never from the whole-pool
/// [`molt_core::relay::pool_gap`]: that one answers "can this node dial
/// anything at all", which is a different question. A coordinator holding one
/// perfectly dialable relay can still share none with this republic — and
/// answering "no relay in common" while the republic's relay sits in Settings
/// behind a switch names the wrong fix, which is the 2026-08-01 report this
/// module exists to prevent.
pub(crate) fn republic_relay_reason(verdicts: &[InviteRelayVerdict]) -> String {
    if verdicts.is_empty() {
        return "no relay recorded for this republic".to_string();
    }
    if verdicts
        .iter()
        .all(|v| v.blocked == Some(InviteRelayBlock::NotInPool))
    {
        // the one the operator must fix by ADDING a relay, not by flipping a
        // switch: this node has never heard of the republic's relays
        return "no relay in common with this republic".to_string();
    }
    if verdicts
        .iter()
        .any(|v| v.blocked == Some(InviteRelayBlock::ClearnetOff))
    {
        return format!("this republic's relay needs {CLEARNET_KEY}");
    }
    if verdicts
        .iter()
        .any(|v| v.blocked == Some(InviteRelayBlock::Unconfirmed))
    {
        return "this republic's relay is not confirmed".to_string();
    }
    "no dialable relay for this republic".to_string()
}

/// The failure in a FEW WORDS — what gets rendered large and in the signal
/// colour above the run log.
///
/// The full sentence stays in the log. This is deliberately not a summary of
/// it: a headline that tries to carry the detail is just the wall of text
/// again, one line higher up. Name the missing thing, stop.
///
/// Two rules make this safe to derive from a rendered sentence:
///
/// 1. **An unrecognised failure gets NO headline** (empty), never a guessed
///    cause. The surface then shows its generic failed-title, which is
///    merely uninformative — whereas a guessed cause is WRONG, and sends the
///    operator to fix something that was never broken.
/// 2. **Every arm matches a phrase this codebase actually emits**, anchored
///    and distinctive — never a short fragment. `contains("tor")` matched
///    "res**tor**e", "s**tor**age" and "his**tor**y", so every restore
///    failure would have been reported, large and red, as a Tor problem.
///    The URL-host-parser lesson in `CLAUDE.md` is the same shape: a
///    substring test that disagrees with the real vocabulary IS the defect.
///
/// The companion test drives the arms from the producing functions
/// (`pool_gap_reason`, `join_relay_refusal`) rather than from re-typed
/// sentences, so a reworded message breaks the test instead of silently
/// killing an arm.
/// Every headline phrase the three classifiers can emit (E3): the GUI
/// localizes BY PHRASE — the English phrase is the stable key — and its
/// coverage test walks exactly this list, so a new arm without a German
/// mapping goes red instead of silently rendering English. Pinned
/// producible by `every_known_headline_is_producible`.
pub fn known_headlines() -> &'static [&'static str] {
    &[
        // network vocabulary (both legs)
        "No shared relay",
        "Clearnet dialing is off",
        "Relay check running",
        "No dialable relay",
        "Relay not answering",
        "No relay configured",
        "No relay confirmed",
        "Tor cannot reach the relay",
        "No answer in time",
        // the founding/join leg
        "No relay took it",
        "Invite already used",
        "The founder ended it",
        "The founder refused it",
        "Workspace already exists",
        // the restore leg
        "Cannot decrypt the backup",
        "Chain does not verify",
        "Backup carries no chain",
        "No seat in this roster",
        "Workspace is open",
        "Cannot read the file",
        "Backup file too big",
        "No backup in the bucket",
        "Download failed",
    ]
}

/// One run-log line SHAPE (E5): the constant parts of the line in order.
/// The first entry is the prefix (it starts with the tone glyph the GUI
/// colours by), the last the suffix (empty when a dynamic tail ends the
/// line), and every gap between two entries is a dynamic slot — a member
/// name, a count, a URL, a free-text error. A fully static line is a
/// single-entry shape.
pub type LogShape = &'static [&'static str];

/// Every run-log line the create/join/restore legs push (E5), as shapes.
/// The GUI localizes a line by matching these constants and carrying the
/// slots over; its coverage test walks exactly this list, so a shape
/// without a German rendering goes red. The producers and this list move
/// together — a new `log.push` without a shape stays English (the honest
/// fallback), which is a review finding.
pub fn known_log_shapes() -> &'static [LogShape] {
    &[
        // the restore leg (lifecycles.rs + restore_task)
        &["→ restore started · way ", " · ", ""],
        &["✓ chain verified · height ", " · ", "-of-", ""],
        &["✓ backup from unix ", " (", " day(s) old) · workspace “", "” materialized"],
        &["→ the blob's seed does not anchor this seat's identity in the verified roster — knowledge-only restore"],
        &["→ knowledge is restored, membership is NOT — the workspace opens detached; rejoin the live republic via a recovery link"],
        &["✗ restore failed: ", ""],
        &["→ fs: read ", ""],
        &["→ s3: list ", ""],
        &["→ s3: GET ", ""],
        &["↓ ", " of ", " bytes"],
        &["→ decrypting + validating the blob"],
        &["→ staged · ", " chain block(s) await verification"],
        // the create leg (lifecycles.rs, session.rs, founding.rs)
        &["→ ritual opened · ", " (founder) · ", "-of-", " · ", " invite(s) minted"],
        &["→ SIMULATION — no real network in this build (the Nostr transport lands with N4): this node auto-activates and signs for every member. Nothing was shared off-band."],
        &["→ share each link off-band, over a private channel — the ritual waits for members to activate"],
        &["✓ roster sealed by everyone · workspace created"],
        &["✗ founding failed: ", ""],
        &["✓ recovery phrase backed up"],
        &["⚠ the relay pool changed — the invites already minted still name the OLD relays. Cancel and re-mint to hand out links that carry this pool."],
        &["→ this node has ", " dialable relays; the invite and the Welcome carry the first ", " (the pool order is the priority — reorder in Settings to change which)"],
        &["✗ ", " does not reach ", " of ", " pool relays - ", ""],
        &["⚠ ", " landed on ", " of ", " relays — ", ""],
        &["✓ direct mesh established · ", " peer(s)"],
        &["→ the group is born · welcomes sent to every member"],
        &["✗ invite ", ": a second activation by ", " did not verify — ignored"],
        &["✗ invite ", ": this founding has already formed its group around the first activation — cancel and re-mint to let ", " back in"],
        &["✗ invite ", " was activated a second time (by ", ") — that link is spent; they need their own, unused link"],
        &["· invite ", " activated by ", " — checking"],
        &["✗ invite ", ": the request claims a transport key it did not sign with — refused (possible impersonation)"],
        &["✗ invite ", ": the ticket code does not match — refused (wrong or edited link, or a link from a different founding)"],
        &["✗ invite ", ": malformed transport key (", ") — refused; the ticket stays usable for a correct retry"],
        &["✗ invite ", ": the name ", " is already taken in this founding — refused (every seat must be distinguishable, and the founder's own name is reserved)"],
        &["✗ invite ", ": that transport key is already used by another seat — refused (two seats may never share one)"],
        &["✗ invite ", ": no usable reply address in the request — refused"],
        &["✗ invite ", ": the encryption key package does not match the identity in the request — refused"],
        &["· invite ", " re-activated by ", " — the earlier attempt is replaced"],
        &["→ ", " activated invite ", " · key received"],
        &["→ every member has joined · propose the charter to seal"],
        &["→ charter proposed · awaiting every member's ratification"],
        &["✗ a decline for invite ", " came from ", ", who does not hold that seat — ignored"],
        &["✗ ", " declined the charter · cancel and re-mint to change it"],
        &["✗ the ritual is over — this republic must be founded anew (close and re-mint)"],
        &["→ charter proposed · sealing the roster for ratification"],
        &["✓ ", " signed the roster · seat sealed"],
        &["✓ ", " secured their key"],
        // the group-channel notes both legs share (nostr_ritual.rs)
        &["✓ the group channel is back"],
        &["⚠ cannot hear the group channel — ", " · still retrying"],
        &["⧗ waiting for the genesis · ", ""],
        // the join leg (lifecycles.rs + relay_msg.rs refusal detail)
        &["✓ recovery phrase backed up · waiting for the others"],
        &["✓ sealed - back up your recovery phrase to enter"],
        &["✗ join failed: ", ""],
        &["✓ the founder accepted your join · waiting for the deliberation"],
        &["→ charter proposed: “", "” · review and confirm to join"],
        &["✓ you ratified the charter · sealing your signature"],
        &["→ save your recovery phrase - re-type it to confirm"],
        &["✗ you declined the charter"],
        &["✗ the ritual is over — this republic must be founded anew"],
        &["→ dialable here: ", ""],
        &["→ ", "  dialable"],
        &["→ ", "  not in relay pool"],
        &["→ ", "  not confirmed"],
        &["→ ", "  clearnet/local dialing off"],
    ]
}

pub(crate) fn headline_for(error: &str) -> String {
    let e = error.to_ascii_lowercase();
    // the leg's OWN anchored phrases decide first; the network vocabulary is
    // the fallback. That order is load-bearing, not cosmetic — see
    // [`network_headline`].
    ritual_headline(&e)
        .or_else(|| network_headline(&e))
        .unwrap_or("")
        .to_string()
}

/// The same, for the RESTORE leg (`fail_restore` / `restore_task`).
///
/// A separate vocabulary, not a few more arms on [`headline_for`]: the two
/// legs share sentences that do NOT mean the same thing. `"crypto: …"` is a
/// wrong passphrase when a backup is being decrypted and a storage fault when
/// a founding is being written — one classifier serving both would announce
/// "Cannot decrypt the backup" over a failed founding. Scoping the arms to the
/// leg that emits them makes that class of mistake unrepresentable instead of
/// merely untested.
pub(crate) fn restore_headline_for(error: &str) -> String {
    let e = error.to_ascii_lowercase();
    restore_only_headline(&e)
        .or_else(|| network_headline(&e))
        .unwrap_or("")
        .to_string()
}

/// Faults that mean the same thing on every leg: the dialer and the relay
/// gate. Ordered most-specific first — several contain "relay", and the
/// clearnet switch is the one cause an operator cannot deduce from their own
/// settings, so it outranks the generic "no dialable relay" it travels inside.
///
/// **This runs LAST, after the leg's own arms.** Some of the sentences fed to
/// the classifier carry text this node did not write: the founder's `reason`
/// on `LinkSpent`/`Aborted` (`nostr_ritual.rs:674-693`) comes off the wire,
/// and a restore's file path and bucket name come from the operator's own
/// input. A remote party that could reach the network arms would choose what
/// this node shouts in 26px — "Tor cannot reach the relay" over a founding
/// the founder simply cancelled. Letting the leg's anchored phrases decide
/// first means our own words, which always precede the borrowed text, win.
fn network_headline(e: &str) -> Option<&'static str> {
    Some(if e.contains("no relay in common") {
        "No shared relay"
    } else if e.contains("clearnet/local dialing off") {
        "Clearnet dialing is off"
    } else if e.contains("confirmation is still verifying") {
        "Relay check running"
    } else if e.contains("no dialable relay") {
        "No dialable relay"
    } else if e.contains("not readable on any relay") {
        "Relay not answering"
    } else if e.contains("no relay configured") {
        "No relay configured"
    } else if e.contains("no relay confirmed") {
        "No relay confirmed"
    // the anchored Tor phrases — `molt_net::dial` emits these verbatim. A
    // bare `contains("tor")` matches "res-tor-e", "s-tor-age", "his-tor-y".
    } else if e.contains("tor circuit") || e.contains("onion-only") || e.contains("tor is off") {
        "Tor cannot reach the relay"
    } else if e.contains("timed out") || e.contains("timeout") {
        "No answer in time"
    } else {
        return None;
    })
}

/// The founding/join leg: the ritual's own faults.
fn ritual_headline(e: &str) -> Option<&'static str> {
    Some(if e.contains("did not publish") || e.contains("relay refused") {
        "No relay took it"
    } else if e.contains("already used") {
        "Invite already used"
    } else if e.contains("ended this founding") {
        "The founder ended it"
    } else if e.contains("the founder refused this activation") {
        "The founder refused it"
    } else if e.contains("already exists") {
        // the founding's own materialization step, not a backup
        "Workspace already exists"
    } else {
        return None;
    })
}

/// The restore leg: reading, decrypting and verifying a backup blob.
fn restore_only_headline(e: &str) -> Option<&'static str> {
    Some(if e.contains("crypto:") {
        // covers a wrong passphrase AND a tampered frame — the headline must
        // not pick one of them and assert it
        "Cannot decrypt the backup"
    } else if e.contains("chain verification failed") {
        "Chain does not verify"
    } else if e.contains("no verifiable chain") {
        "Backup carries no chain"
    } else if e.contains("holds no seat") {
        "No seat in this roster"
    } else if e.contains("is currently open") {
        "Workspace is open"
    } else if e.contains("already exists") {
        "Workspace already exists"
    } else if e.starts_with("reading ") {
        "Cannot read the file"
    } else if e.contains("beyond the") && e.contains("cap") {
        "Backup file too big"
    } else if e.contains("no backup for workspace") {
        "No backup in the bucket"
    } else if e.contains("download failed") {
        "Download failed"
    } else {
        return None;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::relay::diagnose_invite_relays;

    /// E5: the shape list is well-formed — every shape opens with a tone
    /// glyph (the GUI colours by it), middles are non-empty (an empty
    /// middle would merge two slots), and no shape repeats.
    #[test]
    fn every_log_shape_is_well_formed() {
        let shapes = known_log_shapes();
        let glyphs = ['→', '✓', '✗', '⚠', '·', '⧗', '↓'];
        let mut seen = std::collections::BTreeSet::new();
        for s in shapes {
            assert!(!s.is_empty(), "empty shape");
            let first = s[0].chars().next().expect("prefix is non-empty");
            assert!(glyphs.contains(&first), "shape without a tone glyph: {s:?}");
            for mid in &s[1..s.len().saturating_sub(1)] {
                assert!(!mid.is_empty(), "empty middle in {s:?}");
            }
            assert!(seen.insert(s.join("\u{0}")), "duplicate shape {s:?}");
        }
        assert!(shapes.len() >= 60, "the inventory shrank to {}", shapes.len());
    }

    fn entry(url: &str, confirmed: bool) -> RelayEntry {
        RelayEntry { url: url.to_string(), confirmed }
    }

    /// E3: every phrase in [`known_headlines`] is really producible by
    /// one of the three classifiers — the GUI's German coverage walks
    /// that list, so a phrase that fell out of the classifiers (or an arm
    /// missing from the list) must fail HERE, not render silently.
    #[test]
    fn every_known_headline_is_producible() {
        let inputs: &[&str] = &[
            "no relay in common with this invite",
            "clearnet/local dialing off (x)",
            "the confirmation is still verifying",
            "no dialable relay for this invite",
            "inbox not readable on any relay",
            "no relay configured",
            "no relay confirmed",
            "tor circuit failed",
            "the request timed out",
            "the seal did not publish",
            "this invite was already used",
            "the founder ended this founding",
            "the founder refused this activation",
            "a workspace of this name already exists",
        ];
        let restore_inputs: &[&str] = &[
            "crypto: frame does not authenticate",
            "chain verification failed",
            "the blob carries no verifiable chain",
            "this seed holds no seat in the roster",
            "the workspace is currently open",
            "reading /tmp/x: permission denied",
            "file is 9 bytes — beyond the 5-byte cap",
            "no backup for workspace x",
            "download failed: 404",
        ];
        let mut produced: Vec<String> = inputs
            .iter()
            .map(|e| headline_for(e))
            .chain(restore_inputs.iter().map(|e| restore_headline_for(e)))
            .filter(|h| !h.is_empty())
            .collect();
        produced.sort_unstable();
        produced.dedup();
        let mut known: Vec<String> =
            known_headlines().iter().map(|s| (*s).to_string()).collect();
        known.sort_unstable();
        assert_eq!(produced, known, "the list and the classifiers move together");
    }

    /// R2 residual: the MIXED-fault headline (unconfirmed + unknown, no
    /// clearnet switch involved) must name an ACTION like its two sibling
    /// branches — "no dialable relay for this invite" told the operator
    /// what is wrong and nothing about what helps.
    #[test]
    fn a_mixed_fault_headline_names_an_action() {
        let pool = vec![entry("wss://unconfirmed.example", false)];
        let offered = vec![
            "wss://never-heard-of.example".to_string(),
            "wss://unconfirmed.example".to_string(),
        ];
        let verdicts = molt_core::relay::diagnose_invite_relays(&offered, &pool, true);
        let refusal = join_relay_refusal(&verdicts, &pool, true);
        assert!(
            refusal.headline.contains("add") || refusal.headline.contains("confirm"),
            "the headline names the act, not only the fault: {}",
            refusal.headline
        );
    }

    /// Each relay gets its own two-word fault, the remedy appears once, and
    /// the summary may only claim "no relay in common" when every named relay
    /// really is unknown here.
    #[test]
    fn each_relay_gets_a_short_fault_and_the_fix_is_stated_once() {
        let pool = vec![entry("wss://unconfirmed.example", false), entry("wss://dark.example", true)];
        let offered = vec![
            "wss://never-heard-of.example".to_string(),
            "wss://unconfirmed.example".to_string(),
            "wss://dark.example".to_string(),
        ];
        let v = diagnose_invite_relays(&offered, &pool, false);
        let r = join_relay_refusal(&v, &pool, false);
        assert_eq!(r.detail.len(), 3, "one line per relay");
        assert!(r.detail[0].ends_with("not in relay pool"), "{}", r.detail[0]);
        assert!(r.detail[1].ends_with("not confirmed"), "{}", r.detail[1]);
        assert!(r.detail[2].ends_with("clearnet/local dialing off"), "{}", r.detail[2]);
        // compact: a detail line is the url plus a short fault, nothing more
        for l in &r.detail {
            assert!(l.len() < 70, "detail line is not prose: {l}");
        }
        // the remedy is stated ONCE, in the summary
        assert_eq!(
            r.detail.iter().filter(|l| l.contains("clearnet_enabled")).count(),
            0,
            "the config key never repeats per relay: {:?}",
            r.detail
        );
        assert!(r.headline.contains("clearnet_enabled"), "{}", r.headline);
        assert!(!r.headline.contains("no relay in common"), "two ARE in common: {}", r.headline);

        // a genuinely disjoint pair says exactly that, and names what IS dialable
        let mine = vec![entry("wss://mine.example", true)];
        let v = diagnose_invite_relays(&["wss://never-heard-of.example".to_string()], &mine, true);
        let r = join_relay_refusal(&v, &mine, true);
        // …and the summary carries the remedy ONCE: the detail lines already
        // name every relay the republic uses, so "one of them" is actionable
        assert_eq!(
            r.headline,
            "no relay in common with this invite — add one of them"
        );
        assert!(r.detail.iter().any(|l| l.contains("dialable here: wss://mine.example")), "{:?}", r.detail);
    }

    /// Every headline is a few words — never a sentence, never the detail.
    /// A headline that carries the explanation is the wall of text again,
    /// one line higher up.
    ///
    /// The inputs come from the functions that PRODUCE them wherever one
    /// exists. A case list of re-typed sentences pins nothing: the first
    /// version of this matched `"no relay is configured"` while
    /// `pool_gap_reason` emits `"no relay configured"`, so the arm was dead
    /// against every real founding — and the test passed, because it fed the
    /// arm the invented string it wanted.
    #[test]
    fn a_failure_headline_is_a_few_words_not_a_sentence() {
        // a pool that shares nothing with the invite — the producer's own
        // "no relay in common", not a re-typed copy of it
        let mine = vec![entry("wss://mine.example", true)];
        let disjoint = diagnose_invite_relays(&["wss://theirs.example".to_string()], &mine, true);
        let refusal = join_relay_refusal(&disjoint, &mine, true);
        let cases = [
            // …produced by this module, read back from the producer
            refusal.headline.clone(),
            format!("cannot found: {}", pool_gap_reason(PoolGap::Empty)),
            format!("cannot found: {}", pool_gap_reason(PoolGap::Unconfirmed)),
            format!("cannot found: {}", pool_gap_reason(PoolGap::NonOnionOff)),
            format!("cannot found: {}", pool_verifying_reason()),
            // …emitted verbatim by nostr_ritual.rs / dial.rs / lifecycles.rs
            "the founding inbox is not readable on any relay — no relay replayed \
             the subscription (auth required, rate limited, or refused). No invite \
             was published."
                .to_string(),
            "seal did not publish: relay refused: blocked: nope".to_string(),
            "this invite link was already used by someone else".to_string(),
            "the founder ended this founding".to_string(),
            "tor circuit to relay.example via 127.0.0.1:9050 timed out".to_string(),
        ];
        for c in &cases {
            let h = headline_for(c);
            assert!(!h.is_empty(), "a known failure gets a headline: {c}");
            assert!(
                h.split_whitespace().count() <= 5,
                "at most five words, got {h:?} for {c}"
            );
            assert!(!h.ends_with('.'), "a headline is not a sentence: {h}");
            assert!(h.chars().count() <= 32, "short enough to render large: {h:?}");
        }
        // the specific cases are distinguished, not collapsed into one default
        assert_eq!(headline_for(&refusal.headline), "No shared relay");
        assert_ne!(
            headline_for(&format!("cannot found: {}", pool_gap_reason(PoolGap::Empty))),
            headline_for(&format!("cannot found: {}", pool_gap_reason(PoolGap::Unconfirmed))),
            "two different missing things get two different headlines"
        );
        // the switch outranks the generic refusal it travels inside: it is the
        // one cause the operator cannot deduce from their own settings
        let dark = vec![entry("wss://dark.example", true)];
        let blocked = diagnose_invite_relays(&["wss://dark.example".to_string()], &dark, false);
        let switched = join_relay_refusal(&blocked, &dark, false);
        assert_eq!(headline_for(&switched.headline), "Clearnet dialing is off");
    }

    /// An unrecognised failure gets NO headline — never a guessed cause.
    ///
    /// The first version answered "Connection failed" for everything it did
    /// not match, so a local storage fault, a bad key package or an MLS error
    /// would have been announced, large and red, as a network problem — and
    /// the operator would go check their relays. An empty headline is merely
    /// uninformative; a wrong one costs an evening.
    #[test]
    fn an_unknown_failure_gets_no_headline_rather_than_a_guessed_cause() {
        for unknown in [
            "something nobody anticipated",
            "mls identity: key store rejected the credential",
            "key package: unsupported ciphersuite",
            "staging task failed: panic in blocking pool",
        ] {
            assert_eq!(
                headline_for(unknown),
                "",
                "no cause may be invented for: {unknown}"
            );
        }
    }

    /// "res**tor**e", "s**tor**age" and "his**tor**y" all contain "tor".
    ///
    /// The arm that matched the bare fragment would have reported EVERY
    /// restore failure as a Tor problem, in the largest type on the surface.
    /// These are the real sentences `fail_restore` and `restore_task` emit.
    #[test]
    fn a_restore_failure_is_never_reported_as_a_tor_problem() {
        let restore_failures = [
            "the restore task lost its staged blob",
            "the backup carries no verifiable chain — refusing to materialize \
             unverified history",
            "workspace ws-1 is currently open — close it before restoring over it \
             (a replace cannot move a live directory)",
            "this node has no workspace storage to restore into",
            "crypto: aead open failed",
            "reading /home/u/restore.molt.enc: no such file or directory",
        ];
        for f in restore_failures {
            let h = restore_headline_for(f);
            assert!(
                !h.contains("Tor"),
                "a restore failure is not a Tor failure: {f} → {h:?}"
            );
            assert!(h.chars().count() <= 32, "renderable large: {h:?}");
        }
        // …and the ones an operator can act on DO get named
        assert_eq!(
            restore_headline_for("crypto: aead open failed"),
            "Cannot decrypt the backup"
        );
        assert_eq!(
            restore_headline_for("workspace ws-1 is currently open — close it first"),
            "Workspace is open"
        );
    }

    /// The far end does not get to choose what this node shouts.
    ///
    /// `LinkSpent`/`Aborted` carry a `reason` the FOUNDER writes
    /// (`nostr_ritual.rs:674-693`), and it is embedded in the sentence the
    /// classifier then reads. A founder who writes "tor circuit timed out"
    /// must not make the joiner's screen blame its own Tor setup for a
    /// founding that was simply cancelled. Our anchored phrase always
    /// precedes the borrowed text, so ordering the leg's own arms first is
    /// what enforces this.
    #[test]
    fn the_far_end_cannot_choose_this_node_s_headline() {
        // exactly the strings nostr_ritual.rs builds, with a hostile reason
        let hostile = "tor circuit to relay.example timed out";
        assert_eq!(
            headline_for(&format!("the founder ended this founding: {hostile}")),
            "The founder ended it"
        );
        assert_eq!(
            headline_for(&format!("the founder refused this activation: {hostile}")),
            "The founder refused it"
        );
        // and the operator's own file path cannot do it either
        assert_eq!(
            restore_headline_for("reading /home/u/tor circuit/backup.molt.enc: no such file"),
            "Cannot read the file"
        );
    }

    /// The two legs keep separate vocabularies, because they share sentences
    /// that do not share meanings.
    ///
    /// `"crypto: …"` is a wrong passphrase while a backup is decrypted and a
    /// storage fault while a founding is written. A single classifier over
    /// both would announce "Cannot decrypt the backup" across a failed
    /// founding — a confident, wrong cause in the largest type on the screen.
    #[test]
    fn the_restore_vocabulary_never_leaks_into_the_founding_leg() {
        // the restore-only phrases stay silent on the ritual leg…
        for restore_only in [
            "crypto: aead open failed",
            "chain verification failed: height 3 is below threshold",
            "the backup carries no verifiable chain",
            "download failed: 503",
        ] {
            assert_eq!(
                headline_for(restore_only),
                "",
                "a founding must not borrow a restore headline: {restore_only}"
            );
        }
        // …while the network faults, which DO mean the same thing on both
        // legs, are answered identically by each
        for shared in [
            "tor circuit to relay.example via 127.0.0.1:9050 timed out",
            "cannot found: clearnet/local dialing off ([transport.nostr] clearnet_enabled)",
        ] {
            assert_eq!(headline_for(shared), restore_headline_for(shared));
            assert!(!headline_for(shared).is_empty(), "{shared}");
        }
    }

    /// The founding prerequisite names the fault in three words — and the
    /// switch, not a confirmation that already exists.
    #[test]
    fn the_pool_gap_reason_is_short_and_names_the_switch() {
        assert_eq!(pool_gap_reason(PoolGap::Empty), "no relay configured");
        assert_eq!(pool_gap_reason(PoolGap::Unconfirmed), "no relay confirmed");
        let r = pool_gap_reason(PoolGap::NonOnionOff);
        assert!(r.contains("clearnet_enabled") && r.len() < 70, "{r}");
    }
}
