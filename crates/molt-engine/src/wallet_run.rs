// SPDX-License-Identifier: GPL-3.0-or-later

//! The purse run (`docs/chain/wallet_treasury_design.md` §3.3-§3.5, plan
//! §7.4-§7.6): start, readiness and consent, the two PedPoP rounds over
//! control frames, persist-then-attest, the terminal `wallet_created`
//! card, its projection and the restart. Ephemeral except the keys
//! records; the culprit of an abort is only ever logged here.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use molt_core::wallet::{RunStage, WalletRefusal, WalletRunView};
use molt_core::{Event, MemberId, MoltError, ProposalState, Reply, Surface};
use molt_net::wallet_frames::{
    WalletAbortFrame, WalletAttestFrame, WalletFrame, WalletHintFrame, WalletReadyFrame,
    WalletRound1Frame, WalletRound2Frame, WalletStartFrame, WALLET_V,
};
use molt_treasury::attest::{self, Attested};
use molt_treasury::dkg::{self, Frames, Inbox};
use molt_treasury::keys::{self, KeysRecord};
use molt_treasury::rand_core::{RngCore, SeedableRng};
use molt_treasury::{Ed25519, KeyMachine, Network, RunId, SecretShareMachine};
use serde_json::{json, Value};

use crate::wallet::{parse_init, WALLET_CREATED};
use crate::State;

/// Readiness, and each round, must finish within this.
pub(crate) const RUN_DEADLINE_SECS: u64 = 180;
/// The fallback starter / proposer at position `k` waits `(k-1)` steps.
pub(crate) const STEP_SECS: u64 = 60;
/// Own frames go out again this often until commit or abort.
const RESEND_SECS: u64 = 10;
/// Frames of a run this seat has not joined yet, kept for when it does.
const EARLY_MAX: usize = 256;
/// Attestation sets of runs this seat holds no record of.
const FOREIGN_RUNS_MAX: usize = 8;
/// The abort reasons a peer may name; anything else reads `aborted`.
const REASONS: [&str; 9] =
    ["declined", "not ready", "timeout", "restart", "invalid", "equivocation", "transcript", "storage", "aborted"];

/// The run's test seams (never a `Command`).
#[derive(Debug, Default)]
pub(crate) struct WalletSeams {
    /// Overrides [`RUN_DEADLINE_SECS`]; the step is a quarter of it.
    pub(crate) deadline: AtomicU64,
    /// This seat persists but its attestation leaves it neither as a
    /// frame nor in a purse card of its own.
    pub(crate) withhold: AtomicBool,
    /// A start to send on the next tick, with this nonce.
    pub(crate) start: std::sync::Mutex<Option<[u8; 32]>>,
}

/// One run this seat takes part in.
pub(crate) struct Run {
    pub(crate) init: u64,
    pub(crate) nonce: [u8; 32],
    pub(crate) starter: u16,
    pub(crate) stage: RunStage,
    /// When the current stage began (deadline clock).
    since: u64,
    ready: BTreeSet<u16>,
    reason: Option<String>,
    missing: Vec<u16>,
    machine: Option<SecretShareMachine<Ed25519>>,
    round1: Inbox,
    key_machine: Option<KeyMachine<Ed25519>>,
    shares: Inbox,
    transcripts: BTreeMap<u16, [u8; 32]>,
    transcript: Option<[u8; 32]>,
    /// Own frames, resent on the beat.
    out: Vec<WalletFrame>,
    persisted: bool,
}

impl Run {
    /// A finished run keeps no round material (the inboxes wipe on drop).
    fn wipe(&mut self) {
        self.machine = None;
        self.key_machine = None;
        self.round1 = Inbox::new(0);
        self.shares = Inbox::new(0);
        self.out.clear();
    }
}

impl std::fmt::Debug for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Run")
            .field("init", &self.init)
            .field("stage", &self.stage)
            .field("ready", &self.ready)
            .finish_non_exhaustive()
    }
}

/// The run side of the purse, in memory only (the keys records excepted).
#[derive(Default)]
pub(crate) struct RunRt {
    pub(crate) run: Option<Run>,
    /// Nonces of runs joined, refused or aborted: never joined again.
    seen: BTreeSet<[u8; 32]>,
    /// Runs left in readiness for another start, with their starter.
    left: BTreeMap<[u8; 32], u16>,
    /// Aborted runs and when their abort last went out.
    aborted: BTreeMap<[u8; 32], u64>,
    early: Vec<(u16, WalletFrame)>,
    /// This seat's keys records, decoded (one per attested run).
    pub(crate) records: Vec<KeysRecord>,
    attests: BTreeMap<[u8; 32], BTreeMap<u16, [u8; 64]>>,
    complete_at: BTreeMap<[u8; 32], u64>,
    proposed: BTreeSet<[u8; 32]>,
    /// Inits auto-started this session (§3.3: once per init).
    started: BTreeSet<u64>,
    /// A fallback start armed for `(init, at)`.
    auto_from: Option<(u64, u64)>,
    /// Inits this seat consented to in a run.
    consented: BTreeSet<u64>,
    hint: String,
    resent_at: u64,
    /// The purse proposal id already settled here.
    settled: Option<Option<u64>>,
    /// The projected purse's record is not held here.
    pub(crate) watch_only: bool,
    last_view: Option<WalletRunView>,
}

impl std::fmt::Debug for RunRt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunRt")
            .field("run", &self.run)
            .field("records", &self.records.len())
            .finish_non_exhaustive()
    }
}

/// A parsed `wallet_created` (design §3.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Created {
    pub(crate) init: u64,
    pub(crate) run: [u8; 32],
    pub(crate) transcript: [u8; 32],
    pub(crate) address: String,
    pub(crate) network: Network,
    pub(crate) threshold: u16,
    pub(crate) participants: u16,
    pub(crate) birthday: u64,
    pub(crate) sigs: Vec<[u8; 64]>,
}

/// The projected purse: the applied `wallet_created` and its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Purse {
    pub(crate) id: Option<u64>,
    pub(crate) created: Created,
}

pub(crate) fn network_word(n: Network) -> &'static str {
    match n {
        Network::Mainnet => "mainnet",
        Network::Stagenet => "stagenet",
        Network::Testnet => "testnet",
    }
}

pub(crate) fn network_of(word: &str) -> Option<Network> {
    match word {
        "mainnet" => Some(Network::Mainnet),
        "stagenet" => Some(Network::Stagenet),
        "testnet" => Some(Network::Testnet),
        _ => None,
    }
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    hex::decode(s).ok()?.try_into().ok()
}

fn hex64(s: &str) -> Option<[u8; 64]> {
    hex::decode(s).ok()?.try_into().ok()
}

fn short(nonce: &[u8; 32]) -> String {
    hex::encode(&nonce[..4])
}

/// The one encoding of a `wallet_created` payload.
pub(crate) fn created_value(c: &Created) -> Value {
    json!({
        "op": WALLET_CREATED,
        "init": c.init,
        "run": hex::encode(c.run),
        "transcript": hex::encode(c.transcript),
        "address": c.address,
        "network": network_word(c.network),
        "threshold": c.threshold,
        "participants": c.participants,
        "birthday_height": c.birthday,
        "attestations": c.sigs.iter().map(hex::encode).collect::<Vec<_>>(),
    })
}

/// A `wallet_created` in exactly its one encoding.
pub(crate) fn parse_created(v: &Value) -> Option<Created> {
    let u16_of = |k: &str| v.get(k)?.as_u64().and_then(|x| u16::try_from(x).ok());
    let c = Created {
        init: v.get("init")?.as_u64()?,
        run: hex32(v.get("run")?.as_str()?)?,
        transcript: hex32(v.get("transcript")?.as_str()?)?,
        address: v.get("address")?.as_str()?.to_string(),
        network: network_of(v.get("network")?.as_str()?)?,
        threshold: u16_of("threshold")?,
        participants: u16_of("participants")?,
        birthday: v.get("birthday_height")?.as_u64()?,
        sigs: v
            .get("attestations")?
            .as_array()?
            .iter()
            .map(|s| s.as_str().and_then(hex64))
            .collect::<Option<Vec<_>>>()?,
    };
    (created_value(&c) == *v).then_some(c)
}

fn record_created(rec: &KeysRecord, sigs: Vec<[u8; 64]>) -> Created {
    Created {
        init: rec.run.init_id,
        run: rec.run.run,
        transcript: rec.transcript,
        address: rec.address.clone(),
        network: rec.network,
        threshold: rec.m,
        participants: rec.n,
        birthday: rec.birthday,
        sigs,
    }
}

fn attested_of(c: &Created, republic_id: [u8; 32]) -> Attested<'_> {
    Attested {
        run: RunId { republic_id, init_id: c.init, run: c.run },
        transcript: c.transcript,
        address: &c.address,
        network: c.network,
        m: c.threshold,
        n: c.participants,
        birthday: c.birthday,
    }
}

fn rng() -> rand_chacha::ChaCha20Rng {
    let mut seed = [0u8; 32];
    if getrandom::getrandom(&mut seed).is_err() {
        tracing::error!("wallet_rng=unavailable");
    }
    rand_chacha::ChaCha20Rng::from_seed(seed)
}

fn fresh_nonce() -> [u8; 32] {
    let mut n = [0u8; 32];
    rng().fill_bytes(&mut n);
    n
}

impl State {
    /// The republic id raw.
    fn wallet_republic_raw(&self) -> Option<[u8; 32]> {
        hex32(&self.republic_id())
    }

    /// The seats in founding order (W7).
    fn wallet_seats(&self) -> Vec<MemberId> {
        self.vault_founding_table().into_iter().map(|i| i.member).collect()
    }

    fn wallet_pos(&self, member: &str) -> Option<u16> {
        let k = self.wallet_seats().iter().position(|m| m == member)?;
        u16::try_from(k + 1).ok()
    }

    fn wallet_names(&self, positions: impl IntoIterator<Item = u16>) -> Vec<MemberId> {
        let seats = self.wallet_seats();
        positions
            .into_iter()
            .filter_map(|p| seats.get(usize::from(p).checked_sub(1)?).cloned())
            .collect()
    }

    fn wallet_deadline(&self) -> u64 {
        match self.wallet_seams.deadline.load(Ordering::SeqCst) {
            0 => RUN_DEADLINE_SECS,
            d => d,
        }
    }

    fn wallet_step(&self) -> u64 {
        match self.wallet_seams.deadline.load(Ordering::SeqCst) {
            0 => STEP_SECS,
            d => (d / 4).max(1),
        }
    }

    /// `(m, n, init id, birthday, network)` while a run can happen.
    fn wallet_run_ctx(&self) -> Option<(u16, u16, u64, u64, Network)> {
        let (m, n) = self.wallet_rule()?;
        if !molt_core::wallet::bounds_ok(m, n) || self.wallet_purse().is_some() {
            return None;
        }
        let init = self.wallet_init_applied()?;
        Some((u16::from(m), u16::from(n), init.id?, init.birthday, network_of(&init.network)?))
    }

    /// The projected purse (design §3.5): the first applied
    /// `wallet_created` that references the applied init, carries its
    /// birthday and network, the founding rule, and n attestations under
    /// the founding identities. Payload and anchor only.
    pub(crate) fn wallet_purse(&self) -> Option<Purse> {
        let init = self.wallet_init_applied()?;
        let init_id = init.id?;
        let (m, n) = self.wallet_rule()?;
        if !molt_core::wallet::bounds_ok(m, n) {
            return None;
        }
        let table = self.vault_founding_table();
        let pks: Vec<&str> = table.iter().map(|i| i.identity_pk.as_str()).collect();
        let rid = self.wallet_republic_raw()?;
        self.chain.applied.get(&Surface::Wallet)?.iter().find_map(|(id, v)| {
            let c = parse_created(v)?;
            let ok = c.init == init_id
                && c.birthday == init.birthday
                && network_word(c.network) == init.network
                && c.threshold == u16::from(m)
                && c.participants == u16::from(n)
                && pks.len() == usize::from(n)
                && attest::verify_all(&attested_of(&c, rid), &pks, &c.sigs).is_ok();
            ok.then_some(Purse { id: *id, created: c })
        })
    }

    /// The approve arm of `wallet_created` (I2): an exact match with this
    /// seat's own record and n valid attestations.
    pub(crate) fn wallet_created_check(&self, payload: &Value) -> Result<(), WalletRefusal> {
        let c = parse_created(payload).ok_or(WalletRefusal::UnknownOp)?;
        if self.wallet_purse().is_some() {
            return Err(WalletRefusal::InitExists);
        }
        let init = self.wallet_init_applied().ok_or(WalletRefusal::NotMine)?;
        let rec = self
            .purse
            .run
            .records
            .iter()
            .find(|r| r.run.init_id == c.init && r.run.run == c.run)
            .ok_or(WalletRefusal::NotMine)?;
        let (m, n) = self.wallet_rule().ok_or(WalletRefusal::NotChain)?;
        let mine = record_created(rec, c.sigs.clone());
        let pks: Vec<String> = self.vault_founding_table().into_iter().map(|i| i.identity_pk).collect();
        let rid = self.wallet_republic_raw().ok_or(WalletRefusal::NotChain)?;
        let ok = mine == c
            && init.id == Some(c.init)
            && c.birthday == init.birthday
            && network_word(c.network) == init.network
            && c.threshold == u16::from(m)
            && c.participants == u16::from(n)
            && attest::verify_all(&attested_of(&c, rid), &pks, &c.sigs).is_ok();
        if ok {
            Ok(())
        } else {
            Err(WalletRefusal::NotMine)
        }
    }

    /// The INTERNAL frame feed: the MLS sender, the frame with its tag.
    pub(crate) fn cmd_net_wallet_frame(&mut self, from: &MemberId, body: &[u8]) -> Result<Reply, MoltError> {
        let Ok(frame) = WalletFrame::from_frame(body) else {
            return Ok(Reply::Ack);
        };
        if let Some(pos) = self.wallet_pos(from) {
            if *from != self.member() {
                self.wallet_ingest(pos, frame);
                self.wallet_progress();
            }
        }
        Ok(Reply::Ack)
    }

    fn wallet_buffer(&mut self, pos: u16, frame: WalletFrame) {
        let early = &mut self.purse.run.early;
        if !early.iter().any(|(p, f)| *p == pos && *f == frame) {
            if early.len() >= EARLY_MAX {
                early.remove(0);
            }
            early.push((pos, frame));
        }
    }

    fn wallet_ingest(&mut self, pos: u16, frame: WalletFrame) {
        let Some((.., init_id, _, _)) = self.wallet_run_ctx() else {
            // an init this seat has not applied yet: kept for the readiness deadline
            if self.wallet_purse().is_none() {
                self.wallet_buffer(pos, frame);
            }
            return;
        };
        if frame.init() != init_id {
            self.wallet_buffer(pos, frame);
            return;
        }
        if let WalletFrame::Hint(h) = &frame {
            self.purse.run.hint.clone_from(&h.url);
            return;
        }
        let Some(nonce) = frame.run().and_then(hex32) else {
            return;
        };
        // an aborted run's frames are answered like a stranger's: with the abort
        let current = self.purse.run.run.as_ref().filter(|r| r.stage != RunStage::Aborted).map(|r| r.nonce);
        match frame {
            WalletFrame::Start(s) => self.wallet_on_start(pos, nonce, s.starter),
            WalletFrame::Attest(a) => {
                if let Some(sig) = hex64(&a.sig) {
                    self.wallet_on_attest(pos, nonce, sig);
                }
            }
            f if Some(nonce) == current => self.wallet_on_run_frame(pos, f),
            f => self.wallet_on_foreign(pos, nonce, f),
        }
    }

    /// A frame of a run this seat is not in.
    fn wallet_on_foreign(&mut self, pos: u16, nonce: [u8; 32], frame: WalletFrame) {
        let now = self.presence_now();
        if let Some(at) = self.purse.run.aborted.get(&nonce).copied() {
            if !matches!(frame, WalletFrame::Abort(_)) && now.saturating_sub(at) >= RESEND_SECS {
                self.wallet_send_abort(nonce, "aborted");
            }
            return;
        }
        if self.purse.run.seen.contains(&nonce) {
            return;
        }
        let round = matches!(frame, WalletFrame::Round1(_) | WalletFrame::Round2(_));
        // a round frame proves every seat readied that run: it beats one still in readiness here
        let idle = self
            .purse
            .run
            .run
            .as_ref()
            .map_or(true, |r| !r.persisted && matches!(r.stage, RunStage::Ready | RunStage::Aborted));
        if let Some(starter) = self.purse.run.left.get(&nonce).copied().filter(|_| round && idle) {
            self.wallet_join(nonce, starter);
            return self.wallet_ingest(pos, frame);
        }
        // a round frame means every seat was ready, this one too: its state is lost (§3.3)
        let held = self.purse.run.records.iter().any(|r| r.run.run == nonce);
        if matches!(frame, WalletFrame::Round1(_) | WalletFrame::Round2(_)) && !held {
            tracing::warn!(run = %short(&nonce), from = pos, reason = "lost", "wallet_abort");
            self.purse.run.seen.insert(nonce);
            self.wallet_send_abort(nonce, "restart");
            return;
        }
        self.wallet_buffer(pos, frame);
    }

    fn wallet_on_start(&mut self, pos: u16, nonce: [u8; 32], starter: u16) {
        if starter != pos {
            return;
        }
        let cur = self.purse.run.run.as_ref().map(|r| (r.nonce, r.starter, r.stage, r.persisted));
        if let Some((n, ..)) = cur {
            if n == nonce {
                return;
            }
        }
        if self.purse.run.seen.contains(&nonce) {
            tracing::info!(run = %short(&nonce), from = pos, "wallet_start=refused reason=reused");
            return;
        }
        let join = match cur {
            None => true,
            Some((_, _, RunStage::Aborted | RunStage::Done, _)) => true,
            Some((.., true)) => true,
            // racing starts: the lowest starter position, then the lowest nonce
            Some((n, s, RunStage::Ready, _)) => (starter, nonce) < (s, n),
            Some(_) => false,
        };
        // a start not joined comes again on its starter's beat
        if join {
            self.wallet_join(nonce, starter);
        }
    }

    /// Join (or start) run `nonce`; frames that came early are replayed.
    fn wallet_join(&mut self, nonce: [u8; 32], starter: u16) {
        let Some((_, n, init, ..)) = self.wallet_run_ctx() else {
            return;
        };
        let Some(me) = self.wallet_pos(&self.member()) else {
            return;
        };
        self.purse.run.seen.insert(nonce);
        self.purse.run.left.remove(&nonce);
        if let Some(old) = self.purse.run.run.take() {
            // left in readiness for another start: it may come back
            if !old.persisted && old.stage == RunStage::Ready {
                self.purse.run.seen.remove(&old.nonce);
                self.purse.run.left.insert(old.nonce, old.starter);
            }
            if !old.persisted && old.stage != RunStage::Aborted {
                tracing::info!(init, run = %short(&old.nonce), reason = "superseded", "wallet_abort");
            }
        }
        let mut run = Run {
            init,
            nonce,
            starter,
            stage: RunStage::Ready,
            since: self.presence_now(),
            ready: BTreeSet::new(),
            reason: None,
            missing: Vec::new(),
            machine: None,
            round1: Inbox::new(n),
            key_machine: None,
            shares: Inbox::others(n, me),
            transcripts: BTreeMap::new(),
            transcript: None,
            out: Vec::new(),
            persisted: false,
        };
        if starter == me {
            run.out.push(WalletFrame::Start(WalletStartFrame { v: WALLET_V, init, run: hex::encode(nonce), starter }));
            if !self.session.settings.wallet_daemon_url.is_empty() {
                run.out.push(WalletFrame::Hint(WalletHintFrame {
                    v: WALLET_V,
                    init,
                    url: self.session.settings.wallet_daemon_url.clone(),
                }));
            }
        }
        let first: Vec<WalletFrame> = run.out.clone();
        self.purse.run.run = Some(run);
        tracing::info!(init, run = %short(&nonce), starter, "wallet_run=joined");
        for f in &first {
            self.wallet_send(f);
        }
        let early = std::mem::take(&mut self.purse.run.early);
        let (mine, rest): (Vec<_>, Vec<_>) =
            early.into_iter().partition(|(_, f)| f.run().and_then(hex32) == Some(nonce));
        self.purse.run.early = rest;
        self.wallet_advance();
        for (pos, f) in mine {
            self.wallet_ingest(pos, f);
        }
        self.wallet_advance();
    }

    /// Start a new run of the applied init as this seat.
    pub(crate) fn wallet_start(&mut self, nonce: Option<[u8; 32]>) -> Result<(), WalletRefusal> {
        let me = self.wallet_pos(&self.member()).ok_or(WalletRefusal::NotChain)?;
        self.wallet_run_ctx().ok_or(WalletRefusal::NoRun)?;
        let nonce = nonce.unwrap_or_else(fresh_nonce);
        let rounds = self.purse.run.run.as_ref().is_some_and(|r| {
            !r.persisted && matches!(r.stage, RunStage::Round1 | RunStage::Round2)
        });
        if rounds || self.purse.run.seen.contains(&nonce) {
            return Err(WalletRefusal::RunActive);
        }
        self.wallet_join(nonce, me);
        self.wallet_progress();
        Ok(())
    }

    fn wallet_send(&self, frame: &WalletFrame) -> bool {
        self.group_net.as_ref().is_some_and(|g| g.handle.publish_control(frame.to_frame()))
    }

    fn wallet_send_abort(&mut self, nonce: [u8; 32], reason: &str) {
        let init = self.wallet_init_applied().and_then(|i| i.id).unwrap_or(0);
        self.wallet_send(&WalletFrame::Abort(WalletAbortFrame {
            v: WALLET_V,
            init,
            run: hex::encode(nonce),
            reason: reason.to_string(),
        }));
        let now = self.presence_now();
        self.purse.run.aborted.insert(nonce, now);
    }

    /// End the current run before persist (I5): its reason, never a culprit.
    fn wallet_abort(&mut self, reason: &str, missing: Vec<u16>, from: Option<u16>, detail: &str, announce: bool) {
        let Some(run) = self.purse.run.run.as_mut() else {
            return;
        };
        if run.persisted || matches!(run.stage, RunStage::Aborted | RunStage::Done) {
            return;
        }
        run.stage = RunStage::Aborted;
        run.reason = Some(reason.to_string());
        run.missing = missing;
        run.wipe();
        let (init, nonce) = (run.init, run.nonce);
        tracing::warn!(init, run = %short(&nonce), reason, from = ?from, detail, "wallet_abort");
        if announce {
            self.wallet_send_abort(nonce, reason);
        } else {
            let now = self.presence_now();
            self.purse.run.aborted.insert(nonce, now);
        }
    }

    fn wallet_on_run_frame(&mut self, pos: u16, frame: WalletFrame) {
        let Some((_, n, ..)) = self.wallet_run_ctx() else {
            return;
        };
        let Some(me) = self.wallet_pos(&self.member()) else {
            return;
        };
        let Some(run) = self.purse.run.run.as_mut() else {
            return;
        };
        if matches!(run.stage, RunStage::Aborted | RunStage::Done) {
            return;
        }
        let refused: Option<(&str, String)> = match frame {
            WalletFrame::Ready(r) if r.ok => {
                run.ready.insert(pos);
                None
            }
            WalletFrame::Ready(_) => {
                return self.wallet_abort("declined", vec![pos], Some(pos), "", false);
            }
            WalletFrame::Abort(a) => {
                let missing = self.wallet_missing();
                let reason = REASONS.iter().find(|r| **r == a.reason).copied().unwrap_or("aborted");
                return self.wallet_abort(reason, missing, Some(pos), "remote", false);
            }
            WalletFrame::Round1(f) => match hex::decode(&f.msg.0) {
                Ok(msg) => {
                    // its sender was ready; a seat that rejoined learns it here
                    run.ready.insert(pos);
                    run.round1.record(pos, msg).err().map(|e| ("equivocation", e.to_string()))
                }
                Err(_) => None,
            },
            WalletFrame::Round2(f) => {
                let expected: BTreeSet<u16> = (1..=n).filter(|l| *l != pos).collect();
                let share = f.shares.get(&me).and_then(|s| hex::decode(&s.0).ok());
                match (hex32(&f.transcript), share.filter(|_| f.shares.keys().copied().eq(expected))) {
                    (Some(_), None) => Some(("invalid", "share set".to_string())),
                    (Some(t), Some(_)) if run.transcripts.get(&pos).is_some_and(|seen| *seen != t) => {
                        Some(("equivocation", "transcript".to_string()))
                    }
                    (Some(t), Some(share)) => {
                        run.transcripts.insert(pos, t);
                        run.shares.record(pos, share).err().map(|e| ("equivocation", e.to_string()))
                    }
                    (None, _) => None,
                }
            }
            _ => None,
        };
        if let Some((reason, detail)) = refused {
            return self.wallet_abort(reason, Vec::new(), Some(pos), &detail, true);
        }
        self.wallet_advance();
    }

    /// Does this seat stand ready (build, daemon, consent)? Asks the
    /// daemon when it has no height yet.
    fn wallet_ready_here(&mut self, init: u64, network: Network) -> bool {
        let consent = self.chain.own_approvals.contains(&init) || self.purse.run.consented.contains(&init);
        let s = &self.session.settings;
        if s.wallet_daemon_url.is_empty() || network_of(&s.wallet_network) != Some(network) {
            return false;
        }
        if self.purse.height.is_none() {
            if !self.purse.probing {
                let _ = self.start_wallet_probe(None);
            }
            return false;
        }
        consent
    }

    /// Move the current run as far as its frames allow.
    fn wallet_advance(&mut self) {
        let Some((m, n, init, birthday, network)) = self.wallet_run_ctx() else {
            return;
        };
        let Some(me) = self.wallet_pos(&self.member()) else {
            return;
        };
        let Some(rid) = self.wallet_republic_raw() else {
            return;
        };
        let now = self.presence_now();
        let stage = match self.purse.run.run.as_ref() {
            Some(r) if r.init == init => r.stage,
            _ => return,
        };
        if stage == RunStage::Ready {
            let sent = self.purse.run.run.as_ref().is_some_and(|r| r.ready.contains(&me));
            if !sent && self.wallet_ready_here(init, network) {
                let Some(run) = self.purse.run.run.as_mut() else {
                    return;
                };
                run.ready.insert(me);
                let f = WalletFrame::Ready(WalletReadyFrame { v: WALLET_V, init, run: hex::encode(run.nonce), ok: true });
                run.out.push(f.clone());
                self.wallet_send(&f);
            }
            let Some(run) = self.purse.run.run.as_mut() else {
                return;
            };
            if run.ready.len() < usize::from(n) {
                return;
            }
            let Ok(params) = dkg::params(m, n, me) else {
                return;
            };
            let run_id = RunId { republic_id: rid, init_id: init, run: run.nonce };
            let (machine, msg) = dkg::round1(params, dkg::context(&run_id, m, n), &mut rng());
            let f = WalletFrame::Round1(WalletRound1Frame {
                v: WALLET_V,
                init,
                run: hex::encode(run.nonce),
                msg: molt_core::vault::SecretHex(hex::encode(&*msg)),
            });
            if run.round1.record(me, msg.to_vec()).is_err() {
                return;
            }
            run.machine = Some(machine);
            run.stage = RunStage::Round1;
            run.since = now;
            run.out.push(f.clone());
            self.wallet_send(&f);
        }
        if self.purse.run.run.as_ref().is_some_and(|r| r.stage == RunStage::Round1 && r.round1.is_complete()) {
            self.wallet_round2(m, n, me, init);
        }
        if self.purse.run.run.as_ref().is_some_and(|r| r.stage == RunStage::Round2) {
            self.wallet_complete(m, n, me, init, birthday, network, rid);
        }
    }

    fn wallet_round2(&mut self, m: u16, n: u16, me: u16, init: u64) {
        let Some(run) = self.purse.run.run.as_mut() else {
            return;
        };
        let (Ok(params), Some(machine)) = (dkg::params(m, n, me), run.machine.take()) else {
            return;
        };
        let others: Frames = run.round1.frames().iter().filter(|(l, _)| **l != me).map(|(l, b)| (*l, b.clone())).collect();
        let t = match dkg::transcript(&run.nonce, run.round1.frames(), n) {
            Ok(t) => t,
            Err(e) => return self.wallet_abort("invalid", Vec::new(), None, &e.to_string(), true),
        };
        let out = dkg::round2(machine, params, &others, &mut rng());
        let Some(run) = self.purse.run.run.as_mut() else {
            return;
        };
        let (km, shares) = match out {
            Ok(x) => x,
            Err(e) => {
                let from = match e {
                    molt_treasury::TreasuryError::Frame(l) | molt_treasury::TreasuryError::Identity(l) => Some(l),
                    _ => None,
                };
                return self.wallet_abort("invalid", Vec::new(), from, &e.to_string(), true);
            }
        };
        run.key_machine = Some(km);
        run.transcript = Some(t);
        run.stage = RunStage::Round2;
        let f = WalletFrame::Round2(WalletRound2Frame {
            v: WALLET_V,
            init,
            run: hex::encode(run.nonce),
            shares: shares.iter().map(|(to, s)| (*to, molt_core::vault::SecretHex(hex::encode(s)))).collect(),
            transcript: hex::encode(t),
        });
        run.out.push(f.clone());
        self.wallet_send(&f);
    }

    #[allow(clippy::too_many_arguments)]
    fn wallet_complete(&mut self, m: u16, n: u16, me: u16, init: u64, birthday: u64, network: Network, rid: [u8; 32]) {
        let Some(run) = self.purse.run.run.as_ref() else {
            return;
        };
        let Some(mine) = run.transcript else {
            return;
        };
        if let Some((l, _)) = run.transcripts.iter().find(|(_, t)| **t != mine) {
            let l = *l;
            return self.wallet_abort("transcript", Vec::new(), Some(l), "", true);
        }
        if !run.shares.is_complete() || run.transcripts.len() + 1 != usize::from(n) {
            return;
        }
        let Some(sk) = self.identity_sk.clone() else {
            return;
        };
        let Some(run) = self.purse.run.run.as_mut() else {
            return;
        };
        let (Ok(params), Some(km)) = (dkg::params(m, n, me), run.key_machine.take()) else {
            return;
        };
        let run_id = RunId { republic_id: rid, init_id: init, run: run.nonce };
        let done = dkg::complete(km, params, run.shares.frames(), &mut rng()).and_then(|k| {
            let view = keys::view_key(&run_id, run.round1.frames(), n)?;
            let address = keys::standard_address(k.group_key(), view.clone(), network)?.to_string();
            Ok((k, view, address))
        });
        let (k, view, address) = match done {
            Ok(x) => x,
            Err(e) => return self.wallet_abort("invalid", Vec::new(), None, &e.to_string(), true),
        };
        let attestation = attest::sign(&sk, &Attested { run: run_id, transcript: mine, address: &address, network, m, n, birthday });
        let rec = KeysRecord {
            run: run_id,
            transcript: mine,
            address,
            network,
            m,
            n,
            birthday,
            attestation,
            share: k.serialize(),
            view: keys::scalar_bytes(&view),
        };
        // I4: the share is on disk before the attestation exists outside
        let durable = self.active.as_ref().is_some_and(|a| a.handle.persist_wallet_keys_blocking(rec.encode()));
        if !durable {
            return self.wallet_abort("storage", Vec::new(), None, "keys not persisted", true);
        }
        let now = self.presence_now();
        let Some(run) = self.purse.run.run.as_mut() else {
            return;
        };
        run.persisted = true;
        run.stage = RunStage::Attest;
        run.since = now;
        // the frames stay: a seat that missed one still needs it
        let nonce = run.nonce;
        tracing::info!(init, run = %short(&nonce), "wallet_keys=persisted");
        self.purse.run.records.push(rec);
        self.wallet_on_attest(me, nonce, attestation);
        self.wallet_send_attestations();
    }

    /// Every held record's attestation, unless withheld (a test seam).
    fn wallet_send_attestations(&mut self) {
        if self.wallet_seams.withhold.load(Ordering::SeqCst) || self.wallet_purse().is_some() {
            return;
        }
        let frames: Vec<WalletFrame> = self
            .purse
            .run
            .records
            .iter()
            .map(|r| {
                WalletFrame::Attest(WalletAttestFrame {
                    v: WALLET_V,
                    init: r.run.init_id,
                    run: hex::encode(r.run.run),
                    sig: hex::encode(r.attestation),
                })
            })
            .collect();
        for f in &frames {
            self.wallet_send(f);
        }
    }

    fn wallet_on_attest(&mut self, pos: u16, nonce: [u8; 32], sig: [u8; 64]) {
        let held = self.purse.run.records.iter().any(|r| r.run.run == nonce);
        let attests = &mut self.purse.run.attests;
        if !held && !attests.contains_key(&nonce) && attests.len() >= FOREIGN_RUNS_MAX {
            return;
        }
        attests.entry(nonce).or_default().insert(pos, sig);
        self.wallet_check_attests(nonce);
    }

    /// n valid attestations for a run this seat holds: it may propose.
    fn wallet_check_attests(&mut self, nonce: [u8; 32]) {
        let Some(rid) = self.wallet_republic_raw() else {
            return;
        };
        let Some(rec) = self.purse.run.records.iter().find(|r| r.run.run == nonce) else {
            return;
        };
        let n = rec.n;
        let pks: Vec<String> = self.vault_founding_table().into_iter().map(|i| i.identity_pk).collect();
        let Some(set) = self.purse.run.attests.get_mut(&nonce) else {
            return;
        };
        if set.len() < usize::from(n) {
            return;
        }
        let c = record_created(rec, set.values().copied().collect());
        match attest::verify_all(&attested_of(&c, rid), &pks, &c.sigs) {
            Ok(()) => {}
            Err(molt_treasury::TreasuryError::Attestation(i)) => {
                tracing::warn!(run = %short(&nonce), from = i, "wallet_attest=invalid");
                set.remove(&i);
                return;
            }
            Err(_) => return,
        }
        let now = self.presence_now();
        self.purse.run.complete_at.entry(nonce).or_insert(now);
        if let Some(run) = self.purse.run.run.as_mut().filter(|r| r.nonce == nonce) {
            run.stage = RunStage::Sealing;
        }
        self.wallet_maybe_propose(nonce);
    }

    /// Position 1 proposes at once, position k after `(k-1)` steps, while
    /// no card for this run is open (design §3.5).
    fn wallet_maybe_propose(&mut self, nonce: [u8; 32]) {
        if self.purse.run.proposed.contains(&nonce)
            || self.wallet_purse().is_some()
            || self.wallet_seams.withhold.load(Ordering::SeqCst)
        {
            return;
        }
        let Some(at) = self.purse.run.complete_at.get(&nonce).copied() else {
            return;
        };
        let Some(me) = self.wallet_pos(&self.member()) else {
            return;
        };
        let open = self.proposals.values().any(|p| {
            p.surface == Surface::Wallet
                && p.state == ProposalState::Proposed
                && parse_created(&p.payload).is_some_and(|c| c.run == nonce)
        });
        if open {
            return;
        }
        let wait = u64::from(me - 1).saturating_mul(self.wallet_step());
        if self.presence_now().saturating_sub(at) < wait {
            return;
        }
        let Some(rec) = self.purse.run.records.iter().find(|r| r.run.run == nonce) else {
            return;
        };
        let Some(set) = self.purse.run.attests.get(&nonce) else {
            return;
        };
        let payload = created_value(&record_created(rec, set.values().copied().collect()));
        self.purse.run.proposed.insert(nonce);
        match self.propose_payload(Surface::Wallet, payload) {
            Ok(_) => tracing::info!(run = %short(&nonce), "wallet_created=proposed"),
            Err(e) => tracing::warn!(run = %short(&nonce), error = %e, "wallet_created=not_proposed"),
        }
    }

    /// Co-sign every open `wallet_created` that matches a held record (I2).
    pub(crate) fn wallet_cosign_review(&mut self) {
        let ids: Vec<u64> = self
            .proposals
            .iter()
            .filter(|(_, p)| {
                p.surface == Surface::Wallet && p.state == ProposalState::Proposed && parse_created(&p.payload).is_some()
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let mine = self.chain.own_approvals.contains(&id);
            if mine && self.own_signature_stands(id) {
                continue;
            }
            let Some(payload) = self.proposals.get(&id).map(|p| p.payload.clone()) else {
                continue;
            };
            if self.wallet_created_check(&payload).is_ok() {
                tracing::info!(id, "wallet_created=cosigned");
                self.chain_sign_and_gossip_approval(id);
            }
        }
    }

    /// A Wallet block applied in this session: the init's auto-start (§3.3,
    /// this hook only) and the purse's commit.
    pub(crate) fn after_wallet_applied(&mut self, proposal_id: u64, payload: &Value) {
        let first_init =
            parse_init(payload).is_some() && self.wallet_init_applied().and_then(|i| i.id) == Some(proposal_id);
        if first_init && self.wallet_run_ctx().is_some() && self.purse.run.started.insert(proposal_id) {
            let now = self.presence_now();
            if self.wallet_pos(&self.member()) == Some(1) {
                let _ = self.wallet_start(None);
            } else {
                self.purse.run.auto_from = Some((proposal_id, now));
            }
            // frames that outran the init
            let early = std::mem::take(&mut self.purse.run.early);
            for (pos, f) in early {
                self.wallet_ingest(pos, f);
            }
        }
        if self.wallet_settle() {
            if let Some(p) = self.wallet_purse() {
                self.emit(Event::WalletCreated { address: p.created.address });
            }
        }
        self.wallet_progress();
    }

    /// The purse committed: siblings die, the other runs' records go, the
    /// run is done. Idempotent; `true` the first time for this purse.
    pub(crate) fn wallet_settle(&mut self) -> bool {
        let Some(purse) = self.wallet_purse() else {
            return false;
        };
        if self.purse.run.settled == Some(purse.id) {
            return false;
        }
        self.purse.run.settled = Some(purse.id);
        let c = &purse.created;
        let siblings: Vec<u64> = self
            .proposals
            .iter()
            .filter(|(id, p)| {
                Some(**id) != purse.id
                    && p.surface == Surface::Wallet
                    && p.state == ProposalState::Proposed
                    && parse_created(&p.payload).is_some()
            })
            .map(|(id, _)| *id)
            .collect();
        for id in siblings {
            if let Some(p) = self.proposals.get_mut(&id) {
                p.state = ProposalState::Rejected;
                p.superseded = true;
                p.superseded_kind = Some(molt_core::SupersededKind::Conflict);
            }
            self.stash_voted(id);
            self.chain.pending_sigs.remove(&id);
            tracing::info!(id, "wallet_created=superseded");
        }
        let keep = self
            .purse
            .run
            .records
            .iter()
            .position(|r| r.run.init_id == c.init && r.run.run == c.run && r.address == c.address);
        match keep {
            Some(k) => {
                let rec = self.purse.run.records.swap_remove(k);
                if !self.purse.run.records.is_empty() {
                    let pruned = self.active.as_ref().is_some_and(|a| a.handle.prune_wallet_keys_blocking(rec.encode()));
                    if pruned {
                        self.purse.run.records.clear();
                    } else {
                        tracing::error!("wallet_keys=prune_failed");
                    }
                }
                self.purse.run.records.insert(0, rec);
                self.purse.run.watch_only = false;
            }
            None => self.wallet_view_only(),
        }
        if let Some(run) = self.purse.run.run.as_mut() {
            run.stage = RunStage::Done;
            run.wipe();
        }
        tracing::info!(address = %c.address, "wallet_purse=committed");
        true
    }

    /// This seat holds no key part of the purse: loud, once.
    fn wallet_view_only(&mut self) {
        if !self.purse.run.watch_only {
            self.purse.run.watch_only = true;
            tracing::warn!(seat = "watch_only", "wallet_keys=not_held");
            self.session.notice = "purse: view only - no key part here".to_string();
            self.emit_session(molt_core::SessionScope::Full);
        }
    }

    /// The open path: the keys file is read, checked against the purse,
    /// and a run that persisted re-attests (§3.5); nothing is started.
    pub(crate) fn wallet_on_open(&mut self, records: Result<molt_storage::WalletRecords, molt_storage::StorageError>) {
        self.purse.run.records = match records {
            Ok(rs) => rs
                .iter()
                .filter_map(|b| match KeysRecord::decode(b) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        tracing::warn!(error = %e, "wallet_keys=record_invalid");
                        None
                    }
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "wallet_keys=unreadable");
                Vec::new()
            }
        };
        if self.wallet_purse().is_some() {
            self.wallet_settle();
            return;
        }
        let me = self.wallet_pos(&self.member());
        let mine: Vec<([u8; 32], [u8; 64])> = self.purse.run.records.iter().map(|r| (r.run.run, r.attestation)).collect();
        for (nonce, sig) in mine {
            self.purse.run.seen.insert(nonce);
            if let Some(me) = me {
                self.wallet_on_attest(me, nonce, sig);
            }
        }
        self.wallet_send_attestations();
        self.wallet_cosign_review();
    }

    /// The beat: seams, deadlines, fallbacks, resends.
    pub(crate) fn wallet_run_tick(&mut self, now: u64) {
        let forced = self.wallet_seams.start.lock().ok().and_then(|mut s| s.take());
        if let Some(nonce) = forced {
            if let Err(e) = self.wallet_start(Some(nonce)) {
                tracing::warn!(error = %e, "wallet_start=refused");
            }
        }
        if self.wallet_run_ctx().is_none() {
            if self.purse.run.run.as_ref().is_some_and(|r| r.stage != RunStage::Done) && self.wallet_settle() {
                self.wallet_progress();
            }
            return;
        }
        self.wallet_deadlines(now);
        self.wallet_fallback_start(now);
        let complete: Vec<[u8; 32]> = self.purse.run.complete_at.keys().copied().collect();
        for nonce in complete {
            self.wallet_maybe_propose(nonce);
        }
        if now.saturating_sub(self.purse.run.resent_at) >= RESEND_SECS {
            self.purse.run.resent_at = now;
            let out: Vec<WalletFrame> = self.purse.run.run.as_ref().filter(|r| r.stage != RunStage::Aborted).map(|r| r.out.clone()).unwrap_or_default();
            for f in &out {
                self.wallet_send(f);
            }
            self.wallet_send_attestations();
            self.wallet_cosign_review();
            self.wallet_advance();
        }
        self.wallet_progress();
    }

    fn wallet_deadlines(&mut self, now: u64) {
        let limit = self.wallet_deadline();
        let Some(run) = self.purse.run.run.as_ref() else {
            return;
        };
        if run.persisted || now.saturating_sub(run.since) < limit {
            return;
        }
        let reason = match run.stage {
            RunStage::Ready => "not ready",
            RunStage::Round1 | RunStage::Round2 => "timeout",
            _ => return,
        };
        let missing = self.wallet_missing();
        self.wallet_abort(reason, missing, None, "deadline", true);
    }

    /// The seats the current stage still waits for.
    fn wallet_missing(&self) -> Vec<u16> {
        let (Some(run), Some((_, n)), Some(me)) =
            (self.purse.run.run.as_ref(), self.wallet_rule(), self.wallet_pos(&self.member()))
        else {
            return Vec::new();
        };
        let all = 1..=u16::from(n);
        match run.stage {
            RunStage::Ready => all.filter(|p| !run.ready.contains(p)).collect(),
            RunStage::Round1 => all.filter(|p| !run.round1.frames().contains_key(p)).collect(),
            RunStage::Round2 => all.filter(|p| *p != me && !run.transcripts.contains_key(p)).collect(),
            _ => Vec::new(),
        }
    }

    /// The next position starts when the one before it stays silent.
    fn wallet_fallback_start(&mut self, now: u64) {
        let Some((init, at)) = self.purse.run.auto_from else {
            return;
        };
        if self.purse.run.run.is_some() {
            self.purse.run.auto_from = None;
            return;
        }
        let Some(me) = self.wallet_pos(&self.member()) else {
            return;
        };
        if now.saturating_sub(at) >= u64::from(me - 1).saturating_mul(self.wallet_step()) {
            self.purse.run.auto_from = None;
            tracing::info!(init, position = me, "wallet_start=fallback");
            let _ = self.wallet_start(None);
        }
    }

    /// The current run as wizard, panel and MCP see it (plan §7.5).
    pub(crate) fn wallet_run_view(&self) -> Option<WalletRunView> {
        let run = self.purse.run.run.as_ref()?;
        let n = self.wallet_rule().map_or(0, |(_, n)| u16::from(n));
        let me = self.wallet_pos(&self.member()).unwrap_or(0);
        let all = u32::from(n);
        let count = |k: usize| u32::try_from(k).unwrap_or(u32::MAX);
        let (done, missing) = match run.stage {
            RunStage::Ready => (count(run.ready.len()), self.wallet_names((1..=n).filter(|p| !run.ready.contains(p)))),
            RunStage::Round1 => (count(run.round1.frames().len()), Vec::new()),
            RunStage::Round2 => (count(run.transcripts.len() + usize::from(run.transcript.is_some())), Vec::new()),
            RunStage::Attest => (count(self.purse.run.attests.get(&run.nonce).map_or(0, BTreeMap::len)), Vec::new()),
            RunStage::Sealing | RunStage::Done => (all, Vec::new()),
            RunStage::Aborted => (0, self.wallet_names(run.missing.iter().copied())),
        };
        let consent = self.chain.own_approvals.contains(&run.init) || self.purse.run.consented.contains(&run.init);
        Some(WalletRunView {
            stage: run.stage,
            done,
            of: all,
            missing,
            reason: run.reason.clone(),
            needs_consent: run.stage == RunStage::Ready && !consent && !run.ready.contains(&me),
            daemon_hint: self.purse.run.hint.clone(),
        })
    }

    /// Readiness may hold now (a daemon answered).
    pub(crate) fn wallet_advance_now(&mut self) {
        self.wallet_advance();
        self.wallet_progress();
    }

    /// One event per change of the run view.
    pub(crate) fn wallet_progress(&mut self) {
        let view = self.wallet_run_view();
        if view != self.purse.run.last_view {
            self.purse.run.last_view.clone_from(&view);
            if let Some(run) = view {
                self.emit(Event::WalletRunProgress { run });
            }
        }
    }

    /// [`molt_core::Command::WalletConsent`].
    pub(crate) fn cmd_wallet_consent(&mut self, accept: bool) -> Result<Reply, MoltError> {
        let Some((init, nonce)) = self
            .purse
            .run
            .run
            .as_ref()
            .filter(|r| r.stage == RunStage::Ready)
            .map(|r| (r.init, r.nonce))
        else {
            return Err(MoltError::Wallet(WalletRefusal::NoRun));
        };
        if accept {
            self.purse.run.consented.insert(init);
            self.wallet_advance();
        } else {
            let me = self.wallet_pos(&self.member()).unwrap_or(0);
            self.wallet_send(&WalletFrame::Ready(WalletReadyFrame { v: WALLET_V, init, run: hex::encode(nonce), ok: false }));
            self.wallet_abort("declined", vec![me], Some(me), "", false);
        }
        self.wallet_progress();
        Ok(Reply::Ack)
    }

    /// [`molt_core::Command::WalletRetry`]: a fresh nonce for the applied init.
    pub(crate) fn cmd_wallet_retry(&mut self) -> Result<Reply, MoltError> {
        if self.wallet_purse().is_some() {
            return Err(MoltError::Wallet(WalletRefusal::InitExists));
        }
        let busy = self.purse.run.run.as_ref().is_some_and(|r| {
            !r.persisted && matches!(r.stage, RunStage::Ready | RunStage::Round1 | RunStage::Round2)
        });
        if busy {
            return Err(MoltError::Wallet(WalletRefusal::RunActive));
        }
        self.wallet_start(None).map_err(MoltError::Wallet)?;
        Ok(Reply::Ack)
    }
}

#[cfg(test)]
#[path = "wallet_run_tests.rs"]
mod tests;
