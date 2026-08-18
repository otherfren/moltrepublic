// SPDX-License-Identifier: GPL-3.0-or-later

//! **Verification of the persistent commit-block chain** (see
//! [`molt_core::chain`]). This is the security layer that makes a handed-over
//! chain *self-authenticating*: a member — or a rejoiner who fetched the chain
//! from an untrusted peer — checks it here with no live mesh and no trust in the
//! deliverer.
//!
//! The split mirrors the founding roster exactly: the canonical bytes live in
//! `molt-core`, the SHA-256 hash in `molt-storage`, and the Ed25519 checks here
//! next to [`crate::founding::verify_sealed_roster`] — of which the genesis
//! check below is a strict generalization (same `republic_id` content-match,
//! same per-member `identity_verify` over [`molt_core::roster_canonical_bytes`]).
//!
//! **Every check is hard-reject.** A bad signature, a broken `prev` link, a gap
//! in the heights, a genesis whose id does not match its content, a double-apply
//! — any of these fails the whole verification. Determinism across every
//! member's converged state demands nothing weaker.

use std::collections::BTreeSet;

use molt_core::{
    approval_bytes, block_link_bytes, ChainBlock, ChainChange, Event, MemberIdentity, MembershipOp,
    ProposalId, ProposalState, RosterAttestation, SealedRoster, Surface, WorkspaceEvent,
    GENESIS_PREV,
};

use crate::State;

/// WP4b automation: the chain length (blocks held locally, anchor
/// included) at which the lowest-named member auto-proposes the next
/// compaction cut. Blocks are governance decisions — rare — so this is
/// months of activity for a small republic, and after each cut the local
/// chain shrinks back to the anchor + suffix. A constant, not a setting:
/// compaction is hygiene, not policy (`docs_archive/chain/log_compaction.md`).
pub(crate) const AUTO_CHECKPOINT_MIN_LEN: usize = 32;

/// The **ephemeral** signature collection for one pending proposal on a
/// chain-governed republic (never persisted; rebuilt from gossip). The
/// committer bundles these into a block once `sigs` reaches the threshold. A
/// re-base (the head advanced past `height`) clears it and re-signs.
/// L3: open cards one proposer may hold at once — a flooding member can
/// only crowd itself (the shed card is re-earned by the WP2 re-serve).
const OPEN_CARDS_PER_PROPOSER_MAX: usize = 64;

/// L3: how far past the head a buffered future block may claim to be, and
/// the buffer's size bound — larger than any served suffix batch, small
/// enough that ~96 KiB frames cannot pin unbounded RAM.
const CATCHUP_BUFFER_WINDOW: u64 = 4096;

#[derive(Debug, Clone, Default)]
pub(crate) struct PendingApproval {
    /// The chain height every signature here is bound to.
    pub height: u64,
    /// One signature per distinct member (latest wins).
    pub sigs: Vec<RosterAttestation>,
    /// Members whose CURRENT signature verified against the live target's
    /// approval bytes (L2): the DISPLAY reads only these — a raw collected
    /// sig could paint a forged stance onto a named seat. `try_commit`
    /// keeps its own authoritative filter; a sig unverifiable YET (its
    /// card has not landed) stays collected and is re-checked on arrival.
    pub verified: std::collections::BTreeSet<String>,
}

/// A recovery in flight on the coordinator: the returning member's fresh MLS
/// KeyPackage + reply-queue handover, kept keyed by the re-admission proposal id
/// until its `Restored` block commits — then the coordinator re-keys the group
/// (`restore_member`) and sends the Welcome back to `reply`.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed by the MLS re-key increment (restore_member + Welcome)
pub(crate) struct PendingRecovery {
    pub member: String,
    pub key_package: String,
    pub reply: String,
}

/// What a Nostr re-key produced — everything the wire needs, keyed at the
/// stamp the commit was made with.
pub(crate) struct NostrRekey {
    /// The raw MLS commit. It ships RAW inside the 445, never wrapped in an
    /// application ciphertext: a recipient needs it to REACH the new epoch.
    pub commit: Vec<u8>,
    /// The MLS Welcome that puts the returning seat back in the group.
    pub welcome: Vec<u8>,
    /// The exporter secret of the epoch this node just **left** — the one its
    /// recipients are still at.
    ///
    /// A receiver's exporter ring reaches BACKWARD only, so a commit sealed
    /// under the new epoch is opaque to exactly the members it exists to move
    /// forward (`9900f36`). The queue path has no outer layer, which is why
    /// this only bites on 445.
    pub prev_exporter: [u8; 32],
    /// The carrier stamp the commit was keyed with, and the one it MUST be
    /// published at.
    ///
    /// `CommitKey(created_at, sha256(commit))` breaks a concurrent same-epoch
    /// race, and the rule (`molt-net/CLAUDE.md`) is that the stamp comes from
    /// the same source on both sides. The 445 receive side reads the real
    /// `created_at` off the wire, so a sender that let the outbox pick the
    /// publish time would key its own commit at one value while every
    /// receiver keys it at another — the two ends then pick different winners
    /// and diverge permanently under ONE epoch number, silently.
    pub stamp: u64,
}

/// The **working transport anchor** per seat, folded from a verified chain in
/// block order — the last `Restored` block for a seat wins (a seat can
/// recover more than once).
///
/// Deliberately separate from the roster: `apply_membership` keeps a seat's
/// anchored *identity* key across a `Restored` block (a different one there
/// would let m-of-n survivors hijack the seat), so the re-anchored transport
/// key is a projection ALONGSIDE the roster rather than an edit of it. Which
/// means a reader asking "where do I address this member" must ask here, not
/// `head.identities`.
pub(crate) fn working_anchors(
    blocks: &[ChainBlock],
) -> std::collections::HashMap<molt_core::MemberId, String> {
    let mut anchors = std::collections::HashMap::new();
    for block in blocks {
        if let ChainChange::Membership {
            member,
            nostr_pk: Some(pk),
            ..
        } = &block.change
        {
            if !pk.is_empty() {
                anchors.insert(member.clone(), pk.clone());
            }
        }
    }
    anchors
}

/// The relay LEDGER folded from a verified chain in block order (R3b): each
/// seat's DECLARED reachable pool — the last declaration wins, exactly like
/// [`working_anchors`]. A member without an entry is covered by the ratified
/// group pool.
pub(crate) fn declared_relays(
    blocks: &[ChainBlock],
) -> std::collections::HashMap<molt_core::MemberId, Vec<String>> {
    let mut ledger = std::collections::HashMap::new();
    for block in blocks {
        if let ChainChange::Membership { member, relays, .. } = &block.change {
            if !relays.is_empty() {
                ledger.insert(member.clone(), relays.clone());
            }
        }
    }
    ledger
}

/// Re-key a Nostr republic's group: replace `member`'s leaf, at a carrier
/// stamp the caller pinned **before** the commit was made.
///
/// The mesh twin is `NetRuntime::restore_member_on_group`, which reaches the
/// group through `real_crypto` — a Nostr republic has no `NetRuntime` at all,
/// its group MLS lives on `GroupNet`.
pub(crate) fn nostr_rekey(
    mls: &std::sync::Mutex<molt_net::MlsMember>,
    member: &str,
    key_package: &[u8],
    stamp: u64,
) -> Result<NostrRekey, String> {
    let mut group = mls
        .lock()
        .map_err(|_| "the group lock is poisoned".to_string())?;
    let (commit, welcome) = group
        .restore_member(member, key_package, stamp)
        .map_err(|e| e.to_string())?;
    // read AFTER the commit: the ring's newest entry is now the epoch the
    // commit was made from, which is where its recipients still are
    let prev_exporter = group.exporter_ring().first().copied().ok_or_else(|| {
        "the re-key left no previous exporter — the commit would seal opaque".to_string()
    })?;
    Ok(NostrRekey { commit, welcome, prev_exporter, stamp })
}

/// The verified head of a chain plus the roster it establishes: everything a
/// caller needs to check the *next* block or to trust a synced chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainHead {
    /// Height of the last verified block.
    pub height: u64,
    /// [`block_hash`] of the last verified block — the `prev` the next block
    /// must carry.
    pub hash: String,
    /// The content-derived republic id, fixed by the genesis.
    pub republic_id: String,
    /// Approval threshold (m) for every post-genesis block.
    pub rule_m: u8,
    /// The live roster (name → identity key) after all membership blocks.
    pub identities: Vec<MemberIdentity>,
}

/// SHA-256 (hex) of a block's [`block_link_bytes`] — the value the next block's
/// `prev` points at.
pub(crate) fn block_hash(republic_id: &str, block: &ChainBlock) -> String {
    molt_storage::content_hash(&block_link_bytes(republic_id, block))
}

/// One real chain block as the Chain-History display row
/// ([`molt_core::ChainBlockView`]): kind + surface + display payload +
/// consumed proposal id + the signer names in block order. Pure projection —
/// display data, never consensus input.
fn chain_block_view(block: &ChainBlock) -> molt_core::ChainBlockView {
    let signers = block.sigs.iter().map(|a| a.member.clone()).collect();
    let (kind, surface, payload, proposal_id) = match &block.change {
        ChainChange::Genesis { name, .. } => (
            "genesis",
            String::new(),
            serde_json::Value::String(name.clone()),
            0,
        ),
        ChainChange::Applied {
            proposal_id,
            surface,
            payload,
        } => (
            "applied",
            surface.as_str().to_string(),
            payload.clone(),
            *proposal_id,
        ),
        ChainChange::Membership { op, member, .. } => {
            let verb = match op {
                MembershipOp::Joined => "joined",
                MembershipOp::Restored => "restored",
            };
            (
                "membership",
                String::new(),
                serde_json::Value::String(format!("{verb} {member}")),
                0,
            )
        }
        ChainChange::Checkpoint { upto, .. } => (
            "checkpoint",
            String::new(),
            serde_json::Value::from(*upto),
            0,
        ),
    };
    molt_core::ChainBlockView {
        height: block.height,
        kind: kind.to_string(),
        surface,
        payload,
        proposal_id,
        signers,
    }
}

/// The distinct roster members who validly signed `bytes`. A signature from an
/// unknown member, a bad signature, or the same member signing twice can never
/// inflate the count past the number of real, distinct approvers.
fn valid_signers(
    identities: &[MemberIdentity],
    bytes: &[u8],
    sigs: &[RosterAttestation],
) -> BTreeSet<String> {
    let mut ok = BTreeSet::new();
    for att in sigs {
        let Some(id) = identities.iter().find(|i| i.member == att.member) else {
            continue;
        };
        if molt_storage::identity_verify(&id.identity_pk, bytes, &att.sig) {
            ok.insert(att.member.clone());
        }
    }
    ok
}

/// Evolve the roster for the blocks that *follow* a membership change.
fn apply_membership(
    identities: &mut Vec<MemberIdentity>,
    op: MembershipOp,
    member: &str,
    identity_pk: &str,
) -> Result<(), String> {
    match op {
        MembershipOp::Joined => {
            if identities.iter().any(|i| i.member == member) {
                return Err(format!("member {member} is already in the roster"));
            }
            identities.push(MemberIdentity {
                member: member.to_string(),
                identity_pk: identity_pk.to_string(),
                // the Membership change carries no nostr anchor yet (the
                // ChainChange layout is additive-only); a joined-later seat
                // reads as legacy until a versioned Membership binds it
                nostr_pk: String::new(),
            });
        }
        MembershipOp::Restored => {
            let Some(id) = identities.iter_mut().find(|i| i.member == member) else {
                return Err(format!("cannot restore unknown member {member}"));
            };
            // recovery re-derives the SAME identity: a Restored block re-keys
            // the MLS leaf, never the roster identity (`recovery_ritual.md`
            // §6). A block that presents a different key would let m-of-n
            // survivors hijack a seat — hard-reject it here, at the verifier,
            // not only at the coordinator's propose step.
            if id.identity_pk != identity_pk {
                return Err(format!(
                    "a Restored block must keep {member}'s anchored identity key"
                ));
            }
        }
    }
    Ok(())
}

/// Verify block 0: it must be a genesis whose `republic_id` is the neutral
/// content-derived value, sealed **unanimously** (n-of-n) by every anchored
/// member — the founding attestations. Returns the initial head.
fn verify_genesis(block: &ChainBlock) -> Result<ChainHead, String> {
    if block.height != 0 {
        return Err("genesis is not at height 0".to_string());
    }
    if block.prev != GENESIS_PREV {
        return Err("genesis prev is not the zero root".to_string());
    }
    let ChainChange::Genesis {
        name,
        republic_id,
        rule_m,
        rule_n,
        identities,
        agenda: _,
        relays: _,
        features: _,
    } = &block.change
    else {
        return Err("block 0 is not a genesis".to_string());
    };
    if *rule_m == 0 || rule_m > rule_n {
        return Err("genesis threshold is out of range".to_string());
    }
    if usize::from(*rule_n) != identities.len() {
        return Err("genesis roster size does not match n".to_string());
    }
    let rid = molt_storage::republic_id(name, *rule_m, *rule_n, identities);
    if &rid != republic_id {
        return Err("genesis republic id does not match its content".to_string());
    }
    let bytes = approval_bytes(republic_id, 0, &block.change);
    let signers = valid_signers(identities, &bytes, &block.sigs);
    if signers.len() != identities.len() {
        return Err("genesis is not fully signed by every member".to_string());
    }
    Ok(ChainHead {
        height: 0,
        hash: block_hash(republic_id, block),
        republic_id: republic_id.clone(),
        rule_m: *rule_m,
        identities: identities.clone(),
    })
}

#[cfg(test)]
thread_local! {
    /// Test-only count of per-block verifications. **Thread-local on purpose**:
    /// each test runs on its own thread, so a shared counter would be raced by
    /// every other chain test in the binary.
    ///
    /// This is the only way to state the complexity claim as an assertion
    /// rather than a timing: a holder that re-walks its chain per block shows
    /// up here as `N²`, not `N`.
    pub(crate) static VERIFY_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };

    /// Test-only count of whole-chain writes. Same reason as `VERIFY_STEPS`:
    /// the write is a BLOCKING round-trip to the storage writer, so "once per
    /// batch" is a claim worth asserting rather than describing.
    pub(crate) static CHAIN_PERSISTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The distinct members whose voices back ONE block against `identities` —
/// the block's own signatures plus, on a restore, the returning seat's
/// consent. Position-bound: [`approval_bytes`] folds `block.height` in, so
/// this answers "who approved THIS change at THIS height" and nothing else,
/// which is why it also serves the detached bundle verifier
/// ([`verify_wiki_export`]) where the chain's `prev` links are absent.
///
/// The consent rules are hard and fail-closed: it belongs to `Restored`
/// blocks only, must verify against the member's ANCHORED key over
/// [`molt_core::chain::restore_consent_bytes`], and the member must not ALSO
/// appear in `sigs` (one member, one voice). Counting it is what lets an
/// m = n republic recover a seat (recovery approval design, 2026-08-08).
fn block_signers(
    republic_id: &str,
    identities: &[MemberIdentity],
    block: &ChainBlock,
) -> Result<BTreeSet<String>, String> {
    let bytes = approval_bytes(republic_id, block.height, &block.change);
    let mut signers = valid_signers(identities, &bytes, &block.sigs);
    if let ChainChange::Membership {
        op,
        member,
        nostr_pk,
        consent: Some(consent),
        ..
    } = &block.change
    {
        if *op != MembershipOp::Restored {
            return Err(format!(
                "block {} carries a consent on a non-restore membership change",
                block.height
            ));
        }
        if signers.contains(member) {
            return Err(format!(
                "block {} counts {member} twice — consent plus a roster signature",
                block.height
            ));
        }
        let anchored = identities
            .iter()
            .find(|i| i.member == *member)
            .map(|i| i.identity_pk.clone())
            .ok_or_else(|| format!("block {} restores an unknown member", block.height))?;
        let consent_bytes = molt_core::chain::restore_consent_bytes(
            republic_id,
            member,
            &anchored,
            nostr_pk.as_deref().unwrap_or(""),
        );
        if !molt_storage::identity_verify(&anchored, &consent_bytes, consent) {
            return Err(format!(
                "block {} carries a consent that does not verify for {member}",
                block.height
            ));
        }
        signers.insert(member.clone());
    }
    Ok(signers)
}

/// Verify one post-genesis block against the current head. Returns the
/// advanced head and — for a gated change — the proposal id the caller must
/// record as consumed, so a proposal cannot be committed twice.
///
/// **`seen_proposals` is READ-ONLY here, deliberately.** The walk this drives
/// is cached across calls ([`ChainWalk`]), so a block that fails any check
/// must leave the guard exactly as it was. An id recorded from a block that
/// was then rejected would make this holder refuse a proposal every other
/// node accepts — divergence, from bookkeeping alone.
fn verify_next(
    head: &ChainHead,
    block: &ChainBlock,
    seen_proposals: &BTreeSet<u64>,
) -> Result<(ChainHead, Option<u64>), String> {
    #[cfg(test)]
    VERIFY_STEPS.with(|c| c.set(c.get() + 1));
    if block.height != head.height + 1 {
        return Err(format!(
            "block height {} does not follow {}",
            block.height, head.height
        ));
    }
    if block.prev != head.hash {
        return Err(format!("block {} does not link to its predecessor", block.height));
    }
    let mut consumed = None;
    match &block.change {
        ChainChange::Genesis { .. } => {
            return Err("a genesis cannot appear after height 0".to_string());
        }
        ChainChange::Applied { proposal_id, .. } => {
            if seen_proposals.contains(proposal_id) {
                return Err(format!("proposal {proposal_id} is applied twice"));
            }
            consumed = Some(*proposal_id);
        }
        ChainChange::Membership { .. } => {}
        // structural checks only — the CONTENT check (recompute the
        // projection at `upto`, compare `state_hash`) runs in the chain
        // walkers, which hold the blocks/base needed to recompute
        ChainChange::Checkpoint { upto, .. } => {
            // EXACTLY the predecessor: a smaller upto would leave blocks in
            // (upto, height) that neither the blob nor a suffix carries —
            // their applied ids would escape the double-apply guard and
            // their membership changes the roster, forking full holders
            // from suffix holders. A re-based checkpoint proposal must
            // therefore re-cut (recompute state + hash at the new head).
            // upto == height - 1; height 0 is the genesis, never a checkpoint
            if block.height == 0 || *upto != block.height - 1 {
                return Err(format!(
                    "checkpoint upto {upto} must be exactly its block height {} minus one",
                    block.height
                ));
            }
        }
    }
    let signers = block_signers(&head.republic_id, &head.identities, block)?;
    if signers.len() < usize::from(head.rule_m) {
        return Err(format!(
            "block {} has {} valid approvals, threshold is {}",
            block.height,
            signers.len(),
            head.rule_m
        ));
    }
    let mut identities = head.identities.clone();
    if let ChainChange::Membership {
        op,
        member,
        identity_pk,
        ..
    } = &block.change
    {
        apply_membership(&mut identities, *op, member, identity_pk)?;
    }
    Ok((
        ChainHead {
            height: block.height,
            hash: block_hash(&head.republic_id, block),
            republic_id: head.republic_id.clone(),
            rule_m: head.rule_m,
            identities,
        },
        consumed,
    ))
}

/// Everything a chain verification accumulates as it folds — kept so a holder
/// can EXTEND a prefix it has already verified instead of re-walking it.
///
/// **There is exactly one step function.** [`walk_chain`],
/// [`walk_suffix_chain`] and the holder's incremental extension all drive
/// [`ChainWalk::step`], so "extend by one block" cannot drift from "verify
/// from the anchor" — they are the same code, not two implementations kept
/// in sync. Whole-chain verification stays mandatory wherever a chain arrives
/// from OUTSIDE this holder's own verified prefix (adoption, restore import,
/// checkpoint re-anchor, load from disk); the incremental path only ever
/// appends to a prefix this node walked itself. Nothing is skipped — it is
/// remembered.
pub(crate) struct ChainWalk {
    /// The verified head after every block folded so far.
    pub(crate) head: ChainHead,
    /// Applied proposal ids — the double-apply guard. On a pruned holder this
    /// is SEEDED FROM THE BLOB: the blocks carrying the pre-cut ids are gone,
    /// so the surviving chain alone can no longer answer the question.
    seen: BTreeSet<u64>,
    /// The projection a checkpoint's `state_hash` is checked against.
    running: molt_core::CheckpointState,
    /// A suffix holder's blob coverage. A checkpoint claiming less would
    /// leave blocks that neither the blob nor the suffix carries.
    floor: Option<u64>,
    /// How many blocks this walk covers, the seed block included — the
    /// holder's cheap check that a cached walk still describes its chain.
    folded: usize,
}

impl ChainWalk {
    /// Fold ONE block into the walk.
    ///
    /// **Atomic on failure**: every fallible check runs before any field is
    /// touched, so a refused block leaves the walk byte-identical. That is
    /// what makes a CACHED walk safe to reuse after a rejection — and
    /// `apply_membership`, the last thing that can fail, is check-then-mutate
    /// for the same reason.
    fn step(&mut self, block: &ChainBlock) -> Result<(), String> {
        let (head, consumed) = verify_next(&self.head, block, &self.seen)?;
        if let ChainChange::Checkpoint { upto, state_hash } = &block.change {
            if let Some(floor) = self.floor {
                if *upto < floor {
                    return Err(format!(
                        "checkpoint upto {upto} lies below the blob coverage {floor}"
                    ));
                }
            }
            // the running state IS the state at `upto` (upto == height - 1,
            // enforced in verify_next), so the content check needs no refold
            if &hash_walk_state(&self.running, *upto) != state_hash {
                return Err(format!(
                    "checkpoint at upto {upto} does not match this chain's own projection"
                ));
            }
        }
        fold_one(&mut self.running, block)?;
        if let Some(id) = consumed {
            self.seen.insert(id);
        }
        self.head = head;
        self.folded += 1;
        Ok(())
    }

    /// Does this walk still describe `chain` under `blob`? A cached walk is
    /// only ever USED when it does; a mismatch costs a re-walk, never a wrong
    /// accept. So a missed invalidation degrades performance, not safety.
    pub(crate) fn describes(
        &self,
        chain: &[ChainBlock],
        blob: Option<&molt_core::CheckpointState>,
    ) -> bool {
        self.folded == chain.len()
            && self.floor == blob.map(|b| b.upto)
            && chain
                .last()
                .is_some_and(|b| block_hash(&self.head.republic_id, b) == self.head.hash)
    }
}

/// WP4b: fold the checkpoint state a signer attests from a VERIFIED chain
/// (callers run [`verify_chain`] first — this fold trusts its input). Equal
/// chains yield equal states yield equal canonical bytes on every node —
/// which is exactly what makes the m-of-n checkpoint signature meaningful.
pub(crate) fn checkpoint_state(
    blocks: &[ChainBlock],
    upto: u64,
) -> Result<molt_core::CheckpointState, String> {
    let Some(ChainBlock {
        change:
            ChainChange::Genesis {
                name,
                republic_id,
                rule_m,
                rule_n,
                identities,
                agenda,
                relays,
                features,
            },
        ..
    }) = blocks.first()
    else {
        return Err("chain does not start with a genesis".to_string());
    };
    let base = genesis_base(
        name,
        *rule_m,
        *rule_n,
        identities,
        agenda,
        republic_id,
        relays,
        features.as_deref(),
    );
    fold_state(base, &blocks[1..], upto)
}

/// The empty state a genesis roots — the base every full-holder fold and
/// walk starts from.
#[allow(clippy::too_many_arguments)] // the genesis facts are one unit
fn genesis_base(
    name: &str,
    rule_m: u8,
    rule_n: u8,
    identities: &[MemberIdentity],
    agenda: &str,
    republic_id: &str,
    relays: &[String],
    features: Option<&[String]>,
) -> molt_core::CheckpointState {
    molt_core::CheckpointState {
        founding_name: name.to_string(),
        rule_m,
        rule_n,
        founding_identities: identities.to_vec(),
        agenda: agenda.to_string(),
        republic_id: republic_id.to_string(),
        relays: relays.to_vec(),
        founding_features: features.map(<[String]>::to_vec),
        roster: identities.to_vec(),
        applied: Surface::ALL.into_iter().map(|s| (s, Vec::new())).collect(),
        consumed_ids: Vec::new(),
        anchors: Vec::new(),
        member_relays: Vec::new(),
        upto: 0,
    }
}

/// Fold ONE verified block into a running walk state (4d: the walkers
/// carry the projection incrementally instead of refolding from the base
/// at every checkpoint — O(n) instead of O(n·checkpoints)). Checkpoint and
/// Genesis blocks are state-neutral; `consumed_ids` stays UNSORTED here
/// and is sorted per hash in [`hash_walk_state`].
fn fold_one(state: &mut molt_core::CheckpointState, block: &ChainBlock) -> Result<(), String> {
    match &block.change {
        ChainChange::Applied {
            proposal_id,
            surface,
            payload,
        } => {
            if let Some((_, list)) = state.applied.iter_mut().find(|(s, _)| s == surface) {
                // §B.6a (v4): a checkpoint SUMMARIZES. A last-write-wins slot
                // keeps only its latest entry — the answer `org_effective`
                // computes anyway — while a distinct object accumulates.
                // Applied HERE rather than at the cut so a suffix holder
                // folding onto a summarized blob reaches the same state as a
                // full holder folding from the genesis.
                if let Some(slot) = molt_core::applied_lww_slot(*surface, payload) {
                    list.retain(|(_, p)| {
                        molt_core::applied_lww_slot(*surface, p).as_deref() != Some(slot.as_str())
                    });
                }
                list.push((*proposal_id, payload.clone()));
            }
            // EVERY consumed id, including one whose payload the summary just
            // dropped: this is the double-apply guard, and a summarized-away
            // payload must never become a re-appliable proposal
            state.consumed_ids.push(*proposal_id);
        }
        ChainChange::Membership {
            op,
            member,
            identity_pk,
            nostr_pk,
            relays,
            // spent at the block's own verification; the projection carries
            // no per-block consent (nothing downstream re-checks it)
            consent: _,
        } => {
            apply_membership(&mut state.roster, *op, member, identity_pk)?;
            // v5: the re-anchor rides BESIDE the roster, because
            // `apply_membership` may not move a seat's anchored identity.
            // Carrying it here is what keeps a recovered seat addressable
            // after the block that re-anchored it is dropped at a cut.
            if let Some(pk) = nostr_pk.as_ref().filter(|p| !p.is_empty()) {
                match state.anchors.binary_search_by(|(m, _)| m.as_str().cmp(member)) {
                    Ok(i) => state.anchors[i].1 = pk.clone(),
                    Err(i) => state.anchors.insert(i, (member.clone(), pk.clone())),
                }
            }
            // v6: the relay ledger rides beside it, for the same
            // survive-the-cut reason (R3b)
            if !relays.is_empty() {
                match state.member_relays.binary_search_by(|(m, _)| m.as_str().cmp(member)) {
                    Ok(i) => state.member_relays[i].1 = relays.clone(),
                    Err(i) => state.member_relays.insert(i, (member.clone(), relays.clone())),
                }
            }
        }
        ChainChange::Genesis { .. } | ChainChange::Checkpoint { .. } => {}
    }
    Ok(())
}

/// Hash the running walk state as the canonical state at `upto` — the
/// comparison a checkpoint's `state_hash` must match. Clones once to sort
/// the consumed ids (canonical layout) without disturbing the walk.
fn hash_walk_state(state: &molt_core::CheckpointState, upto: u64) -> String {
    let mut at = state.clone();
    at.upto = upto;
    at.consumed_ids.sort_unstable();
    checkpoint_state_hash(&at)
}

/// Fold further verified blocks (heights `<= upto`) onto a base state —
/// the SAME fold whether the base is the genesis (full holder) or a
/// checkpoint blob (suffix holder), so chained checkpoints recompute
/// identically on both. Checkpoint/Genesis blocks in the range are
/// state-neutral.
///
/// Delegates every block to [`fold_one`] rather than repeating the match.
/// The duplication was a live divergence trap: the batch fold and the
/// incremental walk hash the SAME state, so any rule that reached one and
/// not the other (the v4 summary is exactly such a rule) would make a
/// republic unable to agree on a cut, with no error pointing at why.
fn fold_state(
    mut state: molt_core::CheckpointState,
    blocks: &[ChainBlock],
    upto: u64,
) -> Result<molt_core::CheckpointState, String> {
    for b in blocks {
        if b.height > upto {
            break;
        }
        fold_one(&mut state, b)?;
    }
    state.consumed_ids.sort_unstable();
    state.upto = upto;
    Ok(state)
}

/// WP4b 4c: the wire shape a recovery coordinator serves its chain in —
/// the Welcome twin of storage's `ChainStateFile`. Untagged: an array is
/// a genesis-rooted chain (the historical shape, old rejoiners keep
/// working against full coordinators), an object carries the checkpoint
/// blob a PRUNED coordinator anchors on.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
// same reason as `ChainStateFile`: the pruned arm carries a founding summary,
// and this is a WIRE shape — boxing changes what a peer parses.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ServedChainWire {
    Pruned {
        checkpoint_blob: molt_core::CheckpointState,
        blocks: Vec<ChainBlock>,
    },
    Full(Vec<ChainBlock>),
}

/// The lowercase-hex SHA-256 a checkpoint's `state_hash` carries — over
/// [`molt_core::checkpoint_canonical_bytes`], hashed like every other
/// chain artifact (`molt_storage::content_hash`).
pub(crate) fn checkpoint_state_hash(state: &molt_core::CheckpointState) -> String {
    molt_storage::content_hash(&molt_core::checkpoint_canonical_bytes(state))
}

/// Verify a whole chain from its genesis and return its head. Any failure is
/// hard: the chain is rejected in full (a partially-valid chain is not a thing
/// — a rejoiner that trusted a prefix could fork the republic's state).
pub fn verify_chain(blocks: &[ChainBlock]) -> Result<ChainHead, String> {
    Ok(walk_chain(blocks)?.head)
}

/// What a verified wiki export proved — the facts a reviewer reads off the
/// bundle, none of them taken on trust from the exporter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiExportReport {
    /// The content-derived republic id, re-derived from the genesis roster.
    pub republic_id: String,
    /// The republic's founding display name.
    pub name: String,
    /// Approval threshold m every exported patch had to reach.
    pub rule_m: u8,
    /// Founding member count n (the genesis is n-of-n).
    pub rule_n: u8,
    /// The roster after the exported membership blocks, in chain order.
    pub members: Vec<String>,
    /// Membership blocks walked (the identity history).
    pub membership_blocks: usize,
    /// Wiki patches verified. A patch whose hunks no longer applied is VOID
    /// in the fold — it counts here but moves no file, exactly as in the
    /// republic's own state.
    pub patches: usize,
    /// Files in the verified tree.
    pub files: usize,
}

/// **Verify a wiki export against the tree it shipped** (`docs/memory/
/// wiki_export_plan.md`): the outsider's check — no moltd, no workspace key,
/// no trust in the exporter.
///
/// The bundle is a SUBSET of the chain, so `prev` links and contiguous
/// heights are gone by construction; what carries the proof is that every
/// member signature is position-bound ([`approval_bytes`] folds `height` in),
/// so each block stands on its own against the roster valid at its height.
///
/// Four steps, all hard-reject:
/// 1. the genesis seals n-of-n and its `republic_id` re-derives from its own
///    roster content ([`verify_genesis`]),
/// 2. the roster walk applies every `Membership` block in height order, each
///    one itself threshold-signed,
/// 3. every wiki patch carries ≥ m distinct valid signatures against the
///    roster valid at ITS height, heights ascend, no proposal applies twice,
/// 4. [`molt_core::wiki_fold::wiki_fold`] over the patch payloads equals
///    `tree` exactly.
///
/// **What it does NOT prove: completeness.** An exporter can omit trailing
/// patches and ship an older, still genuinely approved state. Freshness needs
/// a second export or another member's copy (`proof/README.md` says so).
pub fn verify_wiki_export(
    bundle_json: &str,
    tree: &std::collections::BTreeMap<String, String>,
) -> Result<WikiExportReport, String> {
    let bundle: crate::wiki_export::WikiExportBundle =
        serde_json::from_str(bundle_json).map_err(|e| format!("bundle: {e}"))?;
    if bundle.format != crate::wiki_export::WIKI_EXPORT_FORMAT {
        return Err(format!(
            "bundle format {} is not {}",
            bundle.format,
            crate::wiki_export::WIKI_EXPORT_FORMAT
        ));
    }
    // 1. the genesis seal (n-of-n over the roster bytes + the id re-derivation)
    let head = verify_genesis(&bundle.genesis).map_err(|e| format!("genesis: {e}"))?;
    let ChainChange::Genesis { name, rule_n, .. } = &bundle.genesis.change else {
        return Err("genesis: block 0 is not a genesis".to_string());
    };

    // 2 + 3. the roster walk and the per-patch threshold check, one pass
    let mut identities = head.identities.clone();
    let mut membership_blocks = 0usize;
    let mut payloads: Vec<serde_json::Value> = Vec::new();
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    let mut last_height = 0u64;
    for block in &bundle.blocks {
        if block.height <= last_height {
            return Err(format!("block {}: heights must ascend", block.height));
        }
        last_height = block.height;
        let membership = matches!(block.change, ChainChange::Membership { .. });
        if !membership && !crate::wiki_export::is_wiki_patch(&block.change) {
            return Err(format!(
                "block {}: only membership blocks and wiki patches are exported",
                block.height
            ));
        }
        let signers = block_signers(&head.republic_id, &identities, block)?;
        if signers.len() < usize::from(head.rule_m) {
            return Err(format!(
                "block {}: {} valid approvals, threshold is {}",
                block.height,
                signers.len(),
                head.rule_m
            ));
        }
        match &block.change {
            ChainChange::Membership {
                op,
                member,
                identity_pk,
                ..
            } => {
                apply_membership(&mut identities, *op, member, identity_pk)
                    .map_err(|e| format!("block {}: {e}", block.height))?;
                membership_blocks += 1;
            }
            ChainChange::Applied {
                proposal_id,
                payload,
                ..
            } => {
                if !seen.insert(*proposal_id) {
                    return Err(format!(
                        "block {}: proposal {proposal_id} is applied twice",
                        block.height
                    ));
                }
                payloads.push(payload.clone());
            }
            _ => unreachable!("admission above accepts only membership and applied blocks"),
        }
    }

    // 4. the fold IS the tree — same function the republic runs on its own state
    let folded = molt_core::wiki_fold::wiki_fold(&payloads);
    for (path, content) in &folded {
        match tree.get(path) {
            None => return Err(format!("tree: {path} is missing from the export")),
            Some(shipped) if shipped != content => {
                return Err(format!("tree: {path} differs from the folded patches"))
            }
            Some(_) => {}
        }
    }
    if let Some(extra) = tree.keys().find(|p| !folded.contains_key(*p)) {
        return Err(format!("tree: {extra} is not in the folded patches"));
    }

    Ok(WikiExportReport {
        republic_id: head.republic_id,
        name: name.clone(),
        rule_m: head.rule_m,
        rule_n: *rule_n,
        members: identities.into_iter().map(|i| i.member).collect(),
        membership_blocks,
        patches: payloads.len(),
        files: tree.len(),
    })
}

/// [`verify_chain`], keeping the walk it built — what a holder caches so the
/// NEXT block costs one block's signatures instead of another full re-walk.
pub(crate) fn walk_chain(blocks: &[ChainBlock]) -> Result<ChainWalk, String> {
    let Some((genesis, rest)) = blocks.split_first() else {
        return Err("empty chain".to_string());
    };
    let head = verify_genesis(genesis)?;
    let ChainChange::Genesis {
        name,
        republic_id,
        rule_m,
        rule_n,
        identities,
        agenda,
        relays,
        features,
    } = &genesis.change
    else {
        unreachable!("verify_genesis accepted a non-genesis block 0");
    };
    // 4d: the walk carries the projection incrementally — a full holder
    // accepts no checkpoint summary it cannot recompute from its own chain
    let mut walk = ChainWalk {
        head,
        seen: BTreeSet::new(),
        running: genesis_base(
            name,
            *rule_m,
            *rule_n,
            identities,
            agenda,
            republic_id,
            relays,
            features.as_deref(),
        ),
        floor: None,
        folded: 1,
    };
    for block in rest {
        walk.step(block)?;
    }
    Ok(walk)
}

/// Verify a SERVED chain — the shared front door of the recovery adoption
/// (`cmd_net_recover_sealed`) and the restore import
/// (`cmd_net_restore_staged`): full chains from block 0 via
/// [`verify_chain`], pruned ones against their checkpoint blob via
/// [`verify_suffix_chain`]. `expected_rid` is the caller's EXTERNAL anchor
/// when it has one (the recovery link); `None` anchors on the
/// content-derived id the genesis/blob founding table itself recomputes —
/// the same trust model a full-chain import has. Hard-reject,
/// all-or-nothing; returns the verified head plus the founding
/// constitution the workspace materializes from.
/// The chain-ratified relay pool as SERVED MATERIAL folds it: the blob's
/// (else the genesis') pool plus every applied `set_relays` edit in the
/// verified run — the recovery gate's authority. A Welcome minted after a
/// pool vote carries the GOVERNED pool, so comparing it against the
/// founding pool refuses every recovery on a republic that ever voted its
/// relays (field find 2026-08-17). Same fold as [`State::effective_relays`],
/// for a caller that holds served blocks instead of a projection.
pub(crate) fn effective_relays_of_served(
    checkpoint: Option<&molt_core::CheckpointState>,
    blocks: &[ChainBlock],
) -> Vec<String> {
    let mut pool = match checkpoint {
        Some(blob) => blob.relays.clone(),
        None => blocks
            .first()
            .and_then(|b| match &b.change {
                ChainChange::Genesis { relays, .. } => Some(relays.clone()),
                _ => None,
            })
            .unwrap_or_default(),
    };
    for b in blocks {
        if let ChainChange::Applied { payload, .. } = &b.change {
            if payload.get("op").and_then(serde_json::Value::as_str) == Some("set_relays") {
                State::fold_pool_edit(
                    &mut pool,
                    payload.get("value").and_then(serde_json::Value::as_str).unwrap_or_default(),
                );
            }
        }
    }
    pool
}

pub(crate) fn verify_served(
    checkpoint: Option<&molt_core::CheckpointState>,
    blocks: &[ChainBlock],
    expected_rid: Option<&str>,
) -> Result<(ChainHead, molt_core::SealedRoster), String> {
    match checkpoint {
        None => {
            let head = verify_chain(blocks)?;
            let sealed = blocks
                .first()
                .and_then(crate::recovery::sealed_roster_from_genesis)
                .ok_or_else(|| {
                    "the chain does not root on a genesis constitution".to_string()
                })?;
            if let Some(rid) = expected_rid {
                if head.republic_id != rid {
                    return Err("the chain does not match the expected republic".to_string());
                }
            }
            Ok((head, sealed))
        }
        Some(blob) => {
            // the suffix rules recompute the blob's founding table to the
            // rid either way; an external anchor additionally pins WHICH
            // republic the caller expects
            let rid_owned;
            let rid = match expected_rid {
                Some(r) => r,
                None => {
                    rid_owned = blob.republic_id.clone();
                    rid_owned.as_str()
                }
            };
            let head = verify_suffix_chain(blob, blocks, rid)?;
            Ok((head, crate::recovery::sealed_roster_from_blob(blob)))
        }
    }
}

/// WP4b: verify a SUFFIX chain — one that begins with a checkpoint block
/// instead of the genesis (`docs_archive/chain/log_compaction.md` §B.5). The
/// checkpoint is the trust anchor: its blob must hash to the signed
/// `state_hash`, its founding table must RECOMPUTE to the expected
/// republic id (the genesis forgery check without the genesis), and the
/// anchor signatures must reach m over the blob's CURRENT roster. The
/// suffix then verifies exactly like any chain, with the double-apply
/// guard seeded from the blob's consumed ids. All-or-nothing, like
/// [`verify_chain`]. Trust model (documented, deliberate): the roster
/// evolution below `upto` is attested by m-of-n instead of replayed —
/// the same honest-majority assumption threshold governance stands on.
pub(crate) fn verify_suffix_chain(
    blob: &molt_core::CheckpointState,
    blocks: &[ChainBlock],
    expected_republic_id: &str,
) -> Result<ChainHead, String> {
    Ok(walk_suffix_chain(blob, blocks, expected_republic_id)?.head)
}

/// [`verify_suffix_chain`], keeping the walk — the pruned holder's twin of
/// [`walk_chain`].
pub(crate) fn walk_suffix_chain(
    blob: &molt_core::CheckpointState,
    blocks: &[ChainBlock],
    expected_republic_id: &str,
) -> Result<ChainWalk, String> {
    let Some((anchor, rest)) = blocks.split_first() else {
        return Err("empty suffix chain".to_string());
    };
    let ChainChange::Checkpoint { upto, state_hash } = &anchor.change else {
        return Err("suffix chain does not start with a checkpoint".to_string());
    };
    if blob.upto != *upto {
        return Err("checkpoint blob does not cover the anchored upto".to_string());
    }
    if &checkpoint_state_hash(blob) != state_hash {
        return Err("checkpoint blob does not match the signed state hash".to_string());
    }
    // a checkpoint anchor can never be the genesis (height 0); the checked
    // subtraction also rejects an attacker-served height-0 anchor instead
    // of underflowing (overflow-checks=true → abort)
    if anchor.height.checked_sub(1) != Some(*upto) {
        return Err(
            "checkpoint upto must be exactly the anchor height minus one".to_string(),
        );
    }
    // founding recomputation — forging the founding changes the id
    let rid = molt_storage::republic_id(
        &blob.founding_name,
        blob.rule_m,
        blob.rule_n,
        &blob.founding_identities,
    );
    if rid != expected_republic_id || rid != blob.republic_id {
        return Err(
            "checkpoint founding table does not recompute to the republic id".to_string(),
        );
    }
    if blob.rule_m == 0 || blob.rule_m > blob.rule_n || blob.roster.is_empty() {
        return Err("checkpoint roster/threshold out of range".to_string());
    }
    // the same structural size check verify_genesis runs on the full path: a
    // founding table larger than rule_n would graft attacker-owned "founding"
    // keys into the valid_signers set below (the rid recompute pins the
    // table's CONTENT, not that its size matches the sealed n)
    if usize::from(blob.rule_n) != blob.founding_identities.len() {
        return Err("checkpoint founding table size does not match n".to_string());
    }
    // NO circular trust: the blob's roster is only bound by the state hash
    // the anchor sigs attest — so the roster itself must chain back to the
    // rid-bound FOUNDING table, and the anchor signatures must verify
    // against founding keys. Seats are fixed at founding (product decision)
    // and a Restored re-key keeps the anchored identity, so every roster
    // entry must literally appear in the founding table — ALL THREE anchors,
    // the nostr transport anchor included (a re-signed blob that keeps
    // member+identity_pk but swaps the third anchor would otherwise
    // redirect that seat's future gift-wrapped material); forging an anchor
    // therefore needs m REAL founding keys — the honest-majority assumption
    // threshold governance already stands on, not less.
    for entry in &blob.roster {
        if !blob.founding_identities.iter().any(|f| {
            f.member == entry.member
                && f.identity_pk == entry.identity_pk
                && f.nostr_pk == entry.nostr_pk
        }) {
            return Err(format!(
                "checkpoint roster member {} is not anchored in the founding table",
                entry.member
            ));
        }
    }
    let bytes = approval_bytes(&blob.republic_id, anchor.height, &anchor.change);
    let signers = valid_signers(&blob.founding_identities, &bytes, &anchor.sigs);
    if signers.len() < usize::from(blob.rule_m) {
        return Err(format!(
            "checkpoint anchor has {} valid approvals, threshold is {}",
            signers.len(),
            blob.rule_m
        ));
    }
    // the walk state, seeded from the blob (the anchor block itself is
    // state-neutral). `seen` MUST come from the blob's consumed ids: the
    // blocks carrying the pre-cut proposal ids are gone, so the surviving
    // suffix cannot answer the double-apply question on its own.
    let mut walk = ChainWalk {
        head: ChainHead {
            height: anchor.height,
            hash: block_hash(&blob.republic_id, anchor),
            republic_id: blob.republic_id.clone(),
            rule_m: blob.rule_m,
            identities: blob.roster.clone(),
        },
        seen: blob.consumed_ids.iter().copied().collect(),
        running: blob.clone(),
        floor: Some(blob.upto),
        folded: 1,
    };
    for block in rest {
        walk.step(block)?;
    }
    Ok(walk)
}

impl State {
    /// Build block 0 of the persistent chain from a sealed roster — but only
    /// for a **real** founding (a content-derived republic id and one
    /// attestation per member). A pre-ritual/demo materialize gets no chain
    /// (empty) and stays on the honest single-operator path.
    pub(crate) fn genesis_chain(&self, sealed: &SealedRoster) -> Vec<ChainBlock> {
        if sealed.republic_id.is_empty()
            || sealed.identities.is_empty()
            || sealed.attestations.len() != sealed.identities.len()
        {
            return Vec::new();
        }
        vec![ChainBlock {
            height: 0,
            prev: GENESIS_PREV.to_string(),
            change: ChainChange::Genesis {
                name: sealed.name.clone(),
                republic_id: sealed.republic_id.clone(),
                rule_m: sealed.rule_m,
                rule_n: sealed.rule_n,
                identities: sealed.identities.clone(),
                agenda: sealed.agenda.clone(),
                // the pool the founders SIGNED — genesis approval_bytes is
                // roster_canonical_bytes, so this must be exactly what the
                // attestations below were made over
                relays: sealed.relays.clone(),
                // same rule for the ratified feature set (roster-v5):
                // presence decides the tag the verifier recomputes under
                features: sealed.features.clone(),
            },
            sigs: sealed.attestations.clone(),
        }]
    }

    /// Verify a freshly-loaded or freshly-built chain and adopt it as the open
    /// workspace's chain + head, then re-project the persistent state from it.
    /// A chain that fails verification is **hard-rejected**: the head stays
    /// `None` and nothing is projected (a partially-trusted chain could fork
    /// state — `docs_archive/chain/persistent_chain.md`).
    pub(crate) fn adopt_chain(&mut self, chain: Vec<ChainBlock>) {
        // a chain from OUTSIDE this holder's own verified prefix: always the
        // full walk, never the cache. The walk it produces is then kept, so
        // the adoption pays for the next append too.
        match self.walk_own(&chain) {
            Ok(walk) => {
                self.chain = chain;
                self.chain_head = Some(walk.head.clone());
                self.chain_walk = Some(walk);
                self.bump_next_id_past_chain();
                self.apply_chain_to_state();
            }
            Err(e) => {
                tracing::warn!(error = %e, "rejecting an unverifiable chain");
                self.chain.clear();
                self.chain_head = None;
                self.chain_walk = None;
                self.set_checkpoint_blob(None);
            }
        }
    }

    /// The mint counter must stay AHEAD of every proposal id the verified
    /// chain has consumed: `receive_proposed` (and its membership twin)
    /// refuses an already-consumed id on every peer, so a locally minted
    /// collision could never seal — a silent liveness hole for any holder
    /// that adopted its chain without the ephemeral event log to bump
    /// `next_id` for it (a blob-seeded rejoiner after total loss). Called
    /// wherever the walk adopts or extends; `max` keeps it monotone.
    fn bump_next_id_past_chain(&mut self) {
        if let Some(top) = self
            .chain_walk
            .as_ref()
            .and_then(|w| w.seen.iter().next_back())
        {
            self.next_id = self.next_id.max(top.saturating_add(1));
        }
    }

    /// Set (or clear) the checkpoint anchor. **The one way to do it** — the
    /// cached walk is seeded from the blob (`seen` from its consumed ids,
    /// `running` from its state), so a blob swap must invalidate it. The
    /// chain-shape backstop in [`ChainWalk::describes`] cannot see a blob
    /// replaced at the same coverage, which is why this is a setter and not
    /// a comment asking callers to remember.
    pub(crate) fn set_checkpoint_blob(&mut self, blob: Option<molt_core::CheckpointState>) {
        self.checkpoint_blob = blob;
        self.chain_walk = None;
    }

    /// Re-project the persistent state from the whole chain: the gated
    /// surfaces' applied logs (into the chain-owned [`State::chain_applied`], a
    /// full clear-and-refold so a re-base is free) and the roster/identities
    /// (taken from the already-verified head, which evolved them across the
    /// membership blocks). Chat, [`State::applied`] and pending proposals are
    /// left untouched — they are ephemeral or legacy-owned.
    /// The transport anchor to ADDRESS this member at right now: the seat's
    /// re-anchored key if a `Restored` block gave it one, else the immutable
    /// founding anchor from the roster. Empty for an unknown member — never
    /// somebody else's key.
    ///
    /// Every gift-wrap send resolves through this. Reaching for
    /// `identities[i].nostr_pk` directly addresses a key a recovered member
    /// no longer holds, and the send simply vanishes.
    // No production caller YET: every gift-wrap send today addresses a RITUAL
    // SEAT during a founding, where no recovered seat can exist. The consumers
    // arrive with N4b step 6 (the rejoiner/coordinator legs) and N5 (the
    // runtime), and each MUST resolve through here. Pinned by
    // `the_working_anchor_follows_a_restored_block_while_the_roster_does_not`
    // so the projection cannot rot before its callers land.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn working_nostr_pk(&self, member: &str) -> String {
        if let Some(pk) = self.chain_anchors.get(member) {
            return pk.clone();
        }
        self.replica
            .as_ref()
            .and_then(|r| r.identities.iter().find(|i| i.member == member))
            .map(|i| i.nostr_pk.clone())
            .unwrap_or_default()
    }

    /// The relays `member` is on record as reaching (R3b, the ledger): its
    /// declared pool if a `Membership` block carried one, else the ratified
    /// GROUP pool — a founding member never declared anything because the
    /// genesis pool it co-signed covers it. The split-detection input (R4).
    pub(crate) fn member_relays(&self, member: &str) -> Vec<String> {
        if let Some(declared) = self.chain_member_relays.get(member) {
            return declared.clone();
        }
        self.effective_relays()
    }

    /// R4 — split detection: every pair of roster members whose EFFECTIVE
    /// relay sets do not intersect, `(a, b)` in roster order. Such a pair
    /// can never exchange a frame no matter how healthy each side's own
    /// relay is, so the republic's threshold may silently be unable to
    /// assemble — a named state, never a silence. Computable by every
    /// member from the same chain (the ledger, R3b).
    pub(crate) fn relay_splits(&self) -> Vec<(molt_core::MemberId, molt_core::MemberId)> {
        let roster = self.roster();
        let mut out = Vec::new();
        for i in 0..roster.len() {
            for j in i + 1..roster.len() {
                let a = self.member_relays(&roster[i]);
                let b = self.member_relays(&roster[j]);
                // no data is no verdict: a non-Nostr chain has no pools, and
                // an empty side must not read as "split from everyone"
                if a.is_empty() || b.is_empty() {
                    continue;
                }
                if a.iter().any(|r| b.contains(r)) {
                    continue;
                }
                out.push((roster[i].clone(), roster[j].clone()));
            }
        }
        out
    }

    /// R4: log every split pair ONCE (structured, greppable) — the run-log
    /// half of the verdict; the members surface carries the per-member
    /// marker. Rides every chain adoption/append.
    pub(crate) fn note_relay_splits(&mut self) {
        for (a, b) in self.relay_splits() {
            if self.split_noted.insert((a.clone(), b.clone())) {
                let bridge = self.member_relays(&a).first().cloned().unwrap_or_default();
                tracing::warn!(%a, %b, %bridge, "relay split - no shared relay");
            }
        }
    }

    /// The EFFECTIVE group pool (R6): the latest applied `set_relays` edit,
    /// else the ratified founding pool. This is the answer every reader
    /// wants — the pool as governed, not as founded.
    pub(crate) fn effective_relays(&self) -> Vec<String> {
        let mut pool = self.ratified_relays();
        for v in self.applied_org_entries() {
            if v.get("op").and_then(serde_json::Value::as_str) == Some("set_relays") {
                Self::fold_pool_edit(
                    &mut pool,
                    v.get("value").and_then(serde_json::Value::as_str).unwrap_or_default(),
                );
            }
        }
        pool
    }

    /// The EFFECTIVE feature set (`charter_features.md` D5): the ratified
    /// founding baseline unioned with every applied `set_features` edit,
    /// sorted + deduped.
    ///
    /// **The fold is a UNION on purpose** — the deterministic twin of the
    /// propose-time enable-only gate: a block that tried to drop a feature
    /// folds as pure addition on every holder, so "features can never be
    /// switched off" is a construction property, not a courtesy (the
    /// `fold_pool_edit` lesson). Unknown keys are kept — an older build
    /// must not un-enable what a newer one ratified; readers ignore keys
    /// they cannot render.
    ///
    /// The baseline (D6, user-decided 2026-08-11): a republic founded
    /// before roster-v5 (`features: None`) keeps exactly what was usable
    /// before the gating existed — Shared Memory.
    pub(crate) fn effective_features(&self) -> Vec<String> {
        let mut set: std::collections::BTreeSet<String> =
            match self.replica.as_ref().and_then(|r| r.features.clone()) {
                Some(f) => f.into_iter().collect(),
                None => molt_core::Surface::LEGACY_FEATURES
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            };
        for v in self.applied_org_entries() {
            if v.get("op").and_then(serde_json::Value::as_str) == Some("set_features") {
                let value =
                    v.get("value").and_then(serde_json::Value::as_str).unwrap_or_default();
                set.extend(value.split_whitespace().map(str::to_string));
            }
        }
        set.into_iter().collect()
    }

    /// The D7 gate: refuse an optional surface the charter has not enabled.
    /// The nav HIDES such a surface; this is the engine-side twin an MCP
    /// agent meets (co-equality — clickable or refused must be one verdict).
    pub(crate) fn require_feature(&self, surface: Surface) -> Result<(), molt_core::MoltError> {
        if surface.is_charter_feature()
            && !self.effective_features().iter().any(|f| f == surface.as_str())
        {
            return Err(molt_core::MoltError::FeatureDisabled(surface.as_str()));
        }
        Ok(())
    }

    /// The R6 fold rule both effective views share (`effective_relays` and
    /// `org_effective`): an applied `set_relays` entry replaces the pool
    /// only if it is non-empty AND shares a relay with the pool accumulated
    /// so far — make-before-break at the FOLD, the only place every holder
    /// passes deterministically. The propose-time gates are local courtesy;
    /// a peer on another build (or a hand-crafted payload) bypasses them,
    /// and two individually-legal pending edits can compose into a
    /// zero-overlap transition (review 2026-08-09). A zero-overlap
    /// transition applied for real would tear the republic at that commit,
    /// so it deterministically becomes a no-op instead.
    pub(crate) fn fold_pool_edit(pool: &mut Vec<String>, value: &str) {
        let parsed: Vec<String> = value.split_whitespace().map(str::to_string).collect();
        if parsed.is_empty() {
            return;
        }
        if !pool.is_empty() && !pool.iter().any(|r| parsed.contains(r)) {
            return;
        }
        *pool = parsed;
    }

    /// R6: the governed pool moved — carry it into the LIVE transport. The
    /// runtime rebuild is the accepted whole-group blip (Track C option A,
    /// 2026-07-23); the ratchet is handed over as the SHARED Arc, exactly
    /// like the mesh-extension rebuild, so no sender generation is reused.
    /// A workspace without a live runtime just adopts the list.
    pub(crate) fn adopt_pool_change(&mut self) {
        let pool = self.effective_relays();
        let Some(nostr) = self.nostr.as_mut() else {
            return;
        };
        if pool.is_empty() || nostr.relays == pool {
            return;
        }
        nostr.relays = pool;
        if let Some(old) = self.group_net.take() {
            tracing::info!(
                relays = self.nostr.as_ref().map_or(0, |n| n.relays.len()),
                "the governed relay pool moved - rebuilding the group runtime"
            );
            let mls = old.mls.clone();
            // dropping the handle latches the stop (watch, not Notify) —
            // the old outbox ends at its next poll
            drop(old);
            self.group_net = self.build_group_net_shared(mls);
        }
    }

    /// The ratified GROUP pool: the checkpoint summary's if this holder
    /// pruned, else the genesis block's. Empty on a non-Nostr chain.
    pub(crate) fn ratified_relays(&self) -> Vec<String> {
        if let Some(blob) = &self.checkpoint_blob {
            return blob.relays.clone();
        }
        self.chain
            .first()
            .and_then(|b| match &b.change {
                ChainChange::Genesis { relays, .. } => Some(relays.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Fold ONE freshly-appended block into the projection.
    ///
    /// [`State::apply_chain_to_state`] rebuilds from the whole chain, which is
    /// right when entries must DISAPPEAR (a re-base, a prune) and wrong for an
    /// append: it re-clones every payload in the chain for every block, so a
    /// catch-up draining N blocks cloned the applied log N²/2 times.
    ///
    /// An append can only add, so this runs the same three folds for the one
    /// block — the projection it produces is the one the full rebuild would.
    fn project_one(&mut self, block: &ChainBlock) {
        match &block.change {
            ChainChange::Applied {
                proposal_id,
                surface,
                payload,
            } => {
                self.chain_applied
                    .entry(*surface)
                    .or_default()
                    .push((Some(*proposal_id), payload.clone()));
                self.chain_applied_sigs
                    .insert(*proposal_id, block.sigs.clone());
                // R6: a committed pool edit reaches the live transport
                if payload.get("op").and_then(serde_json::Value::as_str) == Some("set_relays") {
                    self.adopt_pool_change();
                }
                // the Memory base moved — the supersede walk retires
                // pending wiki patches it left behind (deterministic:
                // this runs on append, catch-up and rebuild alike)
                if *surface == Surface::Memory {
                    self.supersede_stale_wiki();
                }
            }
            // the LAST Restored block for a seat wins, and an append is the
            // last; an empty anchor leaves the previous one standing, exactly
            // as the full rebuild's insert-only fold does
            ChainChange::Membership {
                member,
                nostr_pk,
                relays,
                ..
            } => {
                if let Some(pk) = nostr_pk.as_ref().filter(|p| !p.is_empty()) {
                    self.chain_anchors.insert(member.clone(), pk.clone());
                }
                // R3b: the relay ledger follows the same last-wins fold
                if !relays.is_empty() {
                    self.chain_member_relays.insert(member.clone(), relays.clone());
                    self.note_relay_splits();
                }
            }
            _ => {}
        }
        // the verified head carries the roster after every membership block
        if let Some(head) = &self.chain_head {
            if let Some(r) = &mut self.replica {
                r.identities = head.identities.clone();
                r.roster = head.identities.iter().map(|i| i.member.clone()).collect();
            }
        }
    }

    /// Settle the gossip-built proposal cards against the verified chain:
    /// every proposal a block (or the checkpoint blob below a cut) consumed
    /// shows Applied, every sealed membership change settles its
    /// content-matched cards. Idempotent — the re-base/prune rebuilds run it
    /// harmlessly; the reopen order makes it load-bearing. Deliberation is
    /// ephemeral: after a replay only chain truth remains, so a card the
    /// chain consumed can only honestly read Applied.
    /// The chain is the durable record; the `Proposed` gossip is ephemeral
    /// RAM on every RECEIVER (only the proposer's own log carries it). A
    /// holder that adopts an Applied block without the card — a reopen, a
    /// catch-up past lost gossip — materializes the record FROM the block,
    /// so the Accepted view keeps its id, title, patch shape and (via the
    /// sealed sigs, resolved in `view`) its voters. The proposer stays
    /// unattributed: the block does not record it, and inventing one would
    /// be a forgery.
    fn ensure_applied_record(
        &mut self,
        proposal_id: u64,
        surface: Surface,
        payload: serde_json::Value,
    ) {
        self.proposals
            .entry(proposal_id)
            .or_insert_with(|| molt_core::ProposalRecord {
                surface,
                payload,
                approvals: 0,
                state: ProposalState::Applied,
                declined_at: 0,
                declined_by: molt_core::MemberId::new(),
                decliners: Vec::new(),
                voted: Vec::new(),
                by: molt_core::MemberId::new(),
                superseded: false,
                withdrawn: false,
            });
    }

    fn settle_cards_against_chain(&mut self) {
        // ONE pass over blob + chain: records missing entirely (this holder
        // never was the proposer and its ephemeral gossip is gone) come
        // back from the durable evidence — the blob's summarized applied
        // payloads below the cut (their voter pills stay open: only
        // chain-provable votes are shown, the sigs went with the cut, and
        // without this a pruned and an unpruned holder of the SAME republic
        // showed different Accepted tables) and the live blocks above it.
        // Replay-resurrected open cards settle to Applied.
        let mut materialize: Vec<(u64, Surface, serde_json::Value)> = Vec::new();
        let mut settle: Vec<(u64, Surface)> = Vec::new();
        if let Some(blob) = &self.checkpoint_blob {
            for (surface, entries) in &blob.applied {
                for (id, payload) in entries {
                    match self.proposals.get(id) {
                        None => materialize.push((*id, *surface, payload.clone())),
                        Some(p) if p.state != ProposalState::Applied => {
                            settle.push((*id, *surface));
                        }
                        _ => {}
                    }
                }
            }
            // consumed ids whose payload the summary dropped (LWW slots):
            // no card to materialize, but a surviving open card still
            // settles — the id was decided
            for id in &blob.consumed_ids {
                if let Some(p) = self.proposals.get(id) {
                    if p.state != ProposalState::Applied {
                        settle.push((*id, p.surface));
                    }
                }
            }
        }
        for block in &self.chain {
            if let ChainChange::Applied {
                proposal_id,
                surface,
                payload,
            } = &block.change
            {
                match self.proposals.get(proposal_id) {
                    None => materialize.push((*proposal_id, *surface, payload.clone())),
                    Some(p) if p.state != ProposalState::Applied => {
                        settle.push((*proposal_id, *surface));
                    }
                    _ => {}
                }
            }
        }
        for (id, surface, payload) in materialize {
            self.ensure_applied_record(id, surface, payload);
        }
        for (id, surface) in settle {
            if let Some(p) = self.proposals.get_mut(&id) {
                p.state = ProposalState::Applied;
            }
            self.stash_voted(id);
            self.pending_sigs.remove(&id);
            self.proposal_changes.remove(&id);
            self.emit(Event::Applied {
                id: ProposalId(id),
                surface,
            });
        }
        // membership blocks carry no proposal id — settle by content, the
        // `after_block_applied` pattern
        let membership: Vec<ChainChange> = self
            .chain
            .iter()
            .filter(|b| matches!(b.change, ChainChange::Membership { .. }))
            .map(|b| b.change.clone())
            .collect();
        for change in membership {
            self.settle_membership_records(&change);
        }
    }

    pub(crate) fn apply_chain_to_state(&mut self) {
        let mut projected: std::collections::HashMap<
            Surface,
            Vec<(Option<u64>, serde_json::Value)>,
        > = std::collections::HashMap::new();
        // WP4b: a pruned holder seeds the projection from the checkpoint
        // blob — the pre-cut applied entries stay readable after the drop
        if let Some(blob) = &self.checkpoint_blob {
            for (surface, entries) in &blob.applied {
                let list = projected.entry(*surface).or_default();
                for (id, payload) in entries {
                    list.push((Some(*id), payload.clone()));
                }
            }
        }
        let mut sigs: std::collections::HashMap<u64, Vec<molt_core::RosterAttestation>> =
            std::collections::HashMap::new();
        for block in &self.chain {
            if let ChainChange::Applied {
                proposal_id,
                surface,
                payload,
            } = &block.change
            {
                projected
                    .entry(*surface)
                    .or_default()
                    .push((Some(*proposal_id), payload.clone()));
                sigs.insert(*proposal_id, block.sigs.clone());
            }
        }
        self.chain_applied = projected;
        self.chain_applied_sigs = sigs;
        // the gossip-replayed proposal CARDS are older than the chain on a
        // reopen (`open_stored_workspace` replays them first) — settle them
        // against the verified truth or every restart resurrects decided
        // votes as open cards
        self.settle_cards_against_chain();
        // …and the supersede walk reaches the same terminal states a live
        // node reached (shared_memory_real.md §4 replay determinism)
        self.supersede_stale_wiki();
        // …and the working transport anchors. A pruned holder SEEDS them from
        // the blob: the `Restored` blocks that established them were dropped
        // at the cut, and the roster keeps each seat's founding anchor by
        // design — so folding the surviving suffix alone would silently
        // re-address every recovered member to the key it no longer holds.
        let mut anchors: std::collections::HashMap<molt_core::MemberId, String> = self
            .checkpoint_blob
            .as_ref()
            .map(|b| b.anchors.iter().cloned().collect())
            .unwrap_or_default();
        anchors.extend(working_anchors(&self.chain));
        self.chain_anchors = anchors;
        // …and the relay ledger, seeded from the blob for the same reason
        // (the declaring blocks are gone after a cut — R3b/v6)
        let mut ledger: std::collections::HashMap<molt_core::MemberId, Vec<String>> = self
            .checkpoint_blob
            .as_ref()
            .map(|b| b.member_relays.iter().cloned().collect())
            .unwrap_or_default();
        ledger.extend(declared_relays(&self.chain));
        self.chain_member_relays = ledger;
        self.note_relay_splits();
        // R6: an adopted chain may carry pool edits this node has not lived
        // through (catch-up, restore) — adopt the governed pool it lands on
        self.adopt_pool_change();
        // the verified head carries the roster after every membership block —
        // adopt it so the newcomers/rekeys show up in the roster + approvals
        if let Some(head) = &self.chain_head {
            if let Some(r) = &mut self.replica {
                r.identities = head.identities.clone();
                r.roster = head.identities.iter().map(|i| i.member.clone()).collect();
            }
        }
    }

    /// Surface a chain workspace that opened without its local signing key: it
    /// can still verify and follow the chain, but cannot itself co-sign
    /// governance approvals (a reopen that lost `transport.state`'s
    /// `identity_sk`, or a pre-chain workspace). Cheap invariant check, logged.
    pub(crate) fn note_governance_readiness(&self) {
        if self.chain_head.is_some() && self.identity_sk.is_none() {
            tracing::warn!(
                republic = %self.republic_id(),
                "chain workspace has no local signing key — it can follow governance but not co-sign it"
            );
        }
    }
}

// ---- runtime chain governance (real threshold over the mesh) ---------------

impl State {
    /// A workspace whose governance runs through the chain (real m-of-n
    /// signatures) rather than the single-operator path.
    pub(crate) fn is_chain_governed(&self) -> bool {
        self.chain_head.is_some()
    }

    /// The committed change a pending proposal would enact. A registered change
    /// (any kind — e.g. a `Membership` re-admission) wins; otherwise it is a
    /// gated `Applied` reconstructed from the surface proposal.
    fn proposal_change(&self, id: u64) -> Option<ChainChange> {
        if let Some(change) = self.proposal_changes.get(&id) {
            return Some(change.clone());
        }
        let p = self.proposals.get(&id)?;
        // a MEMBERSHIP record without its registered chain change must not
        // fall through to the Applied shape — an approve would then sign a
        // fabricated surface transition instead of the membership bytes
        // everyone else signs (the reserved ops below never pass
        // `validate_org_payload`, so no user proposal can wear them)
        if matches!(
            p.payload.get("op").and_then(serde_json::Value::as_str),
            Some("restore_member" | "add_member")
        ) {
            return None;
        }
        Some(ChainChange::Applied {
            proposal_id: id,
            surface: p.surface,
            payload: p.payload.clone(),
        })
    }

    /// Propose a membership change (re-admit a returning member, or add a seat)
    /// and co-sign it — the producer for `Membership` blocks (recovery step ❹).
    /// Further approvals arrive from the other members; a block seals at m-of-n.
    /// Returns the proposal id.
    pub(crate) fn propose_membership(
        &mut self,
        op: MembershipOp,
        member: &str,
        identity_pk: &str,
        nostr_pk: Option<String>,
        relays: Vec<String>,
        consent: Option<String>,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.proposal_changes.insert(
            id,
            ChainChange::Membership {
                op,
                member: member.to_string(),
                identity_pk: identity_pk.to_string(),
                nostr_pk: nostr_pk.clone(),
                relays: relays.clone(),
                consent: consent.clone(),
            },
        );
        // announce the proposal over the mesh so every member registers + signs
        // the SAME change (the membership twin of a gated `Proposed`)
        let me = self.member();
        let env = self.make_env(
            me,
            WorkspaceEvent::MembershipProposed {
                id: ProposalId(id),
                op,
                member: member.to_string(),
                identity_pk: identity_pk.to_string(),
                nostr_pk: nostr_pk.clone(),
                relays,
                consent,
            },
        );
        self.record(env);
        if self.config.self_cosign {
            self.chain_sign_and_gossip_approval(id);
        }
        id
    }

    /// Register a membership proposal another member put forward, so this node
    /// signs the SAME change (its bytes) when it approves.
    #[allow(clippy::too_many_arguments)] // one gossiped change's fields, not a bag
    pub(crate) fn receive_membership_proposal(
        &mut self,
        id: u64,
        op: MembershipOp,
        member: &str,
        identity_pk: &str,
        nostr_pk: Option<String>,
        relays: Vec<String>,
        consent: Option<String>,
    ) {
        if !self.plausible_wire_id(id) {
            tracing::warn!(%id, "refusing a membership proposal with an implausible id");
            return;
        }
        // L3: pending membership changes are bounded by what can ever be
        // open at once — one re-admission per seat plus slack for Joined
        // seats not on the roster yet
        let pending_membership = self
            .proposal_changes
            .values()
            .filter(|c| matches!(c, ChainChange::Membership { .. }))
            .count();
        let cap = self
            .replica
            .as_ref()
            .map(|r| r.roster.len().saturating_add(8))
            .unwrap_or(16);
        if pending_membership >= cap && !self.proposal_changes.contains_key(&id) {
            tracing::warn!(%id, "refusing a membership proposal beyond the pending cap");
            return;
        }
        self.next_id = self.next_id.max(id.saturating_add(1));
        let change = ChainChange::Membership {
            op,
            member: member.to_string(),
            identity_pk: identity_pk.to_string(),
            nostr_pk,
            relays,
            consent,
        };
        // SECURITY: the id is peer-chosen. `proposal_change` resolves an id
        // to `proposal_changes` first, so registering a Membership under an
        // id that already names a SURFACE proposal (or a different pending
        // change) would make honest members' later Approve of THAT proposal
        // sign these membership bytes instead — a threshold-gate bypass that
        // injects a roster member with no human ever approving a membership
        // change. Refuse any occupied id that is not this exact change.
        if !self.id_free_for(id, &change) {
            tracing::warn!(%id, "refusing a membership proposal whose id names a different change");
            return;
        }
        self.proposal_changes.insert(id, change);
        // L2: signatures that OUTRAN this change become displayable now
        self.reverify_pending(id);
    }

    /// Whether `id` may register `change`: free unless it already names a
    /// surface proposal (`self.proposals`) or a *different* pending chain
    /// change. Re-gossip of the identical change is idempotent (true). The
    /// shared collision guard for every peer-chosen proposal id
    /// (`receive_proposed` / `receive_membership_proposal` /
    /// `receive_checkpoint_proposal`) — see the security note there.
    pub(crate) fn id_free_for(&self, id: u64, change: &ChainChange) -> bool {
        // the identical change re-gossiped is idempotent — checked FIRST,
        // because a membership proposal now also owns a ProposalRecord under
        // its id (the approval surface), and reading that record as a
        // collision would refuse the legitimate re-serve of the very change
        // it belongs to
        if let Some(existing) = self.proposal_changes.get(&id) {
            return existing == change;
        }
        match self.proposals.get(&id) {
            None => true,
            // a membership RECORD may precede its chain-side registration —
            // the log applier runs first in the same ingest turn. It is the
            // same proposal, not a collision, exactly when the record wears
            // this change's reserved op + member (a record can never wear
            // them via cmd_propose — validate_org_payload knows no such op).
            // The threshold stays the security gate, as it always was for
            // membership gossip.
            Some(p) => {
                let ChainChange::Membership { op, member, .. } = change else {
                    return false;
                };
                let want = match op {
                    MembershipOp::Restored => "restore_member",
                    MembershipOp::Joined => "add_member",
                };
                p.surface == Surface::Organization
                    && p.payload.get("op").and_then(serde_json::Value::as_str) == Some(want)
                    && p.payload.get("member").and_then(serde_json::Value::as_str)
                        == Some(member.as_str())
            }
        }
    }

    /// A recovery coordinator's re-admit decision (recovery step ❸): verify a
    /// returning member's seat proof against its ANCHORED identity, then propose
    /// the threshold `Membership{Restored}` block. Recovery re-derives the same
    /// identity, so the requested key must equal the anchored one (it re-keys the
    /// MLS leaf, not the roster). Returns the proposal id, or the refusal reason.
    ///
    /// A verified request also registers the [`PendingRecovery`] (the fresh
    /// KeyPackage + `reply` handover the MLS re-key consumes) — and it must do
    /// so **before** proposing: with a lone coordinator (m=1, self-cosign) the
    /// `Restored` block commits *synchronously inside* `propose_membership`,
    /// and `after_block_applied` keys the re-key on this entry. Registering it
    /// afterwards would silently skip the re-key (the recovery E2E pins this).
    #[allow(clippy::too_many_arguments)] // one verified request's fields, not a bag
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn verify_and_propose_restore(
        &mut self,
        member: &str,
        requested_pk: &str,
        key_package_hex: &str,
        ticket: &str,
        seat_proof: &str,
        new_nostr_pk: &str,
        declared_relays: &[String],
        consent: &str,
        reply: &str,
    ) -> Result<u64, String> {
        let anchored = self
            .replica
            .as_ref()
            .and_then(|r| r.identities.iter().find(|i| i.member == member))
            .map(|i| i.identity_pk.clone())
            .ok_or_else(|| format!("no anchored seat for {member}"))?;
        if requested_pk != anchored {
            return Err("recovery must re-derive the seat's own identity key".to_string());
        }
        let rid = self.republic_id();
        // the anchor AND the relay declaration are verified as the rejoiner
        // SIGNED them: tampering either on the wire makes the proof fail
        // rather than silently re-anchoring the seat or re-routing its ledger
        if !crate::founding::verify_seat_proof(
            &anchored,
            ticket,
            key_package_hex,
            &rid,
            new_nostr_pk,
            declared_relays,
            seat_proof,
        ) {
            return Err(format!("seat proof for {member} does not verify"));
        }
        // the rejoiner's consent — its automatic co-approval (recovery
        // approval design, 2026-08-08). Verified HERE, in the one validation
        // ladder, against the ANCHORED key over the exact content the
        // `Restored` change will carry; present-but-invalid is fail-closed
        // (a doctored consent must not ride a block m members then sign)
        if !consent.is_empty() {
            let bytes = molt_core::chain::restore_consent_bytes(
                &rid,
                member,
                &anchored,
                new_nostr_pk,
            );
            if !molt_storage::identity_verify(&anchored, &bytes, consent) {
                return Err(format!("restore consent for {member} does not verify"));
            }
        }
        // R5 — the re-join gate: a declaration that shares no relay with
        // some member would commit the very split R4 exists to detect. The
        // refusal names the relay the others must add — that message IS the
        // feature. Ordered AFTER the proof (only an authentic declaration
        // earns the named answer) and BEFORE the ticket is consumed upstream.
        if !declared_relays.is_empty() {
            for other in self.roster() {
                if other == member {
                    continue;
                }
                let theirs = self.member_relays(&other);
                if theirs.is_empty() || declared_relays.iter().any(|r| theirs.contains(r)) {
                    continue;
                }
                let named = declared_relays.first().cloned().unwrap_or_default();
                return Err(format!("{named} is in nobody else's pool - add it first"));
            }
        }
        self.pending_recovery.insert(
            member.to_string(),
            PendingRecovery {
                member: member.to_string(),
                key_package: key_package_hex.to_string(),
                reply: reply.to_string(),
            },
        );
        // the verified new transport anchor rides the block — this is what
        // makes it authoritative for every member that APPLIES it, rather
        // than something each node infers from live traffic
        let anchor = if new_nostr_pk.is_empty() {
            None
        } else {
            Some(new_nostr_pk.to_string())
        };
        // R3b/R5: the seat's OWN declaration when it made one; else the pool
        // it was welcomed over — on the loopback path there is no pool and
        // the declaration stays empty
        let relays = if !declared_relays.is_empty() {
            declared_relays.to_vec()
        } else if anchor.is_some() {
            self.ratified_relays()
        } else {
            Vec::new()
        };
        let consent = if consent.is_empty() {
            None
        } else {
            Some(consent.to_string())
        };
        Ok(self.propose_membership(MembershipOp::Restored, member, &anchored, anchor, relays, consent))
    }

    /// Distinct collected approvals for a proposal (for the UI progress).
    pub(crate) fn chain_approval_count(&self, id: u64) -> usize {
        // L2: the DISPLAYED count is the verified one — raw collected sigs
        // could inflate progress with junk a peer gossiped
        self.pending_sigs.get(&id).map(|p| p.verified.len()).unwrap_or(0)
    }

    /// Sign this node's approval of a proposal at the current head+1 and
    /// record + gossip it (the outbox fans the self-authored `Approved`
    /// envelope out over the mesh). Then try to seal. The proposer's own
    /// co-signature and every explicit approve funnel through here.
    pub(crate) fn chain_sign_and_gossip_approval(&mut self, id: u64) {
        let (Some(sk), Some(head)) = (self.identity_sk.as_ref(), self.chain_head.as_ref()) else {
            return;
        };
        let height = head.height + 1;
        let Some(change) = self.proposal_change(id) else {
            return;
        };
        let bytes = approval_bytes(&self.republic_id(), height, &change);
        let sig = molt_storage::identity_sign(sk, &bytes);
        let me = self.member();
        self.collect_sig(id, height, &me, &sig);
        // the own signature is genuine by construction (L2)
        if let Some(p) = self.pending_sigs.get_mut(&id) {
            p.verified.insert(me.clone());
        }
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::Approved {
                id: ProposalId(id),
                by: me,
                height,
                sig,
            },
        );
        self.record(env);
        self.try_commit(id);
    }

    /// D6: keep the collected voter set as record-side DISPLAY data before
    /// the ephemeral signatures are cleared at a seal — each holder shows
    /// the voices that reached IT (over-subscribed voters included); the
    /// block's m signatures stay the only chain truth.
    fn stash_voted(&mut self, id: u64) {
        let members: Vec<molt_core::MemberId> = self
            .pending_sigs
            .get(&id)
            .map(|s| s.sigs.iter().map(|a| a.member.clone()).collect())
            .unwrap_or_default();
        if members.is_empty() {
            return;
        }
        if let Some(p) = self.proposals.get_mut(&id) {
            for m in members {
                if !p.voted.contains(&m) {
                    p.voted.push(m);
                }
            }
        }
    }

    /// Collect one signature into a proposal's pending set: dedup by member
    /// (latest wins), and rebase the set to a newer `height` (dropping stale
    /// signatures) — a signature for an already-superseded height is ignored.
    /// A TERMINAL card collects nothing (D6): a post-seal approval must not
    /// resurrect the ephemeral set the seal just cleared.
    fn collect_sig(&mut self, id: u64, height: u64, member: &str, sig: &str) {
        if self
            .proposals
            .get(&id)
            .is_some_and(|p| p.state != ProposalState::Proposed)
        {
            return;
        }
        // L3: only roster members' signatures collect — dedup is by the
        // free-form member string, so distinct fake names grew one Vec
        // without bound. Roster membership (not link identity) is the rule:
        // the WP2 re-serve legitimately relays other members' signatures.
        if !self
            .chain_head
            .as_ref()
            .is_some_and(|h| h.identities.iter().any(|i| i.member == member))
        {
            return;
        }
        let entry = self.pending_sigs.entry(id).or_default();
        if height > entry.height {
            entry.height = height;
            entry.sigs.clear();
            entry.verified.clear();
        } else if height < entry.height {
            return;
        }
        entry.sigs.retain(|a| a.member != member);
        // the REPLACED signature's verdict must not survive the replacement
        entry.verified.remove(member);
        entry.sigs.push(RosterAttestation {
            member: member.to_string(),
            sig: sig.to_string(),
        });
    }

    /// L2: does this (member, sig) verify against the LIVE target's
    /// approval bytes? Checkable only when the head exists, the height is
    /// the current target and the change is registered here — anything
    /// else is "not verifiable yet", which callers treat as not-displayed
    /// rather than dropped (liveness: an approval may outrun its card).
    fn approval_verifies(&self, id: u64, height: u64, member: &str, sig: &str) -> bool {
        let Some(head) = self.chain_head.as_ref() else {
            return false;
        };
        if height != head.height + 1 {
            return false;
        }
        let Some(change) = self.proposal_change(id) else {
            return false;
        };
        let bytes = approval_bytes(&self.republic_id(), height, &change);
        head.identities
            .iter()
            .any(|i| i.member == member && molt_storage::identity_verify(&i.identity_pk, &bytes, sig))
    }

    /// L2: re-check every collected-but-unverified signature of `id` — the
    /// card (or its registered change) just landed, so sigs that outran it
    /// become displayable now.
    pub(crate) fn reverify_pending(&mut self, id: u64) {
        let Some(pending) = self.pending_sigs.get(&id) else {
            return;
        };
        let height = pending.height;
        let candidates: Vec<(String, String)> = pending
            .sigs
            .iter()
            .filter(|a| !pending.verified.contains(&a.member))
            .map(|a| (a.member.clone(), a.sig.clone()))
            .collect();
        for (member, sig) in candidates {
            if self.approval_verifies(id, height, &member, &sig) {
                if let Some(p) = self.pending_sigs.get_mut(&id) {
                    p.verified.insert(member);
                }
            }
        }
    }

    /// Try to seal a block for a proposal that has gathered the threshold of
    /// valid, distinct signatures at the current head+1. Deterministic: the m
    /// lowest-named valid signers are chosen, so two nodes that both reach the
    /// threshold seal the byte-identical block (it self-dedups on receipt).
    pub(crate) fn try_commit(&mut self, id: u64) {
        let Some(head) = self.chain_head.clone() else {
            return;
        };
        // already committed?
        if matches!(self.proposals.get(&id), Some(p) if p.state != ProposalState::Proposed) {
            return;
        }
        let target = head.height + 1;
        let Some(change) = self.proposal_change(id) else {
            return;
        };
        let bytes = approval_bytes(&self.republic_id(), target, &change);
        let Some(pending) = self.pending_sigs.get(&id) else {
            return;
        };
        if pending.height != target {
            return; // stale set awaiting a re-base
        }
        // D2 (last vote counts): a CURRENT decliner's signature never
        // counts toward m — a stale re-served sig from a peer that missed
        // the decline must not seal a majority-declined proposal here. (A
        // block sealed elsewhere still wins on arrival; the chain is the
        // record.)
        let current_decliners: Vec<molt_core::MemberId> = self
            .proposals
            .get(&id)
            .map(|p| p.decliners.clone())
            .unwrap_or_default();
        let mut valid: Vec<RosterAttestation> = pending
            .sigs
            .iter()
            .filter(|a| {
                !current_decliners.contains(&a.member)
                    && head.identities.iter().any(|i| {
                        i.member == a.member
                            && molt_storage::identity_verify(&i.identity_pk, &bytes, &a.sig)
                    })
            })
            .cloned()
            .collect();
        valid.sort_by(|a, b| a.member.cmp(&b.member));
        valid.dedup_by(|a, b| a.member == b.member);
        // the restored member's consent is one distinct signer (recovery
        // approval design, 2026-08-08) — the sealer must count EXACTLY like
        // `verify_next`, or it seals blocks the verifiers reject. The consent
        // was validated when the change was registered; the member's own
        // roster signature (it is not on the mesh) cannot legitimately be in
        // `pending`, and dropping it here keeps the distinctness rule the
        // verifier enforces.
        let consented = match &change {
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member,
                consent: Some(_),
                ..
            } => {
                valid.retain(|a| a.member != *member);
                1
            }
            _ => 0,
        };
        let need = usize::from(head.rule_m);
        if valid.len() >= need {
            // enough survivor signatures on their own — the consent still
            // rides the change, but never displaces a survivor's voice
            valid.truncate(need);
        } else if valid.len() + consented < need {
            return;
        }
        let block = ChainBlock {
            height: target,
            prev: head.hash.clone(),
            change,
            sigs: valid,
        };
        self.adopt_committed_block(block, id);
    }

    /// Append a block we sealed ourselves: adopt it, then broadcast it to the
    /// mesh (record a self-authored `Committed` envelope the outbox fans out).
    ///
    /// ORDER is load-bearing: `after_block_applied` runs **before** the
    /// `Committed` envelope is recorded. A `Restored` block's re-key advances
    /// this node's MLS epoch and records the raw `MlsCommit` — and because the
    /// outbox encrypts lazily at *send* time, any envelope sequenced before
    /// that `MlsCommit` gets new-epoch ciphertext the still-old-epoch peers
    /// drop (no cross-epoch buffer). Recording `Committed` after the re-key
    /// puts it *behind* the `MlsCommit` in the per-link stream, so every
    /// survivor merges the commit first and then decrypts the block. (The
    /// ephemeral Proposed/Approved gossip sequenced earlier is caught by the
    /// receive side's cross-epoch retry — held until the commit merges — but
    /// this sender-side ordering keeps the BLOCK's delivery independent of
    /// that bounded buffer.) The recovery E2E with a live survivor pins this.
    fn adopt_committed_block(&mut self, block: ChainBlock, proposal_id: u64) {
        if !self.append_committed_block(block.clone()) {
            return;
        }
        let durable = self.persist_chain_now();
        self.after_block_applied(&block);
        // clean up the proposal we just committed — a Membership block carries
        // no proposal id for after_block_applied to key on, so drop it here
        self.stash_voted(proposal_id);
        self.pending_sigs.remove(&proposal_id);
        self.proposal_changes.remove(&proposal_id);
        // **H3 second half (total_review.md): broadcast only what is
        // durable.** The block stays appended and projected — the m
        // signatures are real, and the peers seal the byte-identical block
        // from the same approval gossip themselves — but a node whose disk
        // did not take it must not spread it as republic history: after a
        // crash it would be asking the group for the very block it
        // announced. The writer's failed flag turns the next record into
        // the operator's storage-failed notice.
        if !durable {
            tracing::error!(
                height = block.height,
                proposal = proposal_id,
                "sealed block held back from broadcast — not durable; \
                 peers seal it from the gossip themselves"
            );
            return;
        }
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::Committed(block.clone()));
        self.record(env);
        // an accepted VOTE posts its summary into its discussion (story
        // 2026-08-09) — minted exactly once, by the sealer (a passively
        // applied broadcast/catch-up block receives this message over the
        // wire instead); sequenced AFTER the Committed envelope, so
        // receivers fold the decision before its notice
        if let ChainChange::Applied { payload, .. } = &block.change {
            let payload = payload.clone();
            self.post_decision_summary(proposal_id, &payload, None);
        }
        tracing::debug!(height = block.height, %proposal_id, "sealed and broadcast a chain block");
        // WP4b automation (2026-07-18): checkpoints trigger themselves —
        // HERE and only here, because reaching adopt_committed_block means
        // THIS node just sealed at the live head with fresh signatures. A
        // passively applied block (apply_next_block: catch-up serve,
        // another sealer's broadcast) must never trigger: a node draining
        // a catch-up would propose at a stale intermediate head, and in a
        // lockstep whole-republic catch-up m nodes could even co-sign that
        // stale cut and fork a holder AFTER it dropped its history. After
        // the re-base above, so a cut this very block staled is swept (and
        // announced stale) before the re-propose at the new head.
        self.maybe_auto_checkpoint();
    }

    /// Write the chain as it stands. Returns whether it is DURABLE — the
    /// seal path gates its broadcast on this (H3 second half); a state
    /// without storage has promised nothing, so it reports `true`.
    ///
    /// **Once per accepted batch, never per block.** The round-trip is
    /// synchronous — `persist_chain_blocking` waits on the writer's ack — so
    /// a catch-up draining N blocks used to sit through N blocking
    /// whole-chain writes inside one uninterruptible actor turn. Losing a
    /// batch to a crash costs a re-fetch and nothing else: any survivor
    /// re-serves the blocks on the next catch-up.
    fn persist_chain_now(&self) -> bool {
        #[cfg(test)]
        CHAIN_PERSISTS.with(|c| c.set(c.get() + 1));
        let Some(active) = &self.active else {
            return true;
        };
        let durable = active
            .handle
            .persist_chain_blocking(self.checkpoint_blob.clone(), self.chain.clone());
        if !durable {
            // The writer also raises its `failed` flag, which the next
            // `record` turns into the operator's "storage-failed" notice.
            // Named here as well because THIS is the write whose loss
            // matters most: the chain is the republic's agreed history, and
            // a block that never reached the disk is one this node will ask
            // for again after a crash.
            tracing::error!("the chain did not reach the disk — it is only in memory");
        }
        durable
    }

    /// Verify a block as the extension of our chain, append it, and re-project
    /// state. Returns whether it was accepted. **Does not persist** — the
    /// caller does, once per batch ([`State::persist_chain_now`]).
    fn append_committed_block(&mut self, block: ChainBlock) -> bool {
        // verify BEFORE appending — the block only ever touches `self.chain`
        // once it has passed, so there is nothing to roll back
        match self.extend_own(&block) {
            Ok(head) => {
                self.chain_head = Some(head);
                // an append only ADDS to the projection — no whole-chain refold
                self.project_one(&block);
                self.chain.push(block);
                self.bump_next_id_past_chain();
                true
            }
            Err(e) => {
                // routine, not an internal fault: a stale re-serve during
                // catch-up and a hostile peer both land here
                tracing::warn!(height = block.height, error = %e, "refused a chain block");
                false
            }
        }
    }

    /// After a block is applied (by us or a peer): mark its proposal committed,
    /// emit, clear its collected signatures, and re-base every other pending
    /// proposal onto the new head (their old-height signatures are now stale).
    fn after_block_applied(&mut self, block: &ChainBlock) {
        match &block.change {
            ChainChange::Applied {
                proposal_id,
                surface,
                payload,
            } => {
                // a block for a proposal this node never heard of (lost
                // gossip, late join) still yields a full accepted card
                self.ensure_applied_record(*proposal_id, *surface, payload.clone());
                if let Some(p) = self.proposals.get_mut(proposal_id) {
                    p.state = ProposalState::Applied;
                }
                self.stash_voted(*proposal_id);
                self.pending_sigs.remove(proposal_id);
                self.emit(Event::Applied {
                    id: ProposalId(*proposal_id),
                    surface: *surface,
                });
                if *surface == Surface::Organization {
                    self.after_org_applied();
                }
            }
            // a re-admission committed: on EVERY node, a threshold-approved
            // recovery outranks the announce rate limit (the member's fresh
            // announce must never be swallowed by a cooldown stamped for its
            // previous life — e.g. a re-recovery within the window); if THIS
            // node coordinated it (holds the returning member's fresh
            // KeyPackage), it also drives the MLS re-key
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member,
                ..
            } => {
                // the approval surface (recovery approval design, 2026-08-08):
                // flip the visible membership record and drop the vote
                // bookkeeping on EVERY node. A Membership block carries no
                // proposal id, so match by content — the Checkpoint arm's
                // pattern (the committer also cleans by id upstream).
                self.settle_membership_records(&block.change);
                self.mesh_extension_at.remove(member);
                if self.pending_recovery.contains_key(member) {
                    let member = member.clone();
                    self.coordinator_rekey(&member);
                }
            }
            ChainChange::Membership {
                op: MembershipOp::Joined,
                ..
            } => {
                self.settle_membership_records(&block.change);
            }
            // WP4b: a checkpoint sealed — on EVERY node, drop the matching
            // proposal bookkeeping (the committer also cleans by id in
            // adopt_committed_block; receivers find it by content). Local
            // block-dropping below `upto` is stage 4.
            ChainChange::Checkpoint { upto, .. } => {
                let sealed = &block.change;
                let ids: Vec<u64> = self
                    .proposal_changes
                    .iter()
                    .filter(|(_, c)| *c == sealed)
                    .map(|(id, _)| *id)
                    .collect();
                for id in ids {
                    self.proposal_changes.remove(&id);
                    self.stash_voted(id);
                    self.pending_sigs.remove(&id);
                }
                // B-F2: drop the summarized history locally, automatically —
                // the vote just confirmed this summary is correct. The blob
                // becomes the holder's trust anchor; the chain keeps the
                // checkpoint block and everything after it.
                let upto = *upto;
                let anchor_height = block.height;
                match self.own_checkpoint_state(upto) {
                    Ok(blob) => {
                        self.set_checkpoint_blob(Some(blob));
                        self.chain.retain(|b| b.height >= anchor_height);
                        self.apply_chain_to_state();
                        self.persist_chain_now();
                        self.emit(Event::CheckpointSealed {
                            height: anchor_height,
                            upto,
                        });
                        tracing::info!(height = anchor_height, upto, "checkpoint sealed — history below the cut dropped");
                    }
                    Err(e) => {
                        // keep full history rather than drop on a state we
                        // could not recompute (should be impossible: the
                        // verifier just matched this very state)
                        tracing::warn!(error = %e, "checkpoint sealed but the blob could not be built — keeping full history");
                    }
                }
            }
            _ => {}
        }
        self.rebase_pending_approvals();
    }

    /// A membership block sealed — settle its approval surface on THIS node
    /// (recovery approval design, 2026-08-08): flip every open membership
    /// record that describes exactly this change to `Applied` and drop the
    /// matching vote bookkeeping. Content-matched (a Membership block carries
    /// no proposal id), so the sealer, every passive applier and a catch-up
    /// all settle identically.
    fn settle_membership_records(&mut self, sealed: &ChainChange) {
        let ChainChange::Membership { op, member, .. } = sealed else {
            return;
        };
        let want_op = match op {
            MembershipOp::Restored => "restore_member",
            MembershipOp::Joined => "add_member",
        };
        let ids: Vec<u64> = self
            .proposals
            .iter()
            .filter(|(_, p)| {
                p.state == ProposalState::Proposed
                    && p.surface == Surface::Organization
                    && p.payload.get("op").and_then(serde_json::Value::as_str) == Some(want_op)
                    && p.payload.get("member").and_then(serde_json::Value::as_str)
                        == Some(member.as_str())
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(p) = self.proposals.get_mut(&id) {
                p.state = ProposalState::Applied;
            }
            self.emit(Event::Applied {
                id: ProposalId(id),
                surface: Surface::Organization,
            });
        }
        let stale: Vec<u64> = self
            .proposal_changes
            .iter()
            .filter(|(_, c)| *c == sealed)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.proposal_changes.remove(&id);
            self.stash_voted(id);
            self.pending_sigs.remove(&id);
        }
    }

    /// WP4b automation (product decision 2026-07-18): the compaction cut
    /// proposes ITSELF — the GUI button is gone; `propose_checkpoint`
    /// stays as the co-equal MCP verb (manual override). Collision-free
    /// and deterministic by construction:
    ///
    /// - only the alphabetically LOWEST-named roster member triggers
    ///   (one proposer — proposal ids are node-local, two simultaneous
    ///   auto-proposers would collide); if that member is offline no cut
    ///   happens, exactly like a manual proposer being away,
    /// - only right after THIS node itself sealed a block at the live
    ///   head (`adopt_committed_block`) — every member that just
    ///   co-signed is at the same head, which is what the receivers'
    ///   verify-before-sign recomputation needs. Passively applied
    ///   blocks (`apply_next_block`: catch-up serves, another sealer's
    ///   broadcast) never trigger — a catching-up node would propose at
    ///   a stale intermediate head (the `catchup_from`/`pending_blocks`
    ///   guard below is defense in depth only: the first served block
    ///   already clears `catchup_from`),
    /// - never while a vote is open (an interfering seal would stale
    ///   the cut; the commit resolving the last open vote re-fires
    ///   this check),
    /// - a staled cut needs no timer or backoff: the very block that
    ///   staled it lands here again and re-proposes at the new head, so
    ///   there is at most one auto-propose per committed block.
    fn maybe_auto_checkpoint(&mut self) {
        if self.chain.len() < AUTO_CHECKPOINT_MIN_LEN {
            return;
        }
        let Some(head) = self.chain_head.as_ref() else {
            return;
        };
        // Only a buffered block ADJACENT to head pins the cut: it is about
        // to apply and would stale the checkpoint on arrival. A gap block
        // cannot apply next, and the buffer accepts claims up to head+4096
        // — gating on "any buffered block" let one plausible far-future
        // claim freeze compaction until a drain cleared it (known-debt
        // refinement, 2026-08-16 list).
        if self.catchup_from.is_some() || self.pending_blocks.contains_key(&(head.height + 1)) {
            return;
        }
        let me = self.member();
        let lowest = head.identities.iter().map(|i| i.member.as_str()).min();
        if lowest != Some(me.as_str()) {
            return;
        }
        // "a vote is open": a surface proposal still Proposed, signatures
        // still being collected, or a cut already in flight. Committed
        // membership residue in `proposal_changes` (never swept on
        // receivers) must NOT block the automation forever, so registered
        // changes only count via their pending signatures — except a
        // checkpoint entry, which means a cut is already pending.
        let vote_open = self
            .proposals
            .values()
            .any(|p| p.state == ProposalState::Proposed)
            || !self.pending_sigs.is_empty()
            || self
                .proposal_changes
                .values()
                .any(|c| matches!(c, ChainChange::Checkpoint { .. }));
        if vote_open {
            return;
        }
        match self.cmd_propose_checkpoint() {
            Ok(_) => {
                tracing::info!(len = self.chain.len(), "auto-proposed a compaction checkpoint");
            }
            Err(e) => tracing::warn!(error = %e, "auto-checkpoint propose failed"),
        }
    }

    /// The coordinator's MLS re-key once a `Restored` block committed: run
    /// `restore_member` on the runtime group with the returning member's fresh
    /// KeyPackage → `(commit, welcome)`, then distribute both. The commit is
    /// broadcast to the survivors over the mesh (a recorded `MlsCommit`, sent raw
    /// so each survivor advances to the new epoch); the welcome goes to the
    /// returning member's reply queue. Finally the rejoin is announced in the
    /// group chat. Consumes the pending recovery. A node with no runtime group
    /// logs and does nothing.
    ///
    /// A **Nostr** republic takes the other arm entirely
    /// ([`State::coordinator_rekey_nostr`]): it has no `NetRuntime`, its group
    /// MLS lives on `GroupNet`, its commit rides a 445 at a pinned stamp and
    /// its Welcome is a gift wrap rather than a reply queue.
    fn coordinator_rekey(&mut self, member: &str) {
        let Some(pending) = self.pending_recovery.remove(member) else {
            return;
        };
        let Ok(kp) = hex::decode(&pending.key_package) else {
            tracing::warn!(%member, "recovery KeyPackage is not valid hex");
            return;
        };
        if self.group_net.is_some() {
            self.coordinator_rekey_nostr(member, &kp);
            return;
        }
        match self.net.as_ref().and_then(|n| n.restore_member_on_group(member, &kp)) {
            Some(Ok((commit, welcome))) => {
                let me = self.member();
                // 1) broadcast the raw re-key commit to the survivors: recorded as
                // an `MlsCommit`, the outbox fans it out; every survivor merges it
                // and advances to the new epoch (it MUST precede any new-epoch
                // traffic — hence recorded before the announcement below).
                let env =
                    self.make_env(me.clone(), WorkspaceEvent::MlsCommit { commit: hex::encode(&commit) });
                self.record(env);
                // 2) deliver the welcome + the whole chain to the returning
                // member's reply queue so it rejoins the group AND catches its
                // state up over this same channel (option A). Off the actor.
                if let Some(transport) = self.net.as_ref().and_then(|n| n.runtime_transport()) {
                    let chain_json = match &self.checkpoint_blob {
                        // a pruned coordinator serves blob + suffix — the
                        // rejoiner verifies via the suffix rules (4c)
                        Some(blob) => serde_json::to_string(&ServedChainWire::Pruned {
                            checkpoint_blob: blob.clone(),
                            blocks: self.chain.clone(),
                        }),
                        None => serde_json::to_string(&self.chain),
                    }
                    .unwrap_or_default();
                    crate::recovery::spawn_welcome_send(
                        transport,
                        pending.reply.clone(),
                        welcome,
                        chain_json,
                    );
                }
                // 3) announce the rejoin in the group chat — AFTER the commit, so
                // the survivors have advanced to the epoch this notice is
                // encrypted at (ephemeral, best-effort like all chat). A
                // System-kind message: every frontend renders it as a quiet
                // system line, not as the coordinator speaking.
                if let Err(e) = self.post_message_with_kind(
                    me,
                    format!("🔑 {member} rejoined the republic after recovery"),
                    None,
                    molt_core::ChannelRef::Group,
                    molt_core::ChatKind::System,
                ) {
                    // best-effort, like all chat — never blocks the re-key
                    tracing::warn!(error = %e, "could not post the rejoin notice");
                }
                // 4) dynamic mesh membership: the rejoiner's mesh announce
                // follows on this same recovery queue — accept it for exactly
                // this member (docs_archive/transport/dynamic_mesh.md §3)
                self.recovery_mesh_window.insert(member.to_string());
                tracing::info!(%member, "re-keyed the group, broadcast the commit, sent the welcome");
            }
            Some(Err(e)) => tracing::warn!(%member, error = %e, "MLS re-key failed"),
            // this arm is reached in TWO different situations and must not
            // describe them as one: a demo/state-only node has no group at
            // all, while a Nostr workspace HAS one — on `GroupNet`, which
            // `restore_member_on_group` cannot see (it reads
            // `NetRuntime::real_crypto`). Saying "state-only" there sends a
            // debugger looking for a missing group instead of a missing arm.
            None => tracing::warn!(
                %member,
                group = "none",
                "no re-key path for this workspace — the returning seat gets no welcome"
            ),
        }
    }

    /// **The Nostr coordinator's re-key** (N4b step 6c).
    ///
    /// Everything the mesh arm does through a `NetRuntime` and a reply queue,
    /// done through `GroupNet` and the relays instead:
    ///
    /// 1. pin the carrier stamp BEFORE committing, and key the commit with it
    ///    (choosing it afterwards is too late — the commit is already made);
    /// 2. publish the commit as a 445 at exactly that stamp, sealed under the
    ///    epoch this node just left;
    /// 3. gift-wrap the 444 Welcome to the seat's **new** anchor —
    ///    `working_nostr_pk` already returns it, because `project_one` folds
    ///    the `Restored` block before `after_block_applied` runs;
    /// 4. offer the chain ANCHOR (the smallest prefix that verifies), not the
    ///    chain: a pruned holder's whole blob does not fit a gift wrap, and
    ///    pruned is the normal state.
    ///
    /// Steps 2 and 3 are off the actor; 1 and 4 are on it. Every failure is
    /// named — a recovery that quietly does nothing leaves a member locked out
    /// with no way to find out why.
    fn coordinator_rekey_nostr(&mut self, member: &str, key_package: &[u8]) {
        // **Everything that can fail is resolved BEFORE the group is touched.**
        //
        // `nostr_rekey` advances the epoch and evicts the old leaf, and there
        // is no undo. A re-key whose delivery then turns out to be impossible
        // leaves this node alone on an epoch no survivor knows about, unable
        // to be read by anyone until some later commit rescues it — the same
        // split the commit-before-welcome rule exists to prevent, reached
        // through an earlier door.
        let relays = self.dialable_group_relays();
        if relays.is_empty() {
            tracing::error!(%member, "no dialable relay for this republic — the re-key cannot be delivered");
            return;
        }
        let Ok(dialer) = self.dialer_for() else {
            tracing::error!(%member, "no usable dial route — the re-key cannot be delivered");
            return;
        };
        // the transport material, copied out so the group borrow below is free
        let Some(nostr) = self.nostr.as_ref() else {
            return;
        };
        let rotation_seed = nostr.rotation_seed;
        // the payload carries what the GROUP ratified, not this node's own
        // intersection: the rejoiner gates that list through its own pool,
        // and handing it a narrowed one would silently shrink the republic
        let ratified: Vec<String> = nostr
            .relays
            .iter()
            .take(molt_net::welcome::MAX_PAYLOAD_RELAYS)
            .cloned()
            .collect();
        let net = match molt_net::ritual_net::RitualNet::new(
            dialer.clone(),
            relays.clone(),
            &nostr.sk,
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(%member, error = %e, "recovery transport keys — the re-key cannot be delivered");
                return;
            }
        };
        let channel = molt_net::ritual_net::GroupChannel::new(dialer, relays, rotation_seed);
        // the NEW anchor: the `Restored` block that triggered this re-key has
        // already been folded, so this is the key the seat just proved it holds
        let to = self.working_nostr_pk(member);
        if to.is_empty() {
            tracing::error!(%member, "the restored seat carries no transport anchor — nothing to address the welcome to");
            return;
        }

        // --- past here the group really changes -------------------------------
        // the stamp is chosen before anything is committed and travels
        // unchanged into both `restore_member` and `publish_frame_at`
        let stamp = molt_storage::now_secs();
        let Some(group) = self.group_net.as_ref() else {
            return;
        };
        let rekey = match nostr_rekey(&group.mls, member, key_package, stamp) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(%member, error = %e, "the Nostr re-key failed — the returning seat gets no welcome");
                return;
            }
        };
        let payload = molt_net::welcome::WelcomePayload {
            welcome: rekey.welcome.clone(),
            rotation_seed,
            relays: ratified,
        };
        crate::nostr_ritual::spawn_rekey_delivery(
            channel,
            net,
            to,
            rekey,
            payload,
            member.to_string(),
        );
        // **Forget the seat's OLD accept window.** The returning member is a
        // new incarnation whose log seq space restarts at 1, so the marks from
        // the lost device swallow every fresh envelope as a duplicate — its
        // chat, and the `ChainRequest` that pulls everything above the anchor.
        //
        // The mesh does this at its authenticated recovery-announce
        // (`cmd_net_recover_announced`), which a Nostr republic has no
        // equivalent of. It does have a stronger one: this re-key runs only
        // behind a threshold-committed `Restored` block for exactly this seat.
        self.reset_peer_accept_window(&member.to_string());
        // the rejoiner's trust root, over the same 445 channel it just joined
        self.serve_chain_anchor();
        // …and the same quiet system line the mesh arm posts. It is encrypted
        // at the NEW epoch, so it can outrun the commit that is still being
        // published — a survivor holds it and retries after the merge
        // (N5.3c), which is exactly what that hold exists for.
        let me = self.member();
        if let Err(e) = self.post_message_with_kind(
            me,
            format!("🔑 {member} rejoined the republic after recovery"),
            None,
            molt_core::ChannelRef::Group,
            molt_core::ChatKind::System,
        ) {
            tracing::warn!(error = %e, "could not post the rejoin notice");
        }
        tracing::info!(%member, stamp, "re-keyed the group on Nostr and offered the chain anchor");
    }

    /// Re-sign this node's standing approvals at the new head+1: an approval
    /// this node already gave (its signature is in the stale set) is a decision
    /// that still stands, only its position moved — so re-express it (the human
    /// is not asked again). Proposals this node did not approve are just cleared.
    fn rebase_pending_approvals(&mut self) {
        let Some(head) = self.chain_head.as_ref() else {
            return;
        };
        let target = head.height + 1;
        let me = self.member();
        let stale: Vec<u64> = self
            .pending_sigs
            .iter()
            .filter(|(_, p)| p.height < target)
            .map(|(id, _)| *id)
            .collect();
        // sweep checkpoint entries that never made it into pending_sigs
        // (a proposer without self-cosign, a bailed sign): a cut below the
        // new head can never seal (upto == height-1 is enforced) and must
        // not linger as bookkeeping a late Approved could resurrect
        let head_height = head.height;
        let swept: Vec<u64> = self
            .proposal_changes
            .iter()
            .filter(|(_, c)| {
                matches!(c, ChainChange::Checkpoint { upto, .. } if *upto < head_height)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in swept {
            self.proposal_changes.remove(&id);
            self.pending_sigs.remove(&id);
            // closure for the proposer/operator: this cut can never seal —
            // re-propose at the new head (the stale loop below can no
            // longer see these entries, so the emit lives HERE)
            self.emit(Event::CheckpointStale { id: ProposalId(id) });
        }
        for id in stale {
            let mine = self
                .pending_sigs
                .get(&id)
                .is_some_and(|p| p.sigs.iter().any(|a| a.member == me));
            self.pending_sigs.remove(&id);
            // WP4b: a checkpoint's change is CUT-bound (upto == height - 1,
            // enforced by the verifier) — after the head moved, re-signing
            // the old cut could only seal an invalid block. Drop it; the
            // proposer re-proposes at the new head (doc §B.2).

            // only re-sign for proposals still pending that this node approved
            if mine && matches!(self.proposals.get(&id), Some(p) if p.state == ProposalState::Proposed)
            {
                self.chain_sign_and_gossip_approval(id);
            }
        }
    }

    /// WP4b stage 4: verify a candidate chain in THIS holder's context —
    /// a full holder verifies from the genesis, a pruned holder from its
    /// checkpoint blob (`verify_suffix_chain`). The one entry every
    /// adopt/append/probe path routes through.
    pub(crate) fn verify_own(&self, blocks: &[ChainBlock]) -> Result<ChainHead, String> {
        Ok(self.walk_own(blocks)?.head)
    }

    /// [`State::verify_own`], keeping the walk.
    pub(crate) fn walk_own(&self, blocks: &[ChainBlock]) -> Result<ChainWalk, String> {
        match &self.checkpoint_blob {
            None => walk_chain(blocks),
            Some(blob) => walk_suffix_chain(blob, blocks, &self.republic_id()),
        }
    }

    /// Verify ONE block as the extension of the chain we already hold — the
    /// hot path, and the reason the walk is cached.
    ///
    /// Re-walking the whole chain per block made catching up N blocks cost
    /// `m·N(N+1)` signature verifications inside a single actor turn; a
    /// catching-up node then looked exactly like a dead one to its peers.
    /// This is the same verification, with the intermediate state kept
    /// instead of thrown away: the walk is driven by the identical
    /// [`ChainWalk::step`] the full verifiers use.
    ///
    /// The cache is never trusted on its word — it is used only while it
    /// still describes our chain, and rebuilt by a full walk otherwise. A
    /// **refused** block leaves it intact (`step` is atomic), so a peer
    /// spamming bad blocks cannot force a re-walk per block either.
    fn extend_own(&mut self, block: &ChainBlock) -> Result<ChainHead, String> {
        let mut walk = match self.chain_walk.take() {
            Some(w) if w.describes(&self.chain, self.checkpoint_blob.as_ref()) => w,
            _ => self.walk_own(&self.chain)?,
        };
        let stepped = walk.step(block);
        let head = walk.head.clone();
        self.chain_walk = Some(walk);
        stepped.map(|()| head)
    }

    /// The canonical state at `upto` from THIS holder's own material —
    /// genesis-rooted for a full holder, blob-based for a pruned one.
    /// What the propose/verify-before-sign paths hash.
    pub(crate) fn own_checkpoint_state(
        &self,
        upto: u64,
    ) -> Result<molt_core::CheckpointState, String> {
        match &self.checkpoint_blob {
            None => checkpoint_state(&self.chain, upto),
            // the anchor block in chain[0] is state-neutral for the fold
            Some(blob) => fold_state(blob.clone(), &self.chain, upto),
        }
    }

    /// The co-equal Chain-History read (`Command::ReadChain`): every
    /// committed block of the open republic as a display view, newest
    /// first — checkpoint blocks included. Read-only and synchronous.
    ///
    /// A PRUNED holder (`checkpoint_blob` is `Some`) APPENDS synthetic
    /// views for the history below the cut, rebuilt from the blob: the
    /// pre-cut applied entries (newest first, per the blob's per-surface
    /// projections) and one genesis view from the founding table. Pre-cut
    /// heights are NOT reconstructible per entry — the blob folds the
    /// dropped blocks into per-surface `(proposal_id, payload)` lists and
    /// loses each block's position (and its signature set, so `signers`
    /// stays empty), which is why every synthetic entry carries height 0:
    /// it marks "below the cut", not a real chain position. The blob also
    /// loses the cross-surface interleaving, so the synthetic ordering is
    /// per-surface block order, best-effort.
    pub(crate) fn cmd_read_chain(&self) -> Result<molt_core::Reply, molt_core::MoltError> {
        let mut blocks: Vec<molt_core::ChainBlockView> =
            self.chain.iter().rev().map(chain_block_view).collect();
        if let Some(blob) = &self.checkpoint_blob {
            let mut pre: Vec<molt_core::ChainBlockView> = Vec::new();
            for (surface, entries) in &blob.applied {
                for (id, payload) in entries {
                    pre.push(molt_core::ChainBlockView {
                        height: 0,
                        kind: "applied".to_string(),
                        surface: surface.as_str().to_string(),
                        payload: payload.clone(),
                        proposal_id: *id,
                        signers: Vec::new(),
                    });
                }
            }
            pre.reverse(); // blob order is oldest-first; the read is newest-first
            blocks.extend(pre);
            // the founding constitution, rebuilt from the blob's (rid-pinned)
            // founding table — the genesis is n-of-n by chain invariant, so
            // the founding members ARE its signers even though the block
            // (and its attestation bytes) were dropped with the history
            blocks.push(molt_core::ChainBlockView {
                height: 0,
                kind: "genesis".to_string(),
                surface: String::new(),
                payload: serde_json::Value::String(blob.founding_name.clone()),
                proposal_id: 0,
                signers: blob
                    .founding_identities
                    .iter()
                    .map(|i| i.member.clone())
                    .collect(),
            });
        }
        Ok(molt_core::Reply::Chain { blocks })
    }

    /// WP4b stage 3: the human verb — propose the compaction cut at the
    /// CURRENT head (`upto` = head height, B-F1). The engine computes the
    /// canonical state hash itself, announces it, and co-signs; every
    /// receiver recomputes before signing (`receive_checkpoint_proposal`).
    pub(crate) fn cmd_propose_checkpoint(
        &mut self,
    ) -> Result<molt_core::Reply, molt_core::MoltError> {
        if !self.is_chain_governed() {
            return Err(molt_core::MoltError::BadPayload(
                "checkpoints need a chain-governed republic".into(),
            ));
        }
        let Some(head) = self.chain_head.as_ref() else {
            return Err(molt_core::MoltError::BadPayload("no chain head".into()));
        };
        let upto = head.height;
        let state = self
            .own_checkpoint_state(upto)
            .map_err(molt_core::MoltError::BadPayload)?;
        let state_hash = checkpoint_state_hash(&state);
        let id = self.next_id;
        self.next_id += 1;
        self.proposal_changes.insert(
            id,
            ChainChange::Checkpoint {
                upto,
                state_hash: state_hash.clone(),
            },
        );
        let me = self.member();
        let env = self.make_env(
            me,
            WorkspaceEvent::CheckpointProposed {
                id: ProposalId(id),
                upto,
                state_hash,
            },
        );
        self.record(env);
        if self.config.self_cosign {
            self.chain_sign_and_gossip_approval(id);
        }
        Ok(molt_core::Reply::Proposed { id: ProposalId(id) })
    }

    /// WP4b stage 3, receive side: verify BEFORE sign. Recompute the
    /// canonical state from OUR OWN chain at the proposed cut and co-sign
    /// only on an exact hash match — nobody ever signs a foreign blob. A
    /// cut that is not our current head is skipped and NOT buffered: a
    /// lagging node simply misses this cut (v1 liveness limit, stage-5
    /// pin in `docs_archive/chain/log_compaction.md`) — the proposer re-proposes
    /// at the then-current head; a stale cut dies on re-base anyway.
    pub(crate) fn receive_checkpoint_proposal(&mut self, id: u64, upto: u64, state_hash: &str) {
        // L3: the guard runs BEFORE the bump — `id + 1` on u64::MAX was a
        // one-frame remote ABORT (overflow-checks + panic=abort), and an
        // in-range absurd id would poison the mint counter
        if !self.plausible_wire_id(id) {
            tracing::warn!(%id, "refusing a checkpoint proposal with an implausible id");
            return;
        }
        self.next_id = self.next_id.max(id.saturating_add(1));
        let Some(head) = self.chain_head.as_ref() else {
            return;
        };
        if head.height != upto {
            tracing::debug!(%id, upto, head = head.height, "ignoring a checkpoint cut that is not our head");
            return;
        }
        let ours = match self.own_checkpoint_state(upto) {
            Ok(state) => checkpoint_state_hash(&state),
            Err(e) => {
                tracing::warn!(%id, error = %e, "cannot recompute the proposed checkpoint state");
                return;
            }
        };
        if ours != state_hash {
            tracing::warn!(%id, "refusing to co-sign a checkpoint that does not match our own projection");
            return;
        }
        // NO id-collision signing (review finding): the peer chose the id,
        // and chain_sign_and_gossip_approval signs whatever change the id
        // RESOLVES to — an id that already names a surface or membership
        // proposal would turn this auto-cosign into an unattended approval
        // of a human-decision change (or let human approvals of that
        // proposal silently sign checkpoint bytes). Refuse any occupied id
        // that is not this exact checkpoint.
        let this = ChainChange::Checkpoint {
            upto,
            state_hash: state_hash.to_string(),
        };
        if !self.id_free_for(id, &this) {
            tracing::warn!(%id, "refusing a checkpoint proposal whose id names a different change");
            return;
        }
        match self.proposal_changes.get(&id) {
            Some(existing) if *existing != this => {
                tracing::warn!(%id, "refusing a checkpoint proposal whose id names a different change");
                return;
            }
            _ => {}
        }
        // L3: ONE cut per head — the identical (upto, state_hash) under a
        // second id would mint one registry entry + one signed Approved
        // per frame (1:1 outbound amplification); the first id IS the cut
        if self
            .proposal_changes
            .iter()
            .any(|(other, c)| *other != id && *c == this)
        {
            tracing::debug!(%id, upto, "ignoring a duplicate checkpoint cut under a fresh id");
            return;
        }
        self.proposal_changes.insert(id, this);
        // replay guard: one signature per member per cut — a re-received
        // frame must not amplify into fresh Approved gossip
        let me = self.member();
        let target = head.height + 1;
        if self
            .pending_sigs
            .get(&id)
            .is_some_and(|p| p.height == target && p.sigs.iter().any(|a| a.member == me))
        {
            return;
        }
        // correctness attestation, not a product decision: co-sign directly
        self.chain_sign_and_gossip_approval(id);
    }

    /// Inbound: a peer proposed something (gossip). Record it as pending so it
    /// shows up and can be approved here. `by` is the authenticated wire
    /// sender — the proposer on a direct delivery, the serving peer on a
    /// WP2 re-serve (a display hint, never an authorization input).
    /// Returns `true` only when the proposal was genuinely NEW here — a
    /// refused id collision or a deduplicated re-serve (WP2 catch-up
    /// re-wraps open proposals under the serving peer's name) returns
    /// `false`, and the caller must not announce it on the event stream.
    /// L3: a peer-chosen proposal id far past the mint counter is garbage —
    /// registering it (or even bumping `next_id` for it) would poison every
    /// later local mint (a u64::MAX id would freeze proposing for good).
    /// Window shared with the decline park.
    fn plausible_wire_id(&self, id: u64) -> bool {
        id <= self
            .next_id
            .saturating_add(crate::proposals::PARKED_DECLINE_ID_WINDOW)
    }

    pub(crate) fn receive_proposed(
        &mut self,
        id: u64,
        surface: Surface,
        payload: serde_json::Value,
        by: &str,
    ) -> bool {
        if !self.plausible_wire_id(id) {
            tracing::warn!(%id, "refusing a proposal with an implausible id");
            return false;
        }
        // L3: a flooding proposer may only crowd ITSELF — the newest card
        // is refused (the WP2 re-serve re-earns an honest one later), and
        // another member's cards are never evicted
        if !self.proposals.contains_key(&id) {
            let open_by = self
                .proposals
                .values()
                .filter(|p| p.state == ProposalState::Proposed && p.by == by)
                .count();
            if open_by >= OPEN_CARDS_PER_PROPOSER_MAX {
                tracing::warn!(%id, %by, "refusing a proposal beyond the per-proposer open cap");
                return false;
            }
        }
        self.next_id = self.next_id.max(id.saturating_add(1));
        // SECURITY (symmetric to receive_membership_proposal): an id already
        // registered in `proposal_changes` (a membership/checkpoint change)
        // must not also become a surface proposal — `proposal_change` would
        // keep resolving it to the chain change, so approvals of this
        // "surface proposal" would sign that change's bytes.
        if self.proposal_changes.contains_key(&id) {
            tracing::warn!(%id, "refusing a surface proposal whose id names a chain change");
            return false;
        }
        // an id the verified chain already consumed (the walk's double-apply
        // guard, blob-seeded on a pruned holder) can only be a stale resend —
        // a fresh card would resurrect a decided vote. The reopen twin of
        // this guard is `settle_cards_against_chain`.
        if self.chain_walk.as_ref().is_some_and(|w| w.seen.contains(&id)) {
            tracing::debug!(%id, "refusing a proposal the chain already consumed");
            return false;
        }
        let mut inserted = false;
        self.proposals.entry(id).or_insert_with(|| {
            inserted = true;
            molt_core::ProposalRecord {
                surface,
                payload,
                approvals: 0,
                state: ProposalState::Proposed,
                declined_at: 0,
                declined_by: String::new(),
                decliners: Vec::new(),
                voted: Vec::new(),
                by: by.to_string(),
                superseded: false,
                withdrawn: false,
            }
        });
        if inserted && surface == Surface::Memory {
            // registration-time check (shared_memory_real.md §4): a patch
            // learned LATE against an already-moved base registers
            // superseded right away — no zombie pending cards on rejoiners
            self.supersede_stale_wiki();
        }
        if inserted {
            // L2: signatures that OUTRAN this card become displayable now
            self.reverify_pending(id);
        }
        inserted
    }

    /// Inbound: a peer's signed approval (gossip). Collect + try to seal.
    pub(crate) fn receive_approval(&mut self, id: u64, by: &str, height: u64, sig: &str) {
        if sig.is_empty() {
            return;
        }
        // SECURITY: `height` is peer-supplied. A legitimate approval can
        // only be for the current target (head + 1) or a value we already
        // hold; an out-of-range height (e.g. u64::MAX) would let collect_sig
        // adopt it, clear the real signatures, and — since rebase only
        // sweeps heights BELOW the target — never recover, permanently
        // freezing the proposal (governance-liveness DoS). Bound it here.
        let target = self.chain_head.as_ref().map(|h| h.height + 1);
        if target.is_some_and(|t| height > t) {
            tracing::warn!(%id, height, "dropping an approval for an implausible future height");
            return;
        }
        // L3: an approval may OUTRUN its card (collected, displayed once it
        // lands) — but only inside the same id window everything else uses,
        // or unknown-id entries grow without bound
        if !self.plausible_wire_id(id) {
            tracing::warn!(%id, "dropping an approval for an implausible proposal id");
            return;
        }
        self.collect_sig(id, height, by, sig);
        if self.approval_verifies(id, height, by, sig) {
            if let Some(p) = self.pending_sigs.get_mut(&id) {
                p.verified.insert(by.to_string());
            }
        }
        self.try_commit(id);
    }

    /// Inbound: a peer broadcast (or re-served) a committed block. Extend the
    /// single branch when it is the next height, tie-break a contended slot we
    /// already filled, or — when it is ahead of us — buffer it and request the
    /// missing suffix (catch-up).
    pub(crate) fn receive_block(&mut self, block: ChainBlock) {
        let Some(head) = self.chain_head.clone() else {
            // a headless rejoiner (total device loss) bootstraps its chain from
            // the genesis a survivor serves, then drains whatever else arrived
            // first; a non-genesis block is buffered until the genesis lands
            if block.height == 0 {
                self.adopt_chain(vec![block]);
                if self.chain_head.is_some() {
                    self.drain_buffered_blocks();
                    self.persist_chain_now();
                }
            } else {
                // L3: headless too, the buffer is size-capped (no head to
                // window against) — shed the highest, the re-serve re-earns
                self.pending_blocks.insert(block.height, block);
                while self.pending_blocks.len()
                    > usize::try_from(CATCHUP_BUFFER_WINDOW).unwrap_or(usize::MAX)
                {
                    if let Some(top) = self.pending_blocks.keys().next_back().copied() {
                        self.pending_blocks.remove(&top);
                    } else {
                        break;
                    }
                }
                // WP4b: with a served blob stashed, the buffered block may
                // be the missing anchor/suffix piece
                self.try_adopt_from_blob();
            }
            return;
        };
        if block.height == head.height + 1 {
            if self.apply_next_block(block) {
                // the buffered suffix drains behind it — ONE write for the
                // whole batch, at the end
                self.drain_buffered_blocks();
                self.persist_chain_now();
            }
        } else if block.height <= head.height {
            self.tie_break(block);
        } else {
            // a gap: we are behind. Buffer this block and ask the mesh for the
            // blocks we are missing (any survivor re-serves them). L3: only
            // heights the drain could ever reach are buffered (contiguous
            // upward from head+1, or the stashed blob's re-anchor run), and
            // the buffer is capped — when full the HIGHEST height is shed
            // (furthest from applicable; a re-served suffix re-earns it).
            let anchor_ok = self
                .pending_served_blob
                .as_ref()
                .is_some_and(|blob| {
                    block.height > blob.upto
                        && block.height <= blob.upto.saturating_add(CATCHUP_BUFFER_WINDOW)
                });
            if block.height > head.height.saturating_add(CATCHUP_BUFFER_WINDOW) && !anchor_ok {
                tracing::warn!(height = block.height, head = head.height, "refusing to buffer a block far past the head");
                return;
            }
            self.pending_blocks.retain(|h, _| *h > head.height);
            self.pending_blocks.insert(block.height, block);
            while self.pending_blocks.len() > usize::try_from(CATCHUP_BUFFER_WINDOW).unwrap_or(usize::MAX) {
                if let Some(top) = self.pending_blocks.keys().next_back().copied() {
                    self.pending_blocks.remove(&top);
                } else {
                    break;
                }
            }
            self.try_adopt_from_blob();
            self.request_catchup(head.height + 1);
        }
    }

    /// Verify a block against the current head, append + apply it, and run the
    /// post-apply bookkeeping. Returns whether it was accepted.
    fn apply_next_block(&mut self, block: ChainBlock) -> bool {
        // no probe clone: `append_committed_block` verifies before it appends,
        // so an unverifiable block never touches the chain. The probe used to
        // verify the whole chain a SECOND time per block — an exact doubling
        // of the catch-up cost that bought nothing.
        if self.append_committed_block(block.clone()) {
            self.after_block_applied(&block);
            // the head advanced — a catch-up request that reached this height is done
            if self.catchup_from.is_some_and(|f| f <= block.height) {
                self.catchup_from = None;
            }
            true
        } else {
            false
        }
    }

    /// Apply buffered catch-up blocks while the next height is available, then
    /// drop any stale buffered blocks at or below the head.
    fn drain_buffered_blocks(&mut self) {
        while let Some(head) = self.chain_head.clone() {
            let next = head.height + 1;
            let Some(block) = self.pending_blocks.remove(&next) else {
                break;
            };
            if !self.apply_next_block(block) {
                break;
            }
        }
        let head_h = self.chain_head.as_ref().map_or(0, |h| h.height);
        self.pending_blocks.retain(|h, _| *h > head_h);
    }

    /// Broadcast a catch-up request for every block from `from` onward (deduped
    /// while the same gap is outstanding). No-op if we cannot be behind.
    pub(crate) fn request_catchup(&mut self, from: u64) {
        if self.chain_head.is_none() || self.catchup_from == Some(from) {
            return;
        }
        self.catchup_from = Some(from);
        let me = self.member();
        tracing::debug!(me = %me, from, "chain catch-up requested");
        let env = self.make_env(me, WorkspaceEvent::ChainRequest { from_height: from });
        self.record(env);
    }

    /// Serve a peer's catch-up request from our OWN chain: re-broadcast every
    /// block we hold from `from` onward (as `Committed`, re-authored so the
    /// outbox fans it out). A single survivor thus reconstitutes the chain for
    /// everyone — independent of who originally committed each block.
    pub(crate) fn serve_chain_from(&mut self, from: u64) {
        let blocks: Vec<ChainBlock> = self
            .chain
            .iter()
            .filter(|b| b.height >= from)
            .cloned()
            .collect();
        tracing::debug!(me = %self.member(), from, served = blocks.len(), "serving chain catch-up");
        if blocks.is_empty() {
            return;
        }
        let me = self.member();
        // WP4b: a pruned holder cannot serve below its anchor — it serves
        // the BLOB instead, ahead of the anchor/suffix, so the requester
        // can hard-verify and re-anchor (suffix rules)
        if let (Some(blob), Some(anchor)) = (&self.checkpoint_blob, self.chain.first()) {
            // strictly below: a requester missing only the anchor block can
            // verify it against its own history — the full-state blob would
            // be pure fan-out amplification
            if from < anchor.height {
                let env = self.make_env(
                    me.clone(),
                    WorkspaceEvent::CheckpointServed { blob: blob.clone() },
                );
                self.record(env);
            }
        }
        for block in blocks {
            let env = self.make_env(me.clone(), WorkspaceEvent::Committed(block));
            self.record(env);
        }
    }

    /// The **smallest prefix that verifies standalone** — what a coordinator
    /// hands a rejoiner so it can materialize a workspace at all.
    ///
    /// Not the chain: one `set_image` block exceeds the gift-wrap cap
    /// (`welcome_chain_budget.rs`), so "the chain fits" is one proposal away
    /// from false, forever. Not a bare head either: `verify_chain` is
    /// all-or-nothing from the anchor, so a head without its chain is an
    /// unverified claim, and a headless node drops every block served to it
    /// (`is_chain_governed()` gates the ingest).
    ///
    /// So: this holder's `chain[0]` — the genesis, or after a compaction the
    /// checkpoint anchor block with the blob that roots it, since by then no
    /// node anywhere still holds a genesis. Everything above arrives over the
    /// ordinary catch-up, once the rejoiner has a head and asking works.
    pub(crate) fn anchor_bootstrap(&self) -> Vec<WorkspaceEvent> {
        let Some(anchor) = self.chain.first() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(blob) = &self.checkpoint_blob {
            out.push(WorkspaceEvent::CheckpointServed { blob: blob.clone() });
        }
        out.push(WorkspaceEvent::Committed(anchor.clone()));
        out
    }

    /// Broadcast [`State::anchor_bootstrap`]. The coordinator pushes this
    /// right after a recovery Welcome, because a rejoiner cannot ASK: it has
    /// no workspace to record a `ChainRequest` from yet.
    ///
    /// **No production caller YET, deliberately.** Its one call site is the
    /// Nostr arm of [`State::coordinator_rekey`], which does not exist:
    /// `restore_member_on_group` reads `NetRuntime::real_crypto`, and a Nostr
    /// workspace has no `NetRuntime` at all — its group MLS lives on
    /// `GroupNet`. So today that path logs "no runtime MLS group to re-key"
    /// and sends nothing. Wiring this into the `else` of the QUEUE branch
    /// would have been an unreachable branch dressed as a feature; the caller
    /// lands with the Nostr re-key, and the offer is pinned meanwhile by
    /// `the_served_anchor_is_the_smallest_prefix_that_verifies`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn serve_chain_anchor(&mut self) {
        let me = self.member();
        for ev in self.anchor_bootstrap() {
            let env = self.make_env(me.clone(), ev);
            self.record(env);
        }
    }

    /// WP4b: a served blob arrives ahead of its anchor. Stash it (runtime
    /// only) after the cheap forgery check — the REAL verification happens
    /// in [`State::try_adopt_from_blob`] once the anchor block is here.
    pub(crate) fn receive_checkpoint_blob(&mut self, blob: molt_core::CheckpointState) {
        // only useful when we are strictly BEHIND the served cut (head ==
        // upto means only the anchor is missing — the normal apply path
        // covers that without the full-state blob)
        let behind = match &self.chain_head {
            None => true,
            Some(head) => head.height < blob.upto,
        };
        if !behind {
            return;
        }
        // first stash wins until it is consumed or invalidated — an
        // overwritable slot would let one insider race garbage over a
        // legitimate blob forever (griefing; per-peer stashes are the
        // fuller fix, doc §B.6)
        if self.pending_served_blob.is_some() {
            return;
        }
        let rid = molt_storage::republic_id(
            &blob.founding_name,
            blob.rule_m,
            blob.rule_n,
            &blob.founding_identities,
        );
        if rid != self.republic_id() || rid != blob.republic_id {
            tracing::warn!("dropping a served checkpoint blob that does not recompute to this republic");
            return;
        }
        self.pending_served_blob = Some(blob);
        self.try_adopt_from_blob();
    }

    /// Adopt blob + buffered anchor/suffix once both are here: build the
    /// longest consecutive candidate from the buffer and run the FULL
    /// suffix verification — all-or-nothing, nothing is trusted from the
    /// stash until it passes.
    pub(crate) fn try_adopt_from_blob(&mut self) {
        let Some(blob) = self.pending_served_blob.clone() else {
            return;
        };
        // the chain advanced past the cut through the normal apply path —
        // the stash is dead weight now
        if self
            .chain_head
            .as_ref()
            .is_some_and(|h| h.height > blob.upto)
        {
            self.pending_served_blob = None;
            return;
        }
        // an attacker-served blob.upto could be u64::MAX; a saturating add
        // makes the lookup miss rather than overflow (overflow-checks abort)
        let Some(anchor_height) = blob.upto.checked_add(1) else {
            self.pending_served_blob = None;
            return;
        };
        if !self.pending_blocks.contains_key(&anchor_height) {
            return;
        }
        let mut candidate = Vec::new();
        let mut h = anchor_height;
        while let Some(b) = self.pending_blocks.get(&h) {
            candidate.push(b.clone());
            let Some(next) = h.checked_add(1) else { break };
            h = next;
        }
        match verify_suffix_chain(&blob, &candidate, &self.republic_id()) {
            Ok(head) => {
                let new_height = head.height;
                self.set_checkpoint_blob(Some(blob));
                self.chain = candidate.clone();
                self.chain_head = Some(head);
                self.pending_served_blob = None;
                self.pending_blocks.retain(|h, _| *h > new_height);
                self.apply_chain_to_state();
                // the post-apply bookkeeping the block-by-block path runs in
                // after_block_applied: sealed proposals get their terminal
                // state + event, PRE-CUT consumed proposals resolve too
                // (else they zombie as Proposed and re-base re-signs them
                // into dead gossip — review finding), org effects refresh,
                // a Restored seat's stale announce-cooldown clears, and
                // stale signatures re-base once at the end
                let consumed: Vec<u64> = self
                    .checkpoint_blob
                    .as_ref()
                    .map(|b| b.consumed_ids.clone())
                    .unwrap_or_default();
                for id in consumed {
                    if let Some(p) = self.proposals.get_mut(&id) {
                        if p.state == ProposalState::Proposed {
                            p.state = ProposalState::Applied;
                        }
                    }
                    self.stash_voted(id);
                    self.pending_sigs.remove(&id);
                }
                let mut org_touched = false;
                for block in &candidate {
                    match &block.change {
                        ChainChange::Applied {
                            proposal_id,
                            surface,
                            ..
                        } => {
                            if let Some(p) = self.proposals.get_mut(proposal_id) {
                                p.state = ProposalState::Applied;
                            }
                            self.stash_voted(*proposal_id);
                            self.pending_sigs.remove(proposal_id);
                            self.emit(Event::Applied {
                                id: ProposalId(*proposal_id),
                                surface: *surface,
                            });
                            if *surface == Surface::Organization {
                                org_touched = true;
                            }
                        }
                        ChainChange::Membership {
                            op: MembershipOp::Restored,
                            member,
                            ..
                        } => {
                            self.mesh_extension_at.remove(member);
                        }
                        _ => {}
                    }
                }
                if org_touched {
                    self.after_org_applied();
                }
                self.rebase_pending_approvals();
                self.persist_chain_now();
                if self.catchup_from.is_some_and(|f| f <= new_height) {
                    self.catchup_from = None;
                }
                tracing::info!(height = new_height, "re-anchored on a served checkpoint");
            }
            Err(e) => {
                // drop THIS stash so a later honest re-serve can land — a
                // failed pairing must not wedge the slot forever
                self.pending_served_blob = None;
                tracing::warn!(error = %e, "served checkpoint blob + suffix do not verify — stash cleared");
            }
        }
    }

    /// The event bodies a catch-up answer re-gossips (WP2): per OPEN surface
    /// proposal a regular `Proposed` plus every already-collected `Approved`
    /// signature — verbatim and position-bound (`(id, by, height, sig)`),
    /// nothing is re-signed. Pure so the unit test pins the batch;
    /// [`State::serve_open_governance`] puts it on the wire. Membership
    /// proposals (recovery) are deliberately absent: their window is
    /// mesh-liveness-bound and their tickets are in-memory by design.
    pub(crate) fn open_governance_events(&self) -> Vec<WorkspaceEvent> {
        let mut events = Vec::new();
        let mut open: Vec<(&u64, &molt_core::ProposalRecord)> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.state == ProposalState::Proposed)
            // membership records stay out (their window is liveness-bound, see
            // the doc above) — re-serving one as a plain `Proposed` would make
            // receivers register a SURFACE change under the membership id and
            // sign different bytes than everyone else
            .filter(|(id, _)| {
                !matches!(
                    self.proposal_changes.get(id),
                    Some(ChainChange::Membership { .. })
                )
            })
            .collect();
        // deterministic order (the map is a HashMap): by id
        open.sort_by_key(|(id, _)| **id);
        for (id, p) in open {
            events.push(WorkspaceEvent::Proposed {
                id: ProposalId(*id),
                surface: p.surface,
                payload: p.payload.clone(),
            });
            if let Some(pending) = self.pending_sigs.get(id) {
                // L2: only VERIFIED signatures are re-served — junk a peer
                // once gossiped must not be amplified to the next node
                for a in pending.sigs.iter().filter(|a| pending.verified.contains(&a.member)) {
                    events.push(WorkspaceEvent::Approved {
                        id: ProposalId(*id),
                        by: a.member.clone(),
                        height: pending.height,
                        sig: a.sig.clone(),
                    });
                }
            }
        }
        // the OWN declines, and only those: a decline carries no signature,
        // so the link identity is the only mouth it may come out of — a
        // foreign decline is never re-attested. Served for open cards, for
        // REJECTED cards (the terminal state is gossip-derived; a peer that
        // missed the vote would keep it open forever) and for parked voices
        // (the own log replayed a decline whose proposal is not back yet).
        // A receiver without the card parks the voice symmetrically.
        // the OWN withdraw re-serves like the own declines below: a peer
        // that was closed while the proposer pulled back must still learn
        // the verdict (same retention gate as the rejected declines)
        let me = self.member();
        let cutoff = self.chat_retention_cutoff();
        let mut own_withdrawn: Vec<u64> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.withdrawn && p.by == me && p.declined_at >= cutoff)
            .map(|(id, _)| *id)
            .collect();
        own_withdrawn.sort_unstable();
        for id in own_withdrawn {
            events.push(WorkspaceEvent::Withdrawn {
                id: ProposalId(id),
                by: me.clone(),
            });
        }
        let mut declined: Vec<(u64, String)> = self
            .proposals
            .iter()
            .filter(|(id, p)| {
                // the same Membership exclusion as the Proposed loop above —
                // a membership id must never re-serve, in any clothing
                !matches!(
                    self.proposal_changes.get(id),
                    Some(ChainChange::Membership { .. })
                ) && p.decliners.iter().any(|d| d == &me)
                    && match p.state {
                        ProposalState::Proposed => true,
                        // a rejected card re-serves only while some view
                        // still shows it: past the display retention it has
                        // no convergence audience, and the batch stays
                        // bounded instead of growing with the republic's
                        // whole rejected history
                        ProposalState::Rejected => !Self::aged_out_at(cutoff, p.declined_at),
                        _ => false,
                    }
            })
            // a registered voice recomputes its anchor from the own record;
            // a parked voice re-serves the hash it ARRIVED with (D1)
            .map(|(id, p)| (*id, crate::State::decline_payload_hash(&p.payload)))
            .chain(self.pending_declines.iter().filter_map(|(id, parked)| {
                parked
                    .iter()
                    .find(|(m, _, _)| m == &me)
                    .map(|(_, _, h)| (*id, h.clone()))
            }))
            .collect();
        declined.sort_unstable();
        declined.dedup();
        for (id, hash) in declined {
            events.push(WorkspaceEvent::Declined {
                id: ProposalId(id),
                by: me.clone(),
                hash,
            });
        }
        events
    }

    /// Answer a peer's catch-up request with the OPEN governance state, the
    /// ephemeral twin of [`State::serve_chain_from`]: a reopened member lost
    /// the Proposed/Approved gossip with its RAM (deliberately unpersisted —
    /// the chain's ephemeral-until-block boundary), so whoever serves the
    /// chain suffix re-serves the open proposals too. Re-gossip of identical
    /// events is idempotent on every receiver (`receive_proposed` or-inserts,
    /// `collect_sig` keeps one signature per member, `try_commit` refuses
    /// decided proposals), so several answering peers converge harmlessly.
    pub(crate) fn serve_open_governance(&mut self) {
        let events = self.open_governance_events();
        let me = self.member();
        for body in events {
            let env = self.make_env(me.clone(), body);
            self.record(env);
        }
    }

    /// Resolve a competing block at a slot we already filled: identical block →
    /// a duplicate broadcast, ignore; a different block at the tip with a
    /// smaller hash wins the single branch, so adopt it and re-base the
    /// displaced proposal. A deeper conflict is logged (deep reorg is Phase 3).
    fn tie_break(&mut self, block: ChainBlock) {
        let Some(existing) = self.chain.iter().find(|b| b.height == block.height) else {
            return;
        };
        if existing == &block {
            return; // duplicate broadcast of the block we already hold
        }
        let rid = self.republic_id();
        let incoming = molt_storage::content_hash(&block_link_bytes(&rid, &block));
        let current = molt_storage::content_hash(&block_link_bytes(&rid, existing));
        let is_tip = self.chain.last().is_some_and(|b| b.height == block.height);
        if is_tip && incoming < current {
            // the incoming block wins the tip; swap it in and re-verify
            let displaced = self.chain.pop();
            self.chain.push(block.clone());
            if let Ok(head) = self.verify_own(&self.chain) {
                self.chain_head = Some(head);
                self.apply_chain_to_state();
                self.persist_chain_now();
                // the displaced proposal returns to pending and re-bases —
                // but ONLY a card with a deliberation behind it (a proposer
                // this holder learned via gossip). A record MATERIALIZED
                // from the now-displaced block (`ensure_applied_record`,
                // by == "") has no vote to return to here: flipping it open
                // would mint an unowned, unwithdrawable phantom card that
                // re-gossips forever and blocks auto-checkpoints. Drop it —
                // the holder returns to "never heard of it", and the WP2
                // re-serve restores the real card while the vote is open.
                if let Some(ChainChange::Applied { proposal_id, .. }) =
                    displaced.as_ref().map(|b| &b.change)
                {
                    let materialized = self
                        .proposals
                        .get(proposal_id)
                        .is_some_and(|p| p.by.is_empty());
                    if materialized {
                        self.proposals.remove(proposal_id);
                    } else if let Some(p) = self.proposals.get_mut(proposal_id) {
                        p.state = ProposalState::Proposed;
                    }
                }
                self.after_block_applied(&block);
            } else {
                // revert — should not happen for a verified block
                self.chain.pop();
                if let Some(b) = displaced {
                    self.chain.push(b);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::{ChainChange, MembershipOp, Surface};
    use molt_storage::{derive_identity_key, identity_sign, SigningKey};
    use serde_json::json;

    /// A minimal chain builder: derives each member's identity key from a seed,
    /// seals the genesis with everyone (n-of-n) and appends later blocks signed
    /// by a chosen subset — exactly what the real founding + threshold path
    /// will produce.
    #[derive(Clone)]
    struct Builder {
        republic_id: String,
        keys: Vec<(String, SigningKey)>,
        blocks: Vec<ChainBlock>,
        head_hash: String,
    }

    impl Builder {
        fn new(members: &[&str], rule_m: u8) -> Builder {
            Builder::new_on_relays(members, rule_m, Vec::new())
        }

        /// A founding whose genesis ratifies `relays` (R3b ledger tests).
        fn new_on_relays(members: &[&str], rule_m: u8, relays: Vec<String>) -> Builder {
            let mut keys: Vec<(String, SigningKey)> = Vec::new();
            let mut identities: Vec<MemberIdentity> = Vec::new();
            for (i, m) in members.iter().enumerate() {
                let seed = [u8::try_from(i + 1).unwrap_or(1); 32];
                let (sk, pk) = derive_identity_key(&seed, m);
                identities.push(MemberIdentity {
                    member: (*m).to_string(),
                    identity_pk: pk,
                    nostr_pk: "cc".repeat(32),
                });
                keys.push(((*m).to_string(), sk));
            }
            let rule_n = u8::try_from(members.len()).expect("small roster");
            let republic_id = molt_storage::republic_id("Chess Club", rule_m, rule_n, &identities);
            let change = ChainChange::Genesis {
                name: "Chess Club".to_string(),
                republic_id: republic_id.clone(),
                rule_m,
                rule_n,
                identities: identities.clone(),
                agenda: "play chess".to_string(),
                features: None,
                relays,
            };
            let mut b = Builder {
                republic_id: republic_id.clone(),
                keys,
                blocks: Vec::new(),
                head_hash: GENESIS_PREV.to_string(),
            };
            // genesis is unanimous
            let all: Vec<&str> = members.to_vec();
            let block = b.seal(0, change, &all);
            b.push(block);
            b
        }

        /// Sign `change` at `height` with each named member and return the block.
        fn seal(&self, height: u64, change: ChainChange, signers: &[&str]) -> ChainBlock {
            let bytes = approval_bytes(&self.republic_id, height, &change);
            let sigs = signers
                .iter()
                .map(|name| {
                    let (_, sk) = self
                        .keys
                        .iter()
                        .find(|(m, _)| m == name)
                        .expect("known signer");
                    RosterAttestation {
                        member: (*name).to_string(),
                        sig: identity_sign(sk, &bytes),
                    }
                })
                .collect();
            ChainBlock {
                height,
                prev: self.head_hash.clone(),
                change,
                sigs,
            }
        }

        fn push(&mut self, block: ChainBlock) {
            self.head_hash = block_hash(&self.republic_id, &block);
            self.blocks.push(block);
        }

        /// Commit a gated Applied change signed by `signers` at the next height.
        fn commit_applied(&mut self, proposal_id: u64, signers: &[&str]) {
            let height = u64::try_from(self.blocks.len()).expect("small chain");
            let change = ChainChange::Applied {
                proposal_id,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": proposal_id }),
            };
            let block = self.seal(height, change, signers);
            self.push(block);
        }

        /// Commit an Organization edit — the surface whose ops occupy
        /// last-write-wins slots (§B.6a), so a checkpoint summarizes them.
        fn commit_org(&mut self, proposal_id: u64, op: &str, value: &str, signers: &[&str]) {
            let height = u64::try_from(self.blocks.len()).expect("small chain");
            let change = ChainChange::Applied {
                proposal_id,
                surface: Surface::Organization,
                payload: json!({ "op": op, "value": value }),
            };
            let block = self.seal(height, change, signers);
            self.push(block);
        }

        /// Commit a `Restored` membership block — the seat keeps its anchored
        /// identity key and re-anchors its transport key.
        fn commit_restored(&mut self, member: &str, nostr_pk: &str, signers: &[&str]) {
            let height = u64::try_from(self.blocks.len()).expect("small chain");
            let change = ChainChange::Membership {
                op: MembershipOp::Restored,
                member: member.to_string(),
                identity_pk: self.pk(member),
                nostr_pk: Some(nostr_pk.to_string()),
                relays: Vec::new(),
                consent: None,
            };
            let block = self.seal(height, change, signers);
            self.push(block);
        }

        /// A member's signing key.
        fn key(&self, member: &str) -> &SigningKey {
            &self
                .keys
                .iter()
                .find(|(m, _)| m == member)
                .expect("known member")
                .1
        }

        /// A member's anchored identity pk (from the genesis roster).
        fn pk(&self, member: &str) -> String {
            let ChainChange::Genesis { identities, .. } = &self.blocks[0].change else {
                panic!("block 0 is not a genesis");
            };
            identities
                .iter()
                .find(|i| i.member == member)
                .expect("anchored member")
                .identity_pk
                .clone()
        }
    }

    #[test]
    fn genesis_then_applied_verifies() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let head = verify_chain(&b.blocks).expect("valid chain verifies");
        assert_eq!(head.height, 1);
        assert_eq!(head.rule_m, 2);
        assert_eq!(head.identities.len(), 3);
    }

    #[test]
    fn a_tampered_payload_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        // rewrite the applied payload without re-signing
        if let ChainChange::Applied { payload, .. } = &mut b.blocks[1].change {
            *payload = json!({ "op": "add_note", "id": 999 });
        }
        assert!(verify_chain(&b.blocks).is_err(), "signatures cover the payload");
    }

    #[test]
    fn a_broken_prev_link_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.blocks[1].prev = GENESIS_PREV.to_string();
        assert!(verify_chain(&b.blocks).is_err(), "the chain link is broken");
    }

    #[test]
    fn below_threshold_approvals_are_rejected() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra"]); // only 1 of the required 2
        assert!(verify_chain(&b.blocks).is_err(), "one approval is below m=2");
    }

    #[test]
    fn a_repeated_signature_does_not_reach_threshold() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        // petra signs, then her attestation is duplicated — still one signer
        b.commit_applied(1, &["petra"]);
        let dup = b.blocks[1].sigs[0].clone();
        b.blocks[1].sigs.push(dup);
        assert!(
            verify_chain(&b.blocks).is_err(),
            "one member signing twice is still one approver"
        );
    }

    #[test]
    fn applying_a_proposal_twice_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(7, &["petra", "walter"]);
        b.commit_applied(7, &["petra", "walter"]); // same proposal id again
        assert!(verify_chain(&b.blocks).is_err(), "no double-apply");
    }

    #[test]
    fn a_height_gap_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.blocks[1].height = 5; // signatures are height-bound, so this also fails the sig check
        assert!(verify_chain(&b.blocks).is_err(), "heights must be gapless");
    }

    #[test]
    fn a_forged_genesis_id_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        if let ChainChange::Genesis { republic_id, .. } = &mut b.blocks[0].change {
            *republic_id = "deadbeef".to_string();
        }
        assert!(
            verify_chain(&b.blocks).is_err(),
            "the republic id must match the roster content"
        );
    }

    #[test]
    fn a_membership_block_grows_the_roster_and_lets_the_newcomer_approve() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        // add dora with her own derived identity key
        let (dora_sk, dora_pk) = derive_identity_key(&[9u8; 32], "dora");
        let height = u64::try_from(b.blocks.len()).expect("small chain");
        let join = ChainChange::Membership {
            op: MembershipOp::Joined,
            member: "dora".to_string(),
            identity_pk: dora_pk,
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        };
        let block = b.seal(height, join, &["petra", "walter"]);
        b.push(block);
        b.keys.push(("dora".to_string(), dora_sk));
        // now an Applied block signed by dora + walter must count dora
        b.commit_applied(1, &["dora", "walter"]);
        let head = verify_chain(&b.blocks).expect("newcomer approval counts");
        assert_eq!(head.identities.len(), 3);
        assert_eq!(head.height, 2);
    }

    /// A member that only holds the genesis receives a peer's broadcast commit
    /// block, verifies + adopts it, and its persistent state converges (the
    /// `receive_block` path that a non-committer follows).
    /// WP1: the chain projection feeds the snapshot's parallel id track —
    /// a committed `Applied` block's `proposal_id` reaches the read contract
    /// positionally next to its payload.
    /// N4b step 3 — the WORKING transport anchor is a chain projection.
    ///
    /// A recovered seat's key changes; the roster's genesis anchor does not
    /// (it is the immutable founding record). Every gift-wrap send must
    /// resolve through the projection, because a sender reaching for the
    /// obvious `identities[i].nostr_pk` would address a key the recovered
    /// member no longer holds — SILENTLY, which is exactly why the plan
    /// rejected "infer the anchor from live traffic".
    #[test]
    fn the_working_anchor_follows_a_restored_block_while_the_roster_does_not() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut st = crate::tests::plain_state();
        st.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "walter".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: "play chess".to_string(),
            features: None,
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        st.adopt_chain(b.blocks.clone());
        let founding = st
            .replica
            .as_ref()
            .and_then(|r| r.identities.iter().find(|i| i.member == "petra"))
            .map(|i| i.nostr_pk.clone())
            .expect("petra is anchored");
        assert_eq!(
            st.working_nostr_pk("petra"),
            founding,
            "before any recovery the projection IS the roster anchor"
        );

        // petra recovers with a FRESH transport key
        let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
        let restored = b.seal(
            1,
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member: "petra".to_string(),
                identity_pk: b.pk("petra"),
                nostr_pk: Some(fresh.clone()),
                relays: Vec::new(),
                consent: None,
            },
            &["petra", "walter"],
        );
        st.adopt_chain({
            let mut c = b.blocks.clone();
            c.push(restored);
            c
        });

        assert_eq!(
            st.working_nostr_pk("petra"),
            fresh,
            "after a Restored block the projection returns the NEW key"
        );
        assert_eq!(
            st.replica
                .as_ref()
                .and_then(|r| r.identities.iter().find(|i| i.member == "petra"))
                .map(|i| i.nostr_pk.clone())
                .expect("still anchored"),
            founding,
            "…while the roster keeps the immutable FOUNDING anchor"
        );
        // an unknown member resolves to nothing, never to somebody else's key
        assert_eq!(st.working_nostr_pk("nobody"), "");
    }

    /// **…and it survives the compaction that drops the block it came from.**
    ///
    /// The `Restored` block is what re-anchors a seat, and a cut drops it.
    /// The roster in the blob keeps the FOUNDING anchor by design
    /// (`apply_membership` refuses to move a seat's identity key), so without
    /// the summary carrying the working anchors explicitly, a compaction
    /// makes every recovered member addressable ONLY at the key it no longer
    /// holds — silently, which is the exact failure `State::chain_anchors`
    /// documents itself as existing to prevent.
    ///
    /// Reachable in the ordinary course: `AUTO_CHECKPOINT_MIN_LEN` is 32.
    #[test]
    fn a_compaction_keeps_the_working_anchor_of_a_recovered_seat() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
        b.commit_restored("petra", &fresh, &["petra", "walter"]);

        // cut ABOVE the Restored block, so the block itself is dropped
        let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
        assert_eq!(
            blob.anchors,
            vec![("petra".to_string(), fresh.clone())],
            "the summary must carry the anchors the dropped blocks established"
        );
        let cut = b.seal(
            2,
            ChainChange::Checkpoint {
                upto: 1,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(cut);

        // the pruned holder: blob + the suffix from the anchor block on
        let mut st = crate::tests::plain_state();
        st.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "walter".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: "play chess".to_string(),
            features: None,
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        st.set_checkpoint_blob(Some(blob));
        st.adopt_chain(b.blocks[2..].to_vec());

        assert_eq!(
            st.working_nostr_pk("petra"),
            fresh,
            "after the cut the seat is addressable only at the key it no longer holds"
        );
    }

    /// R3b — the relay LEDGER: every member's chain answers "which relays is
    /// this seat on record as reaching". A founding member is covered by the
    /// ratified genesis pool; a restored seat's threshold-signed declaration
    /// overrides it — for EVERY member reading the same chain; and a
    /// compaction cut must not forget a declaration (checkpoint-v6), because
    /// split detection (R4) runs on exactly this data.
    #[test]
    fn the_ledger_reports_declared_relays_and_survives_a_cut() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool.clone());
        let rid = b.republic_id.clone();
        let replica = move || {
            Some(crate::ReplicaState {
                name: "Chess Club".to_string(),
                member: "walter".to_string(),
                roster: vec!["petra".to_string(), "walter".to_string()],
                rule_m: 2,
                identities: Vec::new(),
                agenda: "play chess".to_string(),
                features: None,
                republic_id: rid.clone(),
                founded_ts: 0,
            })
        };
        let mut st = crate::tests::plain_state();
        st.replica = replica();
        st.adopt_chain(b.blocks.clone());
        assert_eq!(
            st.member_relays("walter"),
            pool,
            "a founding member is covered by the ratified pool"
        );

        // petra re-joins over a DIFFERENT relay and declares it in the block
        let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
        let declared = vec!["wss://relay.two.example".to_string()];
        let restored = b.seal(
            1,
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member: "petra".to_string(),
                identity_pk: b.pk("petra"),
                nostr_pk: Some(fresh),
                relays: declared.clone(),
                consent: None,
            },
            &["petra", "walter"],
        );
        b.push(restored);
        st.adopt_chain(b.blocks.clone());
        assert_eq!(
            st.member_relays("petra"),
            declared,
            "every member's ledger reports the declared pool"
        );
        assert_eq!(st.member_relays("walter"), pool, "the others stay on the ratified pool");

        // cut ABOVE the Restored block: the declaration must ride the summary
        let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
        assert_eq!(
            blob.member_relays,
            vec![("petra".to_string(), declared.clone())],
            "the summary carries the declarations the dropped blocks established"
        );
        let cut = b.seal(
            2,
            ChainChange::Checkpoint { upto: 1, state_hash: checkpoint_state_hash(&blob) },
            &["petra", "walter"],
        );
        b.push(cut);
        let mut pruned = crate::tests::plain_state();
        pruned.replica = replica();
        pruned.set_checkpoint_blob(Some(blob));
        pruned.adopt_chain(b.blocks[2..].to_vec());
        assert_eq!(
            pruned.member_relays("petra"),
            declared,
            "a cut must not forget a declaration"
        );
        assert_eq!(pruned.member_relays("walter"), pool, "…nor the ratified fallback");
    }

    /// R4 — split detection: two members whose effective relay sets are
    /// disjoint produce a verdict naming both, and the members surface says
    /// so per member — compactly, with the relay that would bridge — rather
    /// than staying a silence while the threshold quietly cannot assemble.
    #[test]
    fn disjoint_relay_sets_produce_a_split_verdict_naming_the_bridge() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool.clone());
        let rid = b.republic_id.clone();
        let mut st = crate::tests::plain_state();
        st.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "walter".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: "play chess".to_string(),
            features: None,
            republic_id: rid,
            founded_ts: 0,
        });
        st.adopt_chain(b.blocks.clone());
        assert!(st.relay_splits().is_empty(), "one shared pool - no split");

        // petra re-joins over a relay NOBODY else carries
        let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
        let restored = b.seal(
            1,
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member: "petra".to_string(),
                identity_pk: b.pk("petra"),
                nostr_pk: Some(fresh),
                relays: vec!["wss://relay.two.example".to_string()],
                consent: None,
            },
            &["petra", "walter"],
        );
        b.push(restored);
        st.adopt_chain(b.blocks.clone());

        let splits = st.relay_splits();
        assert_eq!(
            splits,
            vec![("petra".to_string(), "walter".to_string())],
            "the verdict names both seats"
        );
        // …and the members surface carries the marker, naming the bridge
        let view = st.members_view();
        let row = |m: &str| {
            view.iter()
                .find(|v| v.member == m)
                .unwrap_or_else(|| panic!("{m} row"))
                .split
                .clone()
        };
        assert!(
            row("petra").contains("walter") && row("petra").contains("wss://relay.two.example"),
            "petra's marker names the counterpart and her odd relay: {:?}",
            row("petra")
        );
        assert!(
            row("walter").contains("petra") && row("walter").contains("wss://relay.one"),
            "walter's marker mirrors it: {:?}",
            row("walter")
        );
    }

    #[test]
    fn chain_applied_entries_carry_their_proposal_id() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(7, &["petra", "walter"]);
        let mut peer = crate::tests::plain_state();
        peer.adopt_chain(b.blocks.clone());
        let snap = peer.snapshot(Surface::Memory, None, None);
        assert_eq!(snap.applied.len(), 1, "one committed Applied block");
        assert_eq!(
            snap.applied_ids,
            vec![Some(7)],
            "the block's proposal id rides the id track"
        );
    }

    /// The deliberation gossip is ephemeral RAM on every RECEIVER — only the
    /// proposer's own log records `Proposed`. A holder that adopts an
    /// Applied block without the card (reopen replay, catch-up past lost
    /// gossip) must materialize the record FROM the block: the chain
    /// carries payload and signers. Without it the Accepted view degraded
    /// to an id-less row — no voters, and the raw multi-line patch dumped
    /// into the value cell (field report 2026-08-16, the dev republic).
    #[test]
    fn an_adopted_applied_block_materializes_its_accepted_card() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        let block = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 4,
                surface: Surface::Memory,
                payload: json!({
                    "op": "wiki_patch",
                    "value": "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,1 @@\n+hi\n",
                    "summary": "+1 -0 →0 ~1",
                }),
            },
            &["petra", "walter"],
        );
        b.push(block);

        // a reopen: the chain comes from disk, the ephemeral proposal map
        // is empty (this peer never was the proposer)
        let mut peer = crate::tests::plain_state();
        peer.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "dora".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string(), "dora".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: String::new(),
            features: None,
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        peer.adopt_chain(b.blocks.clone());

        let snap = peer.snapshot(Surface::Memory, None, None);
        assert_eq!(snap.applied_ids, vec![Some(4)]);
        assert_eq!(snap.accepted.len(), 1, "the accepted card exists again");
        let card = &snap.accepted[0];
        assert_eq!(card.id.0, 4);
        assert_eq!(card.state, molt_core::ProposalState::Applied);
        assert_eq!(card.approvals, 2, "the sealed block's signer count");
        let approved: Vec<&str> = card
            .votes
            .iter()
            .filter(|v| v.vote == molt_core::VoteState::Approved)
            .map(|v| v.member.as_str())
            .collect();
        assert_eq!(approved, vec!["petra", "walter"], "who voted is chain-proven");
        assert_eq!(
            card.payload["op"],
            json!("wiki_patch"),
            "the payload keeps its shape (the GUI's patch rendering keys on it)"
        );

        // the LIVE twin: a broadcast block for a proposal this node never
        // heard of (its gossip was lost) materializes the card the same way
        let late = b.seal(
            2,
            ChainChange::Applied {
                proposal_id: 9,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "title": "minutes" }),
            },
            &["walter", "dora"],
        );
        peer.receive_block(late);
        let snap = peer.snapshot(Surface::Memory, None, None);
        assert_eq!(snap.accepted.len(), 2, "both cards stand");
        let card = snap
            .accepted
            .iter()
            .find(|c| c.id.0 == 9)
            .expect("the late block's card");
        assert_eq!(card.approvals, 2);
        assert!(
            card.votes
                .iter()
                .any(|v| v.member == "dora" && v.vote == molt_core::VoteState::Approved),
            "the live-adopted card names its signers too"
        );
    }

    /// KEYSTONE for `tie_break` (previously untested): two members seal
    /// competing blocks at the same height; the lower hash wins the tip.
    /// A record MATERIALIZED from the displaced block must VANISH with it
    /// (review 2026-08-16: flipping it to Proposed minted a permanent,
    /// unowned open card — unwithdrawable, re-gossiped forever, and it
    /// blocked auto-checkpoints on that holder).
    #[test]
    fn tie_break_drops_a_materialized_card_with_its_displaced_block() {
        let b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        let block_a = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 7,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "title": "a" }),
            },
            &["petra", "walter"],
        );
        let block_b = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 9,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "title": "b" }),
            },
            &["petra", "walter"],
        );
        let rid = b.republic_id.clone();
        let hash = |blk: &ChainBlock| molt_storage::content_hash(&block_link_bytes(&rid, blk));
        let (winner, loser) = if hash(&block_a) < hash(&block_b) {
            (block_a, block_b)
        } else {
            (block_b, block_a)
        };
        let (loser_id, winner_id) = match (&loser.change, &winner.change) {
            (
                ChainChange::Applied { proposal_id: l, .. },
                ChainChange::Applied { proposal_id: w, .. },
            ) => (*l, *w),
            _ => unreachable!("both are Applied"),
        };

        // the peer adopts the LOSER first (its card is materialized from
        // the block — this holder never saw the gossip), then the winner
        // arrives and takes the tip
        let mut peer = crate::tests::plain_state();
        peer.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "walter".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: String::new(),
            features: None,
            republic_id: rid.clone(),
            founded_ts: 0,
        });
        peer.adopt_chain(genesis);
        peer.receive_block(loser);
        assert_eq!(
            peer.proposals.get(&loser_id).map(|p| p.state),
            Some(ProposalState::Applied),
            "the loser's card is materialized while its block stands"
        );

        peer.receive_block(winner);
        assert_eq!(peer.chain.len(), 2);
        let tip = peer.chain.last().expect("tip");
        assert!(
            matches!(&tip.change, ChainChange::Applied { proposal_id, .. } if *proposal_id == winner_id),
            "the lower hash holds the tip"
        );
        assert!(
            !peer.proposals.contains_key(&loser_id),
            "the materialized card vanished with its displaced block — no phantom open card"
        );
        assert_eq!(
            peer.proposals.get(&winner_id).map(|p| p.state),
            Some(ProposalState::Applied),
            "the winner's card stands, chain-proven"
        );
    }

    #[test]
    fn a_peer_adopts_a_broadcast_block_and_converges() {
        let b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        // a block committed elsewhere: an Applied change signed by both members
        let change = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "title": "minutes" }),
        };
        let block = b.seal(1, change, &["petra", "walter"]);

        // walter holds only the genesis, then the block arrives over the mesh
        let mut peer = crate::tests::plain_state();
        peer.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "walter".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(), // adopt_chain fills these from the verified head
            agenda: "play chess".to_string(),
            features: None,
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        peer.adopt_chain(genesis);
        assert!(peer.is_chain_governed());
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 0);

        peer.receive_block(block);
        assert_eq!(peer.chain.len(), 2, "the peer adopted the broadcast block");
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 1);
        let applied = peer
            .chain_applied
            .get(&Surface::Memory)
            .expect("memory projection");
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].0, Some(1), "the projection keeps the proposal id");
        assert_eq!(applied[0].1["title"], json!("minutes"));

        // an invalid block (tampered payload, sigs no longer match) is rejected
        let mut forged = b.seal(
            2,
            ChainChange::Applied {
                proposal_id: 2,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "title": "real" }),
            },
            &["petra", "walter"],
        );
        forged.prev = peer.chain_head.as_ref().expect("head").hash.clone();
        if let ChainChange::Applied { payload, .. } = &mut forged.change {
            *payload = json!({ "op": "add_note", "title": "forged" });
        }
        peer.receive_block(forged);
        assert_eq!(peer.chain.len(), 2, "a tampered block is hard-rejected");
    }

    /// Attach real (temp-dir) storage to a test peer; `dead_writer` closes
    /// the writer first, so every blocking persist honestly reports `false`.
    fn attach_storage(peer: &mut crate::State, dead_writer: bool) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed =
            molt_storage::seed_entropy(&molt_storage::generate_seed_phrase().expect("phrase"))
                .expect("entropy");
        let genesis = molt_core::EventEnvelope {
            prev_seq: 0,
            seq: 1,
            ts: 10,
            by: "walter".to_string(),
            body: molt_core::WorkspaceEvent::Founded {
                name: "Chess Club".to_string(),
                rule_m: 2,
                rule_n: 2,
                member: "walter".to_string(),
                roster: vec!["petra".to_string(), "walter".to_string()],
                identities: Vec::new(),
                attestations: Vec::new(),
                republic_id: String::new(),
                agenda: String::new(),
                relays: Vec::new(),
                features: None,
            },
        };
        let ws = molt_storage::create_workspace(tmp.path(), &seed, &genesis).expect("create");
        let dir = ws.dir().to_path_buf();
        let handle = molt_storage::start_writer(ws);
        if dead_writer {
            handle.clone().close(None);
        }
        peer.active = Some(crate::ActiveStorage {
            id: "w-h3".to_string(),
            dir,
            prefs: molt_core::WorkspacePrefs::default(),
            handle,
        });
        tmp
    }

    /// **Known-debt refinement (2026-08-16 list): only a buffered block
    /// ADJACENT to head pins the auto-checkpoint.** The buffer accepts
    /// claims up to head+4096, so an insider posting one plausible
    /// near-future height used to freeze compaction until a drain or
    /// re-serve cleared it. A gap block cannot apply next — only head+1
    /// says the head is about to move and the cut would be stale.
    #[test]
    fn a_far_future_buffered_block_does_not_pin_the_auto_checkpoint() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        for i in 1..=32 {
            b.commit_applied(i, &["petra", "walter"]);
        }
        let head_h = b.blocks.last().expect("blocks").height;
        let mut dummy = b.blocks[1].clone();

        // a far-future claim in the buffer must NOT hold the compaction
        let mut peer = chain_peer("petra", &b, b.blocks.clone());
        dummy.height = head_h + 2;
        peer.pending_blocks.insert(head_h + 2, dummy.clone());
        peer.maybe_auto_checkpoint();
        assert!(
            peer.proposal_changes
                .values()
                .any(|c| matches!(c, ChainChange::Checkpoint { .. })),
            "a gap block cannot apply next — the cut must still be proposed"
        );

        // …but a block adjacent to head still pins it: the head is about
        // to move and the cut would be stale on arrival
        let mut peer = chain_peer("petra", &b, b.blocks.clone());
        dummy.height = head_h + 1;
        peer.pending_blocks.insert(head_h + 1, dummy);
        peer.maybe_auto_checkpoint();
        assert!(
            !peer
                .proposal_changes
                .values()
                .any(|c| matches!(c, ChainChange::Checkpoint { .. })),
            "an adjacent buffered block keeps pinning the auto-checkpoint"
        );
    }

    /// **H3 second half (total_review.md): the governance broadcast waits
    /// for the durable persist.** A threshold-sealed block whose write did
    /// NOT reach the disk is still appended and projected locally — the
    /// signatures are real — but it is NOT broadcast: no `Committed`
    /// envelope, no decision summary. The peers seal the byte-identical
    /// block from the approval gossip themselves; this node must not
    /// spread history it does not durably hold. A durable seal broadcasts
    /// exactly as before.
    #[test]
    fn a_block_that_missed_the_disk_is_not_broadcast() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        b.commit_applied(1, &["petra", "walter"]);
        let block = b.blocks[1].clone();

        // (1) the writer is gone — sealed, appended, NOT broadcast
        let mut peer = chain_peer("walter", &b, genesis.clone());
        let _tmp = attach_storage(&mut peer, true);
        let seq_before = peer.next_seq;
        let chat_before = peer.chat.len();
        peer.adopt_committed_block(block.clone(), 1);
        assert_eq!(peer.chain.len(), 2, "the sealed block is appended locally");
        assert_eq!(
            peer.next_seq, seq_before,
            "no envelope may be minted for a block the disk never took"
        );
        assert_eq!(peer.chat.len(), chat_before, "no decision summary either");

        // (2) the writer lives — durable, broadcast as before
        let mut peer = chain_peer("petra", &b, genesis);
        let _tmp = attach_storage(&mut peer, false);
        let seq_before = peer.next_seq;
        peer.adopt_committed_block(block, 1);
        assert_eq!(peer.chain.len(), 2);
        assert!(
            peer.next_seq > seq_before,
            "a durable seal broadcasts its Committed envelope"
        );
        peer.active.take().expect("active").handle.close(None);
    }

    /// A 2-member chain-governed peer holding only the genesis `b` roots.
    fn chain_peer(member: &str, b: &Builder, chain: Vec<ChainBlock>) -> crate::State {
        let mut peer = crate::tests::plain_state();
        peer.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: member.to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: "play chess".to_string(),
            features: None,
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        peer.adopt_chain(chain);
        peer
    }

    /// A block arriving ahead of our head is buffered, then applied once the
    /// gap fills — catch-up converges regardless of arrival order.
    #[test]
    fn out_of_order_blocks_buffer_and_converge() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let block1 = b.blocks[1].clone();
        let block2 = b.blocks[2].clone();

        let mut peer = chain_peer("walter", &b, genesis);
        // the height-2 block arrives first — a gap, so it is buffered
        peer.receive_block(block2);
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            0,
            "a gap block is buffered, not applied"
        );
        assert_eq!(peer.pending_blocks.len(), 1);
        // the height-1 block fills the gap; the buffered height-2 drains behind it
        peer.receive_block(block1);
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 2);
        assert!(peer.pending_blocks.is_empty(), "the buffer drained");
    }

    /// One survivor holding the full chain re-serves a lagging member the whole
    /// missing suffix — the resilience property (any survivor suffices), and the
    /// suffix applies even delivered out of order.
    #[test]
    fn a_survivor_serves_a_lagging_member_the_full_suffix() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let full = b.blocks.clone();

        let mut peer = chain_peer("walter", &b, genesis);
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 0);

        // the survivor serves every block from the peer's head+1 (=1) onward,
        // straight out of its own chain — exactly what serve_chain_from does
        let served: Vec<ChainBlock> = full.iter().filter(|bl| bl.height >= 1).cloned().collect();
        assert_eq!(served.len(), 2, "survivor serves b1 + b2 from its chain");
        for bl in served.into_iter().rev() {
            peer.receive_block(bl); // delivered newest-first to exercise buffering
        }
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            2,
            "the lagging member caught up to the survivor"
        );
        assert!(peer.pending_blocks.is_empty());
    }

    /// Split a bootstrap offer back into the shape `verify_served` takes.
    fn split_bootstrap(
        events: &[WorkspaceEvent],
    ) -> (Option<molt_core::CheckpointState>, Vec<ChainBlock>) {
        let mut blob = None;
        let mut blocks = Vec::new();
        for ev in events {
            match ev {
                WorkspaceEvent::CheckpointServed { blob: b } => blob = Some(b.clone()),
                WorkspaceEvent::Committed(bl) => blocks.push(bl.clone()),
                other => panic!("a bootstrap offer carries nothing else: {other:?}"),
            }
        }
        (blob, blocks)
    }

    /// **The anchor is the smallest prefix that verifies standalone.**
    ///
    /// A rejoiner cannot be handed the whole chain — one `set_image` block
    /// exceeds the gift-wrap cap — and cannot be handed a bare head, because
    /// `verify_chain` is all-or-nothing from the anchor and a headless node
    /// drops every block served to it. So it is handed the ANCHOR, and asks
    /// for the rest over the ordinary catch-up once it has a workspace.
    ///
    /// The COUNT is asserted, not merely the verification: an implementation
    /// that served the whole chain would verify perfectly well and quietly
    /// reintroduce the size cliff this exists to avoid.
    #[test]
    fn the_served_anchor_is_the_smallest_prefix_that_verifies() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);

        // a FULL holder offers the genesis, alone
        let full = chain_peer("walter", &b, b.blocks.clone());
        let offer = full.anchor_bootstrap();
        let (blob, blocks) = split_bootstrap(&offer);
        assert!(blob.is_none(), "a full holder has no blob to offer");
        assert_eq!(blocks.len(), 1, "the genesis and nothing else — not the chain");
        assert_eq!(blocks[0].height, 0);
        let (head, sealed) = verify_served(blob.as_ref(), &blocks, Some(&b.republic_id))
            .expect("the genesis verifies standalone");
        assert_eq!(head.height, 0, "a length-1 chain is a valid chain");
        assert_eq!(sealed.republic_id, b.republic_id);

        // …and a PRUNED holder offers its blob plus its anchor block, because
        // by then no node anywhere still holds a genesis
        let blob_at_2 = checkpoint_state(&b.blocks, 2).expect("state@2");
        let anchor = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&blob_at_2),
            },
            &["petra", "walter"],
        );
        b.push(anchor.clone());
        let mut pruned = chain_peer("walter", &b, b.blocks[..3].to_vec());
        pruned.receive_block(anchor);
        assert!(pruned.checkpoint_blob.is_some(), "the holder pruned");

        let offer = pruned.anchor_bootstrap();
        let (blob, blocks) = split_bootstrap(&offer);
        assert!(blob.is_some(), "a pruned holder's blob IS its trust root");
        assert_eq!(blocks.len(), 1, "the anchor block alone, not the suffix");
        assert_eq!(blocks[0].height, 3);
        let (head, _) = verify_served(blob.as_ref(), &blocks, Some(&b.republic_id))
            .expect("blob + anchor verify standalone under the suffix rules");
        assert_eq!(head.height, 3, "the rejoiner starts at the cut, not at zero");

        // …and broadcasting it must not disturb the SERVER: a 445 reaches
        // every member, so the offer travels back through this node's own
        // apply path (and through every survivor's) as a duplicate
        let before = (pruned.chain.clone(), pruned.chain_head.clone());
        pruned.serve_chain_anchor();
        assert_eq!(pruned.chain, before.0, "serving must not move the server's chain");
        assert_eq!(
            pruned.chain_head.as_ref().map(|h| h.height),
            before.1.as_ref().map(|h| h.height),
            "nor its head"
        );
    }

    /// WP3, the wire side of the decodability gate: a peer's `set_image`
    /// gossip with undecodable bytes is dropped with a warning, never
    /// recorded as a pending proposal (convergence before enforcement —
    /// the same posture as the byte-cap guard it extends).
    #[test]
    fn an_undecodable_peer_set_image_is_dropped_not_recorded() {
        use base64::Engine as _;
        let b = Builder::new(&["petra", "walter"], 2);
        let mut peer = chain_peer("walter", &b, b.blocks.clone());
        let deliver = |peer: &mut crate::State, id: u64, b64: String| {
            let env = molt_core::EventEnvelope { prev_seq: 0,
                seq: 90 + id,
                ts: 1_751_000_000,
                by: "petra".to_string(),
                body: WorkspaceEvent::Proposed {
                    id: ProposalId(id),
                    surface: Surface::Organization,
                    payload: json!({ "op": "set_image", "value": "x.png", "bytes_b64": b64 }),
                },
            };
            peer.cmd_net_delivered("petra".to_string(), env, None)
                .expect("a wire drop acks, never errors");
        };
        // garbage within the byte cap: dropped, not recorded
        let garbage = base64::engine::general_purpose::STANDARD.encode(b"not an image");
        deliver(&mut peer, 9, garbage);
        assert!(
            !peer.proposals.contains_key(&9),
            "undecodable peer bytes must never become a pending proposal"
        );
        // a real 2x2 png is recorded
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==";
        deliver(&mut peer, 10, png.to_string());
        assert!(
            peer.proposals.contains_key(&10),
            "a decodable peer set_image is recorded as pending"
        );
    }

    /// **Self-edit, on the wire** (`member_profiles_plan.md` §2): a member
    /// profile belongs to its member, and the link identity is the only
    /// proof of authorship — so a profile proposal claiming ANOTHER seat is
    /// dropped, never recorded. Node-independent like the drops beside it,
    /// so every honest holder drops the same frame. The picture ops carry
    /// the decodable+square verdict onto the wire too.
    #[test]
    fn a_profile_proposal_claiming_another_member_is_dropped() {
        use base64::Engine as _;
        let b = Builder::new(&["petra", "walter"], 2);
        let mut peer = chain_peer("walter", &b, b.blocks.clone());
        let deliver = |peer: &mut crate::State, id: u64, from: &str, payload: serde_json::Value| {
            let env = molt_core::EventEnvelope {
                prev_seq: 0,
                seq: 300 + id,
                ts: 1_751_000_000,
                by: from.to_string(),
                body: WorkspaceEvent::Proposed {
                    id: ProposalId(id),
                    surface: Surface::Organization,
                    payload,
                },
            };
            peer.cmd_net_delivered(from.to_string(), env, None)
                .expect("a wire drop acks, never errors");
        };
        // A profile op arriving under ANOTHER member's link is the normal
        // WP2 shape, not a forgery: `serve_open_governance` re-serves every
        // open card under the SERVING peer's identity (make_env(me, body)),
        // so a catching-up holder meets walter's edit with from = petra.
        // Dropping on `payload.member != from` blinded exactly that holder -
        // it could never see, let alone vote on, another seat's profile
        // proposal. Authorship is unauthenticated by design here
        // (`ProposalRecord.by` is a DISPLAY hint); the self-edit rule is
        // enforced where it IS decidable, at the propose gate.
        deliver(&mut peer, 20, "petra", json!({ "op": "set_member_desc", "member": "walter", "value": "hi" }));
        assert!(
            peer.proposals.contains_key(&20),
            "a re-served profile card must reach a catching-up holder"
        );
        // petra editing her own: recorded
        deliver(&mut peer, 21, "petra", json!({ "op": "set_member_desc", "member": "petra", "value": "hi" }));
        assert!(peer.proposals.contains_key(&21), "a member's own profile edit is recorded");
        // what IS node-independently decidable: the seat must exist. A
        // profile op for a stranger could never fold onto anything
        deliver(&mut peer, 26, "petra", json!({ "op": "set_member_desc", "member": "ghost", "value": "hi" }));
        assert!(
            !peer.proposals.contains_key(&26),
            "a profile op for a seat that is not in the roster is dropped"
        );
        deliver(&mut peer, 27, "petra", json!({ "op": "set_member_desc", "value": "hi" }));
        assert!(
            !peer.proposals.contains_key(&27),
            "a profile op naming no seat is dropped"
        );
        // the picture ops carry the square rule onto the wire
        let square = base64::engine::general_purpose::STANDARD
            .encode(crate::tests::tiny_bmp_header(2, 2));
        let wide = base64::engine::general_purpose::STANDARD
            .encode(crate::tests::tiny_bmp_header(4, 2));
        deliver(&mut peer, 22, "petra", json!({ "op": "set_member_image", "member": "petra", "value": "f.bmp", "bytes_b64": wide }));
        assert!(!peer.proposals.contains_key(&22), "a non-square peer avatar is dropped");
        deliver(&mut peer, 23, "petra", json!({ "op": "set_member_image", "member": "petra", "value": "f.bmp", "bytes_b64": square }));
        assert!(peer.proposals.contains_key(&23), "a square, decodable peer avatar is recorded");
        // the length cap is a contract, not a local preference: a description
        // the propose gate refuses must not walk in through the wire door
        let long = "x".repeat(crate::proposals::DESC_MAX + 1);
        deliver(&mut peer, 24, "petra", json!({ "op": "set_member_desc", "member": "petra", "value": long }));
        assert!(
            !peer.proposals.contains_key(&24),
            "an over-long description is dropped at the wire too"
        );
        let edge = "x".repeat(crate::proposals::DESC_MAX);
        deliver(&mut peer, 25, "petra", json!({ "op": "set_member_desc", "member": "petra", "value": edge }));
        assert!(peer.proposals.contains_key(&25), "a description at the limit is recorded");
    }

    /// A 2-of-3 chain peer holding the FULL three-member roster (the shared
    /// `chain_peer` pins the founding pair).
    fn chain_peer_3(member: &str, b: &Builder) -> crate::State {
        let mut peer = crate::tests::plain_state();
        peer.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: member.to_string(),
            roster: vec!["petra".to_string(), "walter".to_string(), "dora".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: "play chess".to_string(),
            features: None,
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        peer.adopt_chain(b.blocks.clone());
        peer
    }

    /// One wire envelope from `from` (per-sender seq; prev_seq 0 = unordered).
    fn wire(peer: &mut crate::State, from: &str, seq: u64, body: WorkspaceEvent) {
        let env = molt_core::EventEnvelope {
            prev_seq: 0,
            seq,
            ts: 1_751_000_000 + seq,
            by: from.to_string(),
            body,
        };
        peer.cmd_net_delivered(from.to_string(), env, None)
            .expect("a wire delivery acks, never errors");
    }

    /// **The proposer pulls a proposal back** (the ProposalCard's "pull
    /// back"): terminal like a rejection, but no vote is forged — the
    /// verdict is `withdrawn`, never "declined by".
    #[test]
    fn a_withdraw_turns_the_card_terminal_without_forging_a_vote() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        let id = match peer
            .cmd_propose(
                Surface::Organization,
                json!({ "op": "set_name", "value": "Mine" }),
            )
            .expect("propose")
        {
            molt_core::Reply::Proposed { id } => id,
            other => panic!("unexpected reply {other:?}"),
        };
        peer.cmd_withdraw(id).expect("withdraw");
        let p = peer.proposals.get(&id.0).expect("card");
        assert_eq!(p.state, ProposalState::Rejected);
        assert!(p.withdrawn, "the verdict is its own, not a decline");
        assert!(p.decliners.is_empty(), "no vote forged");
        assert_eq!(p.declined_by, "", "no decliner named");
        assert!(
            !peer.pending_sigs.contains_key(&id.0),
            "collected signatures are cleared"
        );
        // terminal: a second withdraw refuses
        assert!(peer.cmd_withdraw(id).is_err());
    }

    /// Only the proposer withdraws: the local command refuses a foreign
    /// card, and the wire arm counts a withdraw only when the link
    /// identity IS the recorded proposer (no signature — same posture as
    /// declines, plus the proposer check).
    #[test]
    fn only_the_proposer_may_withdraw() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Petras" }),
            },
        );
        // walter is not the proposer — the command refuses
        assert!(matches!(
            peer.cmd_withdraw(ProposalId(9)),
            Err(molt_core::MoltError::NotTheProposer(_))
        ));
        // forgery: dora's link carries petra's withdraw — dropped
        wire(
            &mut peer,
            "dora",
            1,
            WorkspaceEvent::Withdrawn { id: ProposalId(9), by: "petra".to_string() },
        );
        assert_eq!(
            peer.proposals.get(&9).expect("card").state,
            ProposalState::Proposed
        );
        // dora withdrawing petra's card as herself — not the proposer, dropped
        wire(
            &mut peer,
            "dora",
            2,
            WorkspaceEvent::Withdrawn { id: ProposalId(9), by: "dora".to_string() },
        );
        assert_eq!(
            peer.proposals.get(&9).expect("card").state,
            ProposalState::Proposed
        );
        // the real proposer pulls it back
        wire(
            &mut peer,
            "petra",
            2,
            WorkspaceEvent::Withdrawn { id: ProposalId(9), by: "petra".to_string() },
        );
        let p = peer.proposals.get(&9).expect("card");
        assert_eq!(p.state, ProposalState::Rejected);
        assert!(p.withdrawn);
    }

    /// A withdraw ahead of its proposal parks (G7 orders per sender only)
    /// and lands the moment the card arrives — the verdict must never be
    /// lost to arrival order, exactly like a parked decline.
    #[test]
    fn a_withdraw_ahead_of_its_proposal_parks_and_registers() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Withdrawn { id: ProposalId(9), by: "petra".to_string() },
        );
        wire(
            &mut peer,
            "petra",
            2,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Gone" }),
            },
        );
        let p = peer.proposals.get(&9).expect("card");
        assert_eq!(p.state, ProposalState::Rejected);
        assert!(p.withdrawn, "the parked withdraw registered on arrival");
    }

    /// The own withdraw re-serves with the open governance — a peer that
    /// was closed while the card died must still learn the verdict.
    #[test]
    fn open_governance_reserves_the_own_withdraw() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        let id = match peer
            .cmd_propose(
                Surface::Organization,
                json!({ "op": "set_name", "value": "Short-lived" }),
            )
            .expect("propose")
        {
            molt_core::Reply::Proposed { id } => id,
            other => panic!("unexpected reply {other:?}"),
        };
        peer.cmd_withdraw(id).expect("withdraw");
        // keep the card inside the display retention (fixture ts is historic)
        peer.proposals.get_mut(&id.0).expect("card").declined_at = crate::now_secs();
        let events = peer.open_governance_events();
        assert!(
            events.iter().any(|e| matches!(
                e,
                WorkspaceEvent::Withdrawn { id: wid, by } if *wid == id && by == "walter"
            )),
            "the own withdraw re-serves"
        );
    }

    /// Live incident 2026-08-09 (defect 6): a decline is a VOTE — it must
    /// converge like an approval. Two wire declines in a 2-of-3 kill the
    /// proposal on every node; before the receive arm existed they were
    /// acked and DROPPED, so a majority-declined vote stayed pending forever
    /// on every node but the decliner's own.
    #[test]
    fn wire_declines_converge_and_reject_at_the_veto_threshold() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "New Name" }),
            },
        );
        wire(
            &mut peer,
            "dora",
            1,
            WorkspaceEvent::Declined { id: ProposalId(9), by: "dora".to_string(), hash: String::new() },
        );
        let p = peer.proposals.get(&9).expect("registered");
        assert_eq!(p.decliners, vec!["dora".to_string()], "the wire decline counts");
        assert_eq!(p.state, ProposalState::Proposed, "one decline in 2-of-3 leaves room");
        // forgery: the body claims dora again, but the link says petra — dropped,
        // a peer can only ever decline as itself
        wire(
            &mut peer,
            "petra",
            2,
            WorkspaceEvent::Declined { id: ProposalId(9), by: "dora".to_string(), hash: String::new() },
        );
        let p = peer.proposals.get(&9).expect("still there");
        assert_eq!(p.decliners.len(), 1, "a decline must carry its link identity");
        // a duplicate of dora's decline (resend) stays ONE voice
        wire(
            &mut peer,
            "dora",
            2,
            WorkspaceEvent::Declined { id: ProposalId(9), by: "dora".to_string(), hash: String::new() },
        );
        assert_eq!(peer.proposals.get(&9).expect("still there").decliners.len(), 1);
        // petra's real decline tips it: 2 > n − m = 1 → Rejected
        wire(
            &mut peer,
            "petra",
            3,
            WorkspaceEvent::Declined { id: ProposalId(9), by: "petra".to_string(), hash: String::new() },
        );
        let p = peer.proposals.get(&9).expect("still there");
        assert_eq!(p.state, ProposalState::Rejected, "a majority decline is terminal");
        assert_eq!(p.declined_by, "petra", "the tipping decliner is named");
        assert!(p.declined_at > 0, "the decline timestamp is the envelope's");
    }

    /// D7: a decline the FULL park would shed must stay UNACKED — the
    /// accept point ran before the park admission, so a shed voice was
    /// ACKed and the at-least-once guarantee was already spent on it: the
    /// sender trims it and the voice is gone for good. Left unacked, the
    /// resend machinery re-earns it once the park has room (or the
    /// proposal lands). The implausible-id garbage case deliberately stays
    /// accept-and-drop — a u64::MAX decline must not ride resend forever.
    #[test]
    fn a_shed_decline_stays_unacked_for_the_resend() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let base = walter.next_id;
        let per_member = u64::try_from(crate::proposals::PARKED_DECLINES_PER_MEMBER_MAX)
            .expect("cap fits");
        // fill petra's whole per-member allowance with plausible unknown ids
        for i in 0..per_member {
            wire(
                &mut walter,
                "petra",
                i + 1,
                WorkspaceEvent::Declined { id: ProposalId(base + i), by: "petra".to_string(), hash: String::new() },
            );
        }
        let accepted = |st: &crate::State, seq: u64| {
            st.accepted.get("petra").is_some_and(|w| w.is_accepted(seq))
        };
        assert!(accepted(&walter, per_member), "parked voices are accepted and acked");
        // the voice the park sheds must NOT be marked accepted
        wire(
            &mut walter,
            "petra",
            per_member + 1,
            WorkspaceEvent::Declined {
                id: ProposalId(base + per_member),
                by: "petra".to_string(),
                hash: String::new(),
            },
        );
        assert!(
            !accepted(&walter, per_member + 1),
            "a shed voice stays unacked so the resend re-earns it"
        );
        // …while garbage far past the mint window stays accept-and-drop
        wire(
            &mut walter,
            "petra",
            per_member + 2,
            WorkspaceEvent::Declined { id: ProposalId(u64::MAX), by: "petra".to_string(), hash: String::new() },
        );
        assert!(
            accepted(&walter, per_member + 2),
            "implausible-id garbage is accepted and dropped, never resent"
        );
    }

    /// D4: a park drain speaks with EVERY drained voice — one
    /// `Event::Declined` per registered member (never one event naming
    /// `decliners.last()`), and a drain that tips emits the voices AND the
    /// `Rejected`. An event-stream consumer must not undercount votes.
    #[test]
    fn a_park_drain_emits_one_declined_event_per_voice() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        // veto_room = 4 - 2 = 2: two parked voices stay a Voice drain
        let b = Builder::new(&["petra", "walter", "dora", "erika"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        wire(
            &mut walter,
            "dora",
            1,
            WorkspaceEvent::Declined { id: ProposalId(4), by: "dora".to_string(), hash: String::new() },
        );
        wire(
            &mut walter,
            "erika",
            1,
            WorkspaceEvent::Declined { id: ProposalId(4), by: "erika".to_string(), hash: String::new() },
        );
        let mut ev = walter.subscribe_events();
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(4),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Late" }),
            },
        );
        let mut declined: Vec<String> = Vec::new();
        let mut rejected = 0;
        while let Ok(e) = ev.try_recv() {
            match e {
                crate::Event::Declined { id, by } if id.0 == 4 => declined.push(by),
                crate::Event::Rejected { id } if id.0 == 4 => rejected += 1,
                _ => {}
            }
        }
        declined.sort_unstable();
        assert_eq!(
            declined,
            vec!["dora".to_string(), "erika".to_string()],
            "one event per drained voice"
        );
        assert_eq!(rejected, 0, "two voices in veto room 2 do not tip");

        // …and a drain that TIPS still speaks every voice, then the verdict
        let mut peer = chain_signer("walter", &b, b.blocks.clone());
        for (i, who) in ["dora", "erika", "petra"].iter().enumerate() {
            wire(
                &mut peer,
                who,
                u64::try_from(i).expect("i") + 1,
                WorkspaceEvent::Declined {
                    id: ProposalId(4),
                    by: (*who).to_string(),
                    hash: String::new(),
                },
            );
        }
        let mut ev = peer.subscribe_events();
        wire(
            &mut peer,
            "petra",
            2,
            WorkspaceEvent::Proposed {
                id: ProposalId(4),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Late" }),
            },
        );
        let (mut declined, mut rejected) = (0, 0);
        while let Ok(e) = ev.try_recv() {
            match e {
                crate::Event::Declined { id, .. } if id.0 == 4 => declined += 1,
                crate::Event::Rejected { id } if id.0 == 4 => rejected += 1,
                _ => {}
            }
        }
        assert_eq!((declined, rejected), (3, 1), "3 voices + the verdict");
    }

    /// D5: the decision line of a DECLINED vote is minted under a
    /// DETERMINISTIC message id — whoever tips posts it, concurrent
    /// posters collapse via the ordinary duplicate-id drop, and a wire tip
    /// posts too (it used to stay silent, so a vote tipped by a received
    /// decline had no decision line anywhere).
    #[test]
    fn a_wire_tipped_decline_posts_its_summary_exactly_once() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        // veto_room = 3 - 2 = 1: the SECOND decline tips
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(4),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "X" }),
            },
        );
        let summaries = |st: &crate::State| {
            st.chat_visible()
                .filter(|m| {
                    m.kind == molt_core::ChatKind::System
                        && matches!(&m.channel, molt_core::ChannelRef::Patch { id } if id.0 == 4)
                })
                .count()
        };
        walter.cmd_decline(ProposalId(4)).expect("own voice, no tip");
        assert_eq!(summaries(&walter), 0, "one voice does not decide");
        wire(
            &mut walter,
            "dora",
            1,
            WorkspaceEvent::Declined { id: ProposalId(4), by: "dora".to_string(), hash: String::new() },
        );
        assert_eq!(
            summaries(&walter),
            1,
            "the wire tip posts the decision line"
        );
        // the OTHER tipper's copy arrives under the SAME deterministic id —
        // the ordinary duplicate-id drop collapses it
        let sid = crate::chat::decision_summary_id(&b.republic_id, 4, true);
        let copy = molt_core::ChatMessage::text(sid, "dora".to_string(), "⚖ #4 ⊘ …".to_string(), crate::now_secs())
            .with_channel(molt_core::ChannelRef::Patch { id: ProposalId(4) })
            .with_kind(molt_core::ChatKind::System);
        wire(&mut walter, "dora", 2, WorkspaceEvent::Chat(copy));
        assert_eq!(summaries(&walter), 1, "concurrent posters collapse to one line");
    }

    /// D1: a decline binds the payload the decliner SAW, not a bare id —
    /// two proposers minting the same id in one gossip round-trip must
    /// not let a voice register against a proposal the decliner never
    /// judged. An empty hash (older sender) keeps id-only semantics, and
    /// the park stores the hash so a drained voice is checked too.
    #[test]
    fn a_decline_carrying_a_foreign_payload_hash_does_not_register() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(4),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "X" }),
            },
        );
        let h = |v: &serde_json::Value| crate::State::decline_payload_hash(v);
        wire(
            &mut walter,
            "dora",
            1,
            WorkspaceEvent::Declined {
                id: ProposalId(4),
                by: "dora".to_string(),
                hash: h(&json!({ "op": "set_name", "value": "Y" })),
            },
        );
        let p = walter.proposals.get(&4).expect("card");
        assert!(p.decliners.is_empty(), "a mismatching hash must not register");
        wire(
            &mut walter,
            "dora",
            2,
            WorkspaceEvent::Declined {
                id: ProposalId(4),
                by: "dora".to_string(),
                hash: h(&json!({ "op": "set_name", "value": "X" })),
            },
        );
        assert_eq!(
            walter.proposals.get(&4).expect("card").decliners,
            vec!["dora".to_string()],
            "the matching hash registers"
        );
        wire(
            &mut walter,
            "petra",
            2,
            WorkspaceEvent::Declined {
                id: ProposalId(4),
                by: "petra".to_string(),
                hash: String::new(),
            },
        );
        assert_eq!(
            walter.proposals.get(&4).expect("card").decliners.len(),
            2,
            "an empty hash (older sender) keeps id-only semantics"
        );
        // the PARK stores the hash: a parked mismatch never registers either
        wire(
            &mut walter,
            "dora",
            3,
            WorkspaceEvent::Declined {
                id: ProposalId(9),
                by: "dora".to_string(),
                hash: h(&json!({ "op": "set_name", "value": "Z" })),
            },
        );
        wire(
            &mut walter,
            "petra",
            3,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "W" }),
            },
        );
        assert!(
            walter.proposals.get(&9).expect("card").decliners.is_empty(),
            "a drained parked voice is hash-checked too"
        );
    }

    /// A decline can outrun its proposal on the wire (G7 orders per sender
    /// only) and it replays from the own log before a re-served proposal
    /// returns: either way it PARKS and registers the moment the proposal
    /// is known — a vote must never be lost to arrival order.
    #[test]
    fn a_decline_ahead_of_its_proposal_parks_and_registers() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        wire(
            &mut peer,
            "dora",
            1,
            WorkspaceEvent::Declined { id: ProposalId(4), by: "dora".to_string(), hash: String::new() },
        );
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Declined { id: ProposalId(4), by: "petra".to_string(), hash: String::new() },
        );
        assert!(peer.proposals.is_empty(), "no card yet — the declines wait");
        wire(
            &mut peer,
            "petra",
            2,
            WorkspaceEvent::Proposed {
                id: ProposalId(4),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Late" }),
            },
        );
        let p = peer.proposals.get(&4).expect("registered");
        assert_eq!(p.decliners.len(), 2, "both parked declines registered");
        assert_eq!(p.state, ProposalState::Rejected, "and they tip it immediately");
    }

    /// WP2 re-serve carries the OWN decline: a vote against survives RAM
    /// loss like a collected signature does, so a rejoiner (or a node whose
    /// pre-fix engine dropped the gossip) can still converge. Foreign
    /// declines are NOT re-attested — only the link identity vouches a
    /// decline — and a REJECTED card still serves the own voice.
    #[test]
    fn open_governance_reserves_the_own_decline() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        // an open card walter declined (1 ≤ veto room → still Proposed)
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(7),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Open" }),
            },
        );
        peer.cmd_decline(ProposalId(7)).expect("decline");
        // a card that went Rejected (petra's wire decline tips it)
        wire(
            &mut peer,
            "petra",
            2,
            WorkspaceEvent::Proposed {
                id: ProposalId(8),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Dead" }),
            },
        );
        peer.cmd_decline(ProposalId(8)).expect("decline");
        wire(
            &mut peer,
            "petra",
            3,
            WorkspaceEvent::Declined { id: ProposalId(8), by: "petra".to_string(), hash: String::new() },
        );
        assert_eq!(
            peer.proposals.get(&8).expect("card").state,
            ProposalState::Rejected
        );
        // the fixture's wire ts is historic — stamp the decline fresh, or
        // the retention gate below would age the card out immediately
        peer.proposals.get_mut(&8).expect("card").declined_at = crate::now_secs();
        // a parked own decline (own-log replay raced a re-served proposal)
        peer.register_decline(11, "walter", 1_751_000_000, "");
        let events = peer.open_governance_events();
        let own_declines: Vec<u64> = events
            .iter()
            .filter_map(|e| match e {
                WorkspaceEvent::Declined { id, by, .. } if by == "walter" => Some(id.0),
                _ => None,
            })
            .collect();
        assert!(own_declines.contains(&7), "the open card's own decline re-serves");
        assert!(own_declines.contains(&8), "the rejected card's own decline re-serves");
        assert!(own_declines.contains(&11), "the parked own decline re-serves");
        assert!(
            !events.iter().any(
                |e| matches!(e, WorkspaceEvent::Declined { by, .. } if by == "petra")
            ),
            "a foreign decline is never re-attested"
        );
        // a rejected card past the display retention has no convergence
        // audience — its voice leaves the batch (review 2026-08-09,
        // finding 12), so the re-serve stays bounded
        peer.proposals.get_mut(&8).expect("card").declined_at = 1;
        let aged: Vec<u64> = peer
            .open_governance_events()
            .iter()
            .filter_map(|e| match e {
                WorkspaceEvent::Declined { id, by, .. } if by == "walter" => Some(id.0),
                _ => None,
            })
            .collect();
        assert!(!aged.contains(&8), "an aged-out rejected voice stops re-serving");
        assert!(aged.contains(&7), "the open card's voice stays");
    }

    /// Answering a ChainRequest re-records the served Proposed envelopes
    /// through the applier — that must not clobber the live card: an
    /// unconditional insert wiped every collected foreign decline on the
    /// SERVING node (review 2026-08-09, finding 1).
    #[test]
    fn serving_open_governance_keeps_the_own_cards_decliners() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Kept" }),
            },
        );
        wire(
            &mut peer,
            "dora",
            1,
            WorkspaceEvent::Declined { id: ProposalId(9), by: "dora".to_string(), hash: String::new() },
        );
        peer.serve_open_governance();
        assert_eq!(
            peer.proposals.get(&9).expect("card").decliners,
            vec!["dora".to_string()],
            "serving must not wipe the collected voices"
        );
    }

    /// A decline referencing an id far past the mint counter is garbage —
    /// parking it would poison `next_id` (one u64::MAX frame froze every
    /// later local mint; review 2026-08-09, finding 2).
    #[test]
    fn a_decline_for_an_implausible_id_is_dropped() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        let before = peer.next_id;
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Declined { id: ProposalId(u64::MAX), by: "petra".to_string(), hash: String::new() },
        );
        assert!(peer.pending_declines.is_empty(), "garbage never parks");
        assert_eq!(peer.next_id, before, "and never moves the mint counter");
    }

    /// Decline-after-approve stays ALLOWED — it is how a proposer
    /// withdraws (the auto-cosign would otherwise lock every proposal
    /// open); the summary test pins the terminal effect. The view still
    /// reports the own stance so frontends can gray per what the engine
    /// actually refuses (approve-after-decline, re-decline).
    #[test]
    fn a_decline_after_the_own_approval_still_works() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        peer.identity_sk = b
            .keys
            .iter()
            .find(|(m, _)| m == "walter")
            .map(|(_, sk)| sk.clone());
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Erst ja" }),
            },
        );
        peer.cmd_approve(ProposalId(9)).expect("approve signs");
        let p = peer.proposals.get(&9).cloned().expect("card");
        assert!(peer.view(9, &p).approved_by_me, "the signature is collected");
        peer.cmd_decline(ProposalId(9)).expect("the withdrawal path stays open");
        let p = peer.proposals.get(&9).cloned().expect("card");
        let v = peer.view(9, &p);
        assert!(v.declined_by_me, "the stance the frontend grays on");
        // D2 (last vote counts): the decline RETRACTED the collected
        // signature — one member holds one stance, never both
        assert!(
            !peer
                .pending_sigs
                .get(&9)
                .is_some_and(|s| s.sigs.iter().any(|a| a.member == "walter")),
            "the own signature is retracted by the decline"
        );
        assert!(!v.approved_by_me, "…and the view says so");
    }

    /// L3 headline: `receive_checkpoint_proposal` ran `id + 1` BEFORE any
    /// guard — with overflow-checks + panic=abort in release, one hostile
    /// frame from any roster peer ABORTED the process. And every wire
    /// receive fn bumped the mint counter before its guards, so an
    /// in-window absurd id poisoned every later local mint.
    #[test]
    fn implausible_wire_ids_neither_abort_nor_poison_the_mint() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let before = walter.next_id;
        // the former one-frame remote abort
        walter.receive_checkpoint_proposal(u64::MAX, 0, "00");
        assert_eq!(walter.next_id, before, "no mint poison from the cut");
        // the surface twin
        assert!(!walter.receive_proposed(
            u64::MAX,
            Surface::Memory,
            json!({ "op": "add_note" }),
            "petra"
        ));
        assert_eq!(walter.next_id, before, "no mint poison from a proposal");
        // …and the membership twin
        walter.receive_membership_proposal(
            u64::MAX,
            MembershipOp::Restored,
            "petra",
            &b.pk("petra"),
            None,
            Vec::new(),
            None,
        );
        assert_eq!(walter.next_id, before, "no mint poison from membership");
    }

    /// L3: signatures collect only for ROSTER members — dedup is by the
    /// free-form member string, so distinct fake names grew one Vec
    /// without bound (~96 KiB of wire per entry).
    #[test]
    fn approvals_from_non_members_never_enter_the_pending_set() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": 1 }),
            },
        );
        for i in 0..100u64 {
            walter.receive_approval(1, &format!("ghost{i}"), 1, "ff");
        }
        assert!(
            walter.pending_sigs.get(&1).map_or(0, |p| p.sigs.len()) <= 2,
            "ghost names must not grow the set"
        );
    }

    /// L3: ONE cut per head is registered and co-signed — the identical
    /// (upto, state_hash) under fresh ids minted one registry entry plus
    /// one signed Approved per frame (a 1:1 outbound amplifier).
    #[test]
    fn only_one_cut_per_head_registers() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let ours = checkpoint_state_hash(
            &walter.own_checkpoint_state(0).expect("own projection"),
        );
        for id in 50..60u64 {
            walter.receive_checkpoint_proposal(id, 0, &ours);
        }
        let cuts = walter
            .proposal_changes
            .values()
            .filter(|c| matches!(c, ChainChange::Checkpoint { .. }))
            .count();
        assert_eq!(cuts, 1, "the first id IS the cut for this head");
    }

    /// L3: the future-block buffer holds only heights the drain could ever
    /// reach and stays size-capped — an unverified far-future block was
    /// buffered forever (and one such block froze auto-compaction).
    #[test]
    fn a_far_future_block_is_refused_not_buffered() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let junk = |height: u64| ChainBlock {
            height,
            prev: "00".to_string(),
            change: ChainChange::Applied {
                proposal_id: height,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note" }),
            },
            sigs: Vec::new(),
        };
        walter.receive_block(junk(u64::MAX / 2));
        assert!(
            walter.pending_blocks.is_empty(),
            "a block far past the head never buffers"
        );
        walter.receive_block(junk(3));
        assert_eq!(walter.pending_blocks.len(), 1, "a near gap buffers for the drain");
    }

    /// L3: a flooding proposer crowds only ITSELF — the newest own card is
    /// refused at the cap, another member's card still lands.
    #[test]
    fn a_wire_proposal_flood_is_bounded_per_proposer() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        for i in 0..200u64 {
            walter.receive_proposed(
                100 + i,
                Surface::Memory,
                json!({ "op": "add_note", "i": i }),
                "petra",
            );
        }
        let open_petra = walter
            .proposals
            .values()
            .filter(|p| p.state == ProposalState::Proposed && p.by == "petra")
            .count();
        assert_eq!(open_petra, OPEN_CARDS_PER_PROPOSER_MAX, "the cap holds");
        assert!(
            walter.receive_proposed(
                900,
                Surface::Memory,
                json!({ "op": "add_note" }),
                "walter"
            ),
            "another member's honest card still lands"
        );
    }

    /// L2: the DISPLAYED approval count and pills read only signatures
    /// that VERIFY — a peer gossiping junk must not inflate progress or
    /// paint a forged stance onto a named seat. Sealing was always safe
    /// (`try_commit` filters); this pins the display.
    #[test]
    fn an_unverifiable_approval_is_not_displayed_as_consent() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let payload = json!({ "op": "add_note", "id": 1 });
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Memory,
                payload: payload.clone(),
            },
        );
        // junk: parses as no valid signature over the approval bytes
        walter.receive_approval(1, "petra", 1, "deadbeef");
        assert_eq!(walter.chain_approval_count(1), 0, "junk shows no progress");
        let p = walter.proposals.get(&1).cloned().expect("card");
        let v = walter.view(1, &p);
        assert_eq!(v.approvals, 0);
        let petra_row = v
            .votes
            .iter()
            .find(|mv| mv.member == "petra")
            .map(|mv| mv.vote)
            .expect("row");
        assert_eq!(petra_row, molt_core::VoteState::Open, "no forged pill");
        // …the genuine signature counts, and the vote still seals (liveness)
        let change = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Memory,
            payload: payload.clone(),
        };
        let bytes = approval_bytes(&b.republic_id, 1, &change);
        walter.receive_approval(1, "petra", 1, &identity_sign(b.key("petra"), &bytes));
        assert_eq!(walter.chain_approval_count(1), 1, "the genuine one displays");
        walter.chain_sign_and_gossip_approval(1);
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            1,
            "verification costs no liveness — the block seals"
        );
    }

    /// L2 liveness twin: an approval that OUTRAN its card is collected but
    /// not displayed, and becomes displayable the moment the card lands —
    /// the naive drop-on-unverifiable fix would wedge gossip ordering.
    #[test]
    fn an_approval_that_outran_its_card_counts_once_the_card_lands() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let payload = json!({ "op": "set_name", "value": "Early" });
        let change = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Organization,
            payload: payload.clone(),
        };
        let bytes = approval_bytes(&b.republic_id, 1, &change);
        walter.receive_approval(1, "petra", 1, &identity_sign(b.key("petra"), &bytes));
        assert_eq!(
            walter.chain_approval_count(1),
            0,
            "not verifiable yet — the card has not landed"
        );
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Organization,
                payload,
            },
        );
        assert_eq!(
            walter.chain_approval_count(1),
            1,
            "the card landed — the collected signature displays"
        );
    }

    /// D2: `try_commit` excludes CURRENT decliners — a stale re-served
    /// signature of a member whose standing decline this node holds must
    /// not count toward m, or a majority-declined proposal seals on
    /// whichever node collected the leftovers.
    #[test]
    fn a_current_decliners_stale_signature_does_not_seal() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let payload = json!({ "op": "set_name", "value": "Contested" });
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Organization,
                payload: payload.clone(),
            },
        );
        // dora's decline stands…
        wire(
            &mut walter,
            "dora",
            1,
            WorkspaceEvent::Declined { id: ProposalId(1), by: "dora".to_string(), hash: String::new() },
        );
        // …then her STALE signature arrives (re-served by a peer that
        // missed the decline) and collects
        let change = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Organization,
            payload: payload.clone(),
        };
        let bytes = approval_bytes(&b.republic_id, 1, &change);
        let dora_sig = identity_sign(b.key("dora"), &bytes);
        walter.receive_approval(1, "dora", 1, &dora_sig);
        // walter co-signs: 2 collected — but dora is a CURRENT decliner
        walter.chain_sign_and_gossip_approval(1);
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            0,
            "no block seals while a counted signer's decline stands"
        );
        assert!(
            matches!(walter.proposals.get(&1), Some(p) if p.state == ProposalState::Proposed),
            "the card stays open"
        );
    }

    /// D2 (last vote counts, decided 2026-08-16): approving over the own
    /// standing decline RETRACTS the decline — the newest stance wins,
    /// mirroring the decline's signature retraction. One member, one
    /// stance, changeable until the vote seals.
    #[test]
    fn an_approve_retracts_the_standing_own_decline() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Beides?" }),
            },
        );
        peer.identity_sk = b
            .keys
            .iter()
            .find(|(m, _)| m == "walter")
            .map(|(_, sk)| sk.clone());
        peer.cmd_decline(ProposalId(9)).expect("decline");
        peer.cmd_approve(ProposalId(9)).expect("the newest stance wins");
        let p = peer.proposals.get(&9).cloned().expect("card");
        let v = peer.view(9, &p);
        assert!(!v.declined_by_me, "the decline is retracted");
        assert!(v.approved_by_me, "…and the approval stands");
        assert!(
            !p.decliners.iter().any(|d| d == "walter"),
            "the decliner list no longer names the member"
        );
    }

    /// An applied MEMBERSHIP card reads its voters from the sealed block
    /// too — matched by content (op + member), since a Membership block
    /// carries no proposal id (review 2026-08-09, finding 7).
    #[test]
    fn an_applied_membership_card_reports_the_block_signers() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        let block = b.seal(
            1,
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member: "dora".to_string(),
                identity_pk: b
                    .keys
                    .iter()
                    .find(|(m, _)| m == "dora")
                    .map(|(_, sk)| hex::encode(sk.verifying_key().to_bytes()))
                    .expect("dora's key"),
                nostr_pk: None,
                relays: Vec::new(),
                consent: None,
            },
            &["petra", "walter"],
        );
        b.push(block);
        let mut peer = chain_peer_3("walter", &b);
        assert_eq!(
            peer.chain_head.as_ref().map(|h| h.height),
            Some(1),
            "the membership block adopted"
        );
        peer.proposals.insert(
            4,
            molt_core::ProposalRecord {
                surface: Surface::Organization,
                payload: json!({ "op": "restore_member", "member": "dora" }),
                approvals: 0,
                state: ProposalState::Applied,
                declined_at: 0,
                declined_by: String::new(),
                decliners: Vec::new(),
                voted: Vec::new(),
                by: String::new(),
                superseded: false,
                withdrawn: false,
            },
        );
        let p = peer.proposals.get(&4).cloned().expect("card");
        let v = peer.view(4, &p);
        assert_eq!(v.approvals, 2, "the membership block's signature count");
    }

    /// An APPLIED card keeps naming its voters: the sealed block carries
    /// the signatures (the ephemeral collection is cleared at commit), so
    /// the view reads them from the chain — live incident 2026-08-09,
    /// defect 7: the applied history showed "0 approvals, every pill open".
    #[test]
    fn an_over_subscribed_voter_still_reads_approved_on_the_applied_card() {
        // D6: try_commit seals only the m lowest-named signatures (chain
        // truth), but a voter whose signature fell off the block must not
        // read Open on a vote they cast — the collected set survives as
        // record-side DISPLAY data at the seal.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_signer("walter", &b, b.blocks.clone());
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": 1 }),
            },
        );
        peer.cmd_approve(ProposalId(1)).expect("walter signs — 1 of 2 locally");
        // the block seals from the other side, signed by petra and dora
        b.commit_applied(1, &["petra", "dora"]);
        peer.receive_block(b.blocks[1].clone());
        let p = peer.proposals.get(&1).cloned().expect("card");
        assert_eq!(p.state, ProposalState::Applied);
        let v = peer.view(1, &p);
        assert_eq!(v.approvals, 2, "the chain-proven count stays the block's");
        assert!(v.approved_by_me, "walter cast a vote and must see it");
        let walter_row = v
            .votes
            .iter()
            .find(|mv| mv.member == "walter")
            .map(|mv| mv.vote)
            .expect("roster row");
        assert_eq!(
            walter_row,
            molt_core::VoteState::Approved,
            "the over-subscribed voter's pill"
        );
        // a post-seal approval must feed the display, never resurrect the
        // ephemeral collection on a terminal card
        wire(
            &mut peer,
            "dora",
            1,
            WorkspaceEvent::Approved {
                id: ProposalId(1),
                by: "dora".to_string(),
                height: 1,
                sig: "ff".to_string(),
            },
        );
        assert!(
            !peer.pending_sigs.contains_key(&1),
            "a terminal card collects no pending signatures"
        );
    }

    #[test]
    fn an_applied_card_reports_the_block_signers() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        // the card arrives as gossip; the sealed block (signed by petra and
        // dora) commits it — walter himself never collected a signature
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": 1 }),
            },
        );
        b.commit_applied(1, &["petra", "dora"]);
        peer.receive_block(b.blocks[1].clone());
        let p = peer.proposals.get(&1).cloned().expect("card");
        assert_eq!(p.state, ProposalState::Applied);
        let v = peer.view(1, &p);
        assert_eq!(v.approvals, 2, "the block's signature count");
        let vote_of = |m: &str| {
            v.votes
                .iter()
                .find(|mv| mv.member == m)
                .map(|mv| mv.vote)
                .expect("roster row")
        };
        assert_eq!(vote_of("petra"), molt_core::VoteState::Approved);
        assert_eq!(vote_of("dora"), molt_core::VoteState::Approved);
        assert_eq!(vote_of("walter"), molt_core::VoteState::Open);
        assert!(!v.approved_by_me, "walter did not sign");
        // the read contract serves the applied proposals too (the Accepted
        // table renders from the snapshot, co-equal for every frontend)
        let snap = peer.snapshot(Surface::Memory, None, None);
        assert_eq!(snap.accepted.len(), 1, "the applied card is in the snapshot");
        assert_eq!(snap.accepted[0].id, ProposalId(1));
        assert_eq!(snap.accepted[0].approvals, 2, "with its block-sourced voters");
    }

    /// WP4b stage 2, full holders: a committed checkpoint block verifies
    /// only when its `state_hash` matches THIS chain's own recomputed
    /// projection — a forged or drifted summary is hard-rejected with the
    /// whole chain (all-or-nothing, like every other violation).
    #[test]
    fn a_checkpoint_block_verifies_against_the_own_projection() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["walter", "dora"]);
        let state = checkpoint_state(&b.blocks, 2).expect("state@2");
        let good = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&state),
            },
            &["petra", "walter"],
        );
        let mut chain = b.blocks.clone();
        chain.push(good.clone());
        let head = verify_chain(&chain).expect("a truthful checkpoint verifies");
        assert_eq!(head.height, 3);
        // a forged state hash is rejected with the whole chain
        let forged = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: molt_storage::content_hash(b"not the projection"),
            },
            &["petra", "walter"],
        );
        let mut bad = b.blocks.clone();
        bad.push(forged);
        assert!(verify_chain(&bad).is_err(), "a forged checkpoint kills the chain");
        // upto must precede the block height
        let self_ref = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 3,
                state_hash: checkpoint_state_hash(&state),
            },
            &["petra", "walter"],
        );
        let mut bad = b.blocks.clone();
        bad.push(self_ref);
        assert!(verify_chain(&bad).is_err(), "upto >= height is structural nonsense");
    }

    /// WP4b stage 2, suffix holders: a chain that BEGINS with a checkpoint
    /// verifies from the blob as trust anchor — blob hash, founding
    /// recomputation (forgery check without the genesis), current-roster
    /// threshold on the anchor block, double-apply seeded across the cut.
    #[test]
    fn a_suffix_chain_bootstraps_from_a_checkpoint() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["walter", "dora"]);
        let blob = checkpoint_state(&b.blocks, 2).expect("state@2");
        let anchor = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(anchor.clone());
        // one more applied block on top — the suffix a dropped-history
        // holder keeps
        b.commit_applied(7, &["petra", "dora"]);
        let suffix: Vec<ChainBlock> = b.blocks[3..].to_vec();
        assert_eq!(suffix.len(), 2, "anchor + one applied block");

        let head = verify_suffix_chain(&blob, &suffix, &b.republic_id)
            .expect("the suffix verifies from the checkpoint anchor");
        assert_eq!(head.height, 4);
        assert_eq!(head.identities.len(), 3, "roster comes from the blob");

        // a doctored blob (foreign roster key) fails the hash check
        let mut forged = blob.clone();
        forged.roster[0].identity_pk = "00".repeat(32);
        assert!(
            verify_suffix_chain(&forged, &suffix, &b.republic_id).is_err(),
            "a doctored roster no longer hashes to the signed state"
        );
        // …and its nostr_pk twin: under checkpoint-v2 the third anchor is
        // inside the hashed bytes, so a swapped roster transport anchor is
        // caught exactly like a swapped identity key (under v1 it was NOT
        // hashed — a served blob's roster anchor was silently mutable)
        let mut forged_npk = blob.clone();
        forged_npk.roster[0].nostr_pk = "ee".repeat(32);
        assert!(
            verify_suffix_chain(&forged_npk, &suffix, &b.republic_id).is_err(),
            "a doctored roster nostr anchor no longer hashes to the signed state"
        );
        // a wholly self-consistent forged blob still fails the founding
        // recomputation against the expected republic id
        let mut alien = blob.clone();
        alien.founding_name = "Fake Club".to_string();
        let alien_anchor_hash = checkpoint_state_hash(&alien);
        let mut alien_suffix = suffix.clone();
        if let ChainChange::Checkpoint { state_hash, .. } = &mut alien_suffix[0].change {
            *state_hash = alien_anchor_hash;
        }
        assert!(
            verify_suffix_chain(&alien, &alien_suffix, &b.republic_id).is_err(),
            "a forged founding does not recompute to the real republic id"
        );
        // double-apply across the cut: proposal 1 was consumed below upto
        let mut replay = b.clone();
        replay.commit_applied(1, &["petra", "walter"]);
        let replay_suffix: Vec<ChainBlock> = replay.blocks[3..].to_vec();
        assert!(
            verify_suffix_chain(&blob, &replay_suffix, &b.republic_id).is_err(),
            "an id consumed below the cut cannot re-apply in the suffix"
        );
        // below-threshold anchor signatures are refused
        let weak_anchor = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra"],
        );
        assert!(
            verify_suffix_chain(&blob, &[weak_anchor], &b.republic_id).is_err(),
            "one signature is not a threshold"
        );
    }

    /// WP4b stage 3: the propose flow end to end at the state level.
    /// Petra proposes the cut (self-cosign = 1 of 2); Walter receives the
    /// gossip, RECOMPUTES the hash from his own chain, auto-co-signs on
    /// the match, and the checkpoint block seals at 2-of-2 — on both
    /// nodes, byte-identically. A mismatched hash is never signed; a
    /// stale cut dies on re-base instead of sealing an invalid block.
    #[test]
    fn a_checkpoint_proposal_seals_via_verify_before_sign() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        let mut walter = chain_signer("walter", &b, b.blocks.clone());

        let id = match petra.cmd_propose_checkpoint().expect("propose") {
            molt_core::Reply::Proposed { id } => id.0,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!(
            petra.pending_sigs.get(&id).map(|p| p.sigs.len()),
            Some(1),
            "the proposer co-signed its own cut"
        );
        let (upto, state_hash) = match petra.proposal_changes.get(&id) {
            Some(ChainChange::Checkpoint { upto, state_hash }) => {
                (*upto, state_hash.clone())
            }
            other => panic!("unexpected change: {other:?}"),
        };
        assert_eq!(upto, 1, "the cut is the current head (B-F1)");

        // a WRONG hash is refused: nothing registered, nothing signed
        walter.receive_checkpoint_proposal(id, upto, "00");
        assert!(!walter.proposal_changes.contains_key(&id));
        assert!(!walter.pending_sigs.contains_key(&id));

        // the truthful gossip: walter recomputes, matches, auto-co-signs
        walter.receive_checkpoint_proposal(id, upto, &state_hash);
        let petra_sig = petra
            .pending_sigs
            .get(&id)
            .expect("petra's set")
            .sigs
            .first()
            .expect("petra signed")
            .sig
            .clone();
        walter.receive_approval(id, "petra", 2, &petra_sig);
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            2,
            "the checkpoint sealed at 2-of-2 on walter"
        );
        assert!(matches!(
            walter.chain.last().expect("block").change,
            ChainChange::Checkpoint { .. }
        ));
        // petra converges from walter's signature the same way
        let walter_sig = walter
            .chain
            .last()
            .expect("block")
            .sigs
            .iter()
            .find(|a| a.member == "walter")
            .expect("walter signed")
            .sig
            .clone();
        petra.receive_approval(id, "walter", 2, &walter_sig);
        assert_eq!(petra.chain_head.as_ref().expect("head").height, 2);
        assert_eq!(
            block_hash(&b.republic_id, petra.chain.last().expect("b")),
            block_hash(&b.republic_id, walter.chain.last().expect("b")),
            "both nodes sealed the byte-identical checkpoint block"
        );
        // the sealed proposal's bookkeeping is gone on both
        assert!(!petra.proposal_changes.contains_key(&id));
        assert!(!walter.proposal_changes.contains_key(&id));
    }

    /// WP4b stage 4: sealing a checkpoint DROPS the summarized history
    /// locally (B-F2), the blob becomes the trust anchor, pre-cut applied
    /// entries stay readable, and the pruned holder keeps verifying and
    /// extending its suffix chain — including a reopen-style re-adopt.
    #[test]
    fn a_sealed_checkpoint_drops_history_and_the_holder_keeps_governing() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        // the propose flow seals the cut at 2-of-2 (stage-3 mechanics)
        let hash = checkpoint_state_hash(&checkpoint_state(&b.blocks, 1).expect("state"));
        walter.receive_checkpoint_proposal(40, 1, &hash);
        let change = ChainChange::Checkpoint { upto: 1, state_hash: hash };
        let bytes = approval_bytes(&b.republic_id, 2, &change);
        let petra_sig = identity_sign(b.key("petra"), &bytes);
        walter.receive_approval(40, "petra", 2, &petra_sig);
        // sealed AND pruned: only the anchor remains, the blob anchors
        assert_eq!(walter.chain_head.as_ref().expect("head").height, 2);
        assert_eq!(walter.chain.len(), 1, "history below the cut is dropped");
        assert!(matches!(
            walter.chain.first().expect("anchor").change,
            ChainChange::Checkpoint { .. }
        ));
        let blob = walter.checkpoint_blob.clone().expect("blob anchors the holder");
        assert_eq!(blob.upto, 1);
        // pre-cut applied entries survive in the read projection
        let mem = walter.chain_applied.get(&Surface::Memory).expect("projection");
        assert_eq!(mem.len(), 1, "the pre-cut applied entry stays readable");
        // the pruned holder keeps governing: a fresh applied change seals
        // on top of the suffix (verify runs the suffix rules)
        let payload = json!({"op": "add_note", "title": "post-cut"});
        walter.receive_proposed(41, Surface::Memory, payload.clone(), "peer");
        let post = ChainChange::Applied {
            proposal_id: 41,
            surface: Surface::Memory,
            payload,
        };
        let bytes = approval_bytes(&b.republic_id, 3, &post);
        let petra_sig = identity_sign(b.key("petra"), &bytes);
        walter.receive_approval(41, "petra", 3, &petra_sig);
        walter.chain_sign_and_gossip_approval(41);
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            3,
            "the pruned holder extends its suffix"
        );
        assert_eq!(
            walter.chain_applied.get(&Surface::Memory).map(|v| v.len()),
            Some(2),
            "pre- and post-cut entries read together"
        );
        // reopen-style: a fresh holder re-anchors on blob + suffix
        let mut reopened = chain_peer("walter", &b, b.blocks.clone());
        reopened.checkpoint_blob = Some(blob);
        reopened.adopt_chain(walter.chain.clone());
        assert_eq!(
            reopened.chain_head.as_ref().expect("head").height,
            3,
            "a pruned chain re-adopts from the persisted blob"
        );
        assert_eq!(
            reopened.chain_applied.get(&Surface::Memory).map(|v| v.len()),
            Some(2)
        );
        // …and the Accepted cards match an unpruned holder's (review
        // 2026-08-16): the pre-cut card materializes from the blob's
        // summarized payloads — voter pills open, the sigs went with the
        // cut — the post-cut card from its live block, voters proven
        let snap = reopened.snapshot(Surface::Memory, None, None);
        let card = |pid: u64| {
            snap.accepted
                .iter()
                .find(|c| c.id.0 == pid)
                .unwrap_or_else(|| panic!("card {pid}"))
        };
        assert_eq!(card(1).approvals, 0, "pre-cut: only chain-provable votes show");
        assert!(
            card(41)
                .votes
                .iter()
                .any(|v| v.vote == molt_core::VoteState::Approved),
            "post-cut: the live block's signers show"
        );
    }

    /// A reopen replays the proposal CARDS from the persisted gossip first
    /// and adopts the chain second (`open_stored_workspace`) — adoption must
    /// settle every card the chain already consumed, or each restart
    /// resurrects decided votes as open cards (live incident 2026-08-09: a
    /// sealed `set_relays` vote came back 'proposed' on every launch of the
    /// proposer's node, its `restore_member` twins with it).
    #[test]
    fn adopting_a_chain_settles_replayed_proposal_cards() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        b.commit_applied(7, &["petra", "walter"]);
        b.commit_restored("petra", &"ab".repeat(32), &["petra", "walter"]);

        // the reopen shape: gossip-replayed cards, THEN the chain
        let mut reopened = chain_peer("walter", &b, genesis);
        reopened.receive_proposed(7, Surface::Memory, json!({ "op": "add_note", "id": 7 }), "peer");
        reopened.receive_proposed(
            8,
            Surface::Organization,
            json!({ "op": "restore_member", "member": "petra" }),
            "peer",
        );
        reopened.adopt_chain(b.blocks.clone());

        let card = reopened.proposals.get(&7).expect("card survives");
        assert_eq!(card.state, ProposalState::Applied, "the chain consumed id 7");
        let restore = reopened.proposals.get(&8).expect("restore card survives");
        assert_eq!(
            restore.state,
            ProposalState::Applied,
            "the Restored block settles the membership card"
        );

        // the LIVE twin: a late (resent) Proposed for a consumed id must not
        // re-open a card. Adoption already materialized the APPLIED record
        // from the block (ensure_applied_record) — the resend must neither
        // create a second one nor flip it back to open.
        let mut live = chain_peer("walter", &b, b.blocks.clone());
        assert!(
            !live.receive_proposed(7, Surface::Memory, json!({ "op": "add_note", "id": 7 }), "peer"),
            "a consumed id must not open a fresh card"
        );
        assert_eq!(
            live.proposals.get(&7).map(|p| p.state),
            Some(ProposalState::Applied),
            "the consumed id stays a settled, chain-proven card"
        );
    }

    /// **The `seen` trap.** Once a checkpoint drops the history below the cut,
    /// the double-apply guard can no longer be read off `self.chain`: the
    /// blocks carrying those proposal ids are gone. It has to come from the
    /// blob's `consumed_ids` — which is exactly what a walk state carried
    /// across the prune, or rebuilt from the surviving blocks, gets wrong.
    ///
    /// `verify_suffix_chain` seeds it correctly today
    /// (`a_suffix_chain_bootstraps_from_a_checkpoint`), and this is the
    /// ENGINE-level twin: the guard must survive the prune on a live holder,
    /// which is the property an incremental verifier has to preserve.
    #[test]
    fn an_id_consumed_below_the_cut_cannot_replay_on_a_pruned_holder() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let pre_cut = b.blocks.clone();
        let blob = checkpoint_state(&b.blocks, 2).expect("state@2");
        let anchor = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(anchor.clone());

        let mut peer = chain_peer("walter", &b, pre_cut);
        peer.receive_block(anchor);
        assert!(peer.checkpoint_blob.is_some(), "the cut sealed and anchored");
        assert_eq!(peer.chain.len(), 1, "history below the cut is dropped");
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 3);

        // proposal 1 was consumed at height 1 — a block this holder no longer
        // has. Re-offering it must still be refused.
        b.commit_applied(1, &["petra", "walter"]);
        let replay = b.blocks.last().expect("the replay block").clone();
        assert_eq!(replay.height, 4, "the replay sits on top of the anchor");
        peer.receive_block(replay);
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            3,
            "an id consumed below the cut cannot re-apply after the prune"
        );
        assert_eq!(peer.chain.len(), 1, "the refused block is not retained");

        // …while a FRESH id on the same suffix still extends it, so the test
        // cannot pass by the holder having stopped accepting anything
        b.blocks.pop();
        b.head_hash = block_hash(&b.republic_id, &peer.chain[0]);
        b.commit_applied(9, &["petra", "walter"]);
        peer.receive_block(b.blocks.last().expect("fresh block").clone());
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            4,
            "a fresh id extends the pruned holder"
        );
    }

    /// **The catch-up is linear.** Draining a buffered suffix used to verify
    /// the whole chain from the anchor for every block, and TWICE per block
    /// (a probe clone, then the append) — `2nN + m·N(N+1)` signature checks,
    /// all inside one uninterruptible actor turn. A node catching up then
    /// looked exactly like a dead one to its peers, which is what the
    /// delivery guarantee escalates on.
    ///
    /// Counted, not timed: the assertion is the point of the whole change.
    #[test]
    fn catching_up_verifies_each_block_once() {
        const N: usize = 40;
        let b = grown_chain(N + 1);
        let mut peer = chain_peer("walter", &b, b.blocks[..1].to_vec());

        // reverse order, so every block buffers and the whole suffix drains
        // in ONE turn — the shape a real catch-up has
        VERIFY_STEPS.with(|c| c.set(0));
        CHAIN_PERSISTS.with(|c| c.set(0));
        for block in b.blocks[1..].iter().rev() {
            peer.receive_block(block.clone());
        }
        let steps = VERIFY_STEPS.with(std::cell::Cell::get);
        let writes = CHAIN_PERSISTS.with(std::cell::Cell::get);

        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            u64::try_from(N).expect("small chain"),
            "the whole suffix drained"
        );
        assert_eq!(
            steps, N,
            "each block is verified exactly once — a re-walk per block would \
             cost {} here, and 7M at N=1000",
            N * (N + 1)
        );
        assert_eq!(
            writes, 1,
            "the drained batch is written ONCE — the write blocks on the \
             storage writer's ack, so {N} of them would sit inside one turn"
        );
    }

    /// Batching the write must not turn "once per block" into "never".
    ///
    /// Every path that accepts a block ends in exactly one
    /// `persist_chain_now`; this is the guard for a future third caller that
    /// forgets, which would leave accepted blocks unwritten and silently
    /// re-fetched on every restart.
    #[test]
    fn an_accepted_block_is_written_once_and_a_refused_one_not_at_all() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        b.commit_applied(1, &["petra", "walter"]);
        let mut peer = chain_peer("walter", &b, genesis);

        CHAIN_PERSISTS.with(|c| c.set(0));
        peer.receive_block(b.blocks[1].clone());
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 1);
        assert_eq!(
            CHAIN_PERSISTS.with(std::cell::Cell::get),
            1,
            "an accepted block is written"
        );

        let refused = b.seal(
            2,
            ChainChange::Applied {
                proposal_id: 2,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": 2 }),
            },
            &["petra"],
        );
        CHAIN_PERSISTS.with(|c| c.set(0));
        peer.receive_block(refused);
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 1);
        assert_eq!(
            CHAIN_PERSISTS.with(std::cell::Cell::get),
            0,
            "a refused block writes nothing"
        );
    }

    /// A **refused** block must leave the walk byte-identical, because the
    /// walk is now cached across calls.
    ///
    /// The order inside `verify_next` is what makes this sharp: the
    /// double-apply guard is consulted BEFORE the signatures are checked. An
    /// implementation that recorded the id while checking would let one
    /// unsigned block burn a proposal id forever — this holder would then
    /// refuse a block every other node accepts, which is a fork produced by
    /// bookkeeping alone.
    #[test]
    fn a_refused_block_does_not_poison_the_walk() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        let mut peer = chain_peer("walter", &b, genesis);

        // a well-formed block for proposal 7, signed by ONE of two — refused
        // at the threshold, but only after the guard has seen the id
        let change = ChainChange::Applied {
            proposal_id: 7,
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "id": 7 }),
        };
        peer.receive_block(b.seal(1, change, &["petra"]));
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            0,
            "a below-threshold block is refused"
        );

        // …and the legitimate block for the SAME proposal still lands
        b.commit_applied(7, &["petra", "walter"]);
        peer.receive_block(b.blocks[1].clone());
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            1,
            "a refused block must not burn its proposal id"
        );
    }

    /// The incrementally-folded projection must equal the whole-chain
    /// rebuild — including across a Membership block, where the anchors map
    /// and the roster move too.
    ///
    /// This is the property `project_one` trades a refold for; a full rebuild
    /// re-clones every payload in the chain per block, which made a drain of
    /// N blocks clone the applied log N²/2 times.
    #[test]
    fn the_appended_projection_equals_the_whole_chain_rebuild() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let fresh = molt_net::nostr_identity(b"walter-recovered", "new-ticket").1;
        b.commit_restored("walter", &fresh, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let mut peer = chain_peer("walter", &b, b.blocks[..1].to_vec());
        for block in b.blocks[1..].iter().rev() {
            peer.receive_block(block.clone());
        }
        assert_eq!(peer.chain.len(), 4, "the whole suffix drained");

        let incremental = (peer.chain_applied.clone(), peer.chain_anchors.clone());
        peer.apply_chain_to_state();
        assert_eq!(
            incremental,
            (peer.chain_applied.clone(), peer.chain_anchors.clone()),
            "the appended projection must equal the whole-chain rebuild"
        );
    }

    /// Extending incrementally must equal verifying from the anchor — at
    /// EVERY prefix, not just the end. The cached walk is only sound while
    /// that holds, and it is the property a second implementation would
    /// silently drift from.
    #[test]
    fn incremental_extension_equals_full_verification_at_every_prefix() {
        let b = grown_chain(12);
        let mut peer = chain_peer("walter", &b, b.blocks[..1].to_vec());
        for (i, block) in b.blocks[1..].iter().enumerate() {
            peer.receive_block(block.clone());
            let full = verify_chain(&b.blocks[..=i + 1]).expect("the prefix verifies in full");
            let cached = peer.chain_head.as_ref().expect("head");
            assert_eq!(cached.height, full.height);
            assert_eq!(cached.hash, full.hash, "prefix {} diverged", i + 1);
            assert_eq!(cached.identities, full.identities);
            assert!(
                peer.chain_walk
                    .as_ref()
                    .expect("the walk is kept")
                    .describes(&peer.chain, peer.checkpoint_blob.as_ref()),
                "the cached walk must describe the chain it was built on"
            );
        }
    }

    /// Grow a builder chain to exactly `len` blocks (genesis included).
    fn grown_chain(len: usize) -> Builder {
        let mut b = Builder::new(&["petra", "walter"], 2);
        for id in 0..len.saturating_sub(1) {
            b.commit_applied(u64::try_from(id + 100).expect("small id"), &["petra", "walter"]);
        }
        assert_eq!(b.blocks.len(), len);
        b
    }

    /// Drive one gated proposal through `s` to a sealed block: peer
    /// approval first, then the local co-sign seals at 2-of-2.
    fn seal_one(s: &mut crate::State, b: &Builder, peer: &str, id: u64) {
        let target = s.chain_head.as_ref().expect("head").height + 1;
        let payload = json!({"op": "add_note", "id": id});
        s.receive_proposed(id, Surface::Memory, payload.clone(), "peer");
        let change = ChainChange::Applied {
            proposal_id: id,
            surface: Surface::Memory,
            payload,
        };
        let bytes = approval_bytes(&b.republic_id, target, &change);
        let sig = identity_sign(b.key(peer), &bytes);
        s.receive_approval(id, peer, target, &sig);
        s.chain_sign_and_gossip_approval(id);
        assert_eq!(
            s.chain_head.as_ref().expect("head").height,
            target,
            "the driven proposal seals"
        );
    }

    /// The pending checkpoint cut registered in `s`, if any.
    fn pending_cut(s: &crate::State) -> Option<u64> {
        s.proposal_changes.values().find_map(|c| match c {
            ChainChange::Checkpoint { upto, .. } => Some(*upto),
            _ => None,
        })
    }

    /// WP4b automation: once the chain reaches the trigger length, the
    /// alphabetically LOWEST-named roster member auto-proposes the
    /// compaction cut right after a block commit (every co-signer is at
    /// the same head then) — and co-signs it like a manual propose. A
    /// non-lowest member never auto-proposes: one deterministic
    /// proposer, no node-local id collisions.
    #[test]
    fn the_lowest_member_auto_proposes_a_checkpoint_at_the_trigger_length() {
        let b = grown_chain(AUTO_CHECKPOINT_MIN_LEN - 2);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        // one below the trigger: a commit runs the hook, but no cut yet —
        // pins the length lower bound THROUGH the hook, not just at init
        seal_one(&mut petra, &b, "walter", 90);
        assert_eq!(pending_cut(&petra), None, "below the trigger: no cut");
        seal_one(&mut petra, &b, "walter", 300);
        let head = petra.chain_head.as_ref().expect("head").height;
        assert_eq!(
            pending_cut(&petra),
            Some(head),
            "the lowest member proposes the cut at the fresh head"
        );

        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        seal_one(&mut walter, &b, "petra", 90);
        seal_one(&mut walter, &b, "petra", 300);
        assert_eq!(
            pending_cut(&walter),
            None,
            "a non-lowest member never auto-proposes"
        );
    }

    /// The trigger is bound to SEALING at the live head: a passively
    /// applied block (catch-up serve, another sealer's broadcast) never
    /// auto-proposes — a catching-up node would cut at a stale
    /// intermediate head, and a lockstep-catching-up quorum could even
    /// co-sign that cut and fork a holder after it dropped history.
    #[test]
    fn a_passively_applied_block_never_auto_proposes() {
        let mut b = grown_chain(AUTO_CHECKPOINT_MIN_LEN - 1);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        // the trigger length arrives via the PASSIVE path — no cut
        b.commit_applied(400, &["petra", "walter"]);
        petra.receive_block(b.blocks.last().expect("built block").clone());
        assert_eq!(petra.chain.len(), AUTO_CHECKPOINT_MIN_LEN, "passively at length");
        assert_eq!(
            pending_cut(&petra),
            None,
            "a passively applied block never triggers the cut"
        );
        // the next SELF-sealed block fires it
        seal_one(&mut petra, &b, "walter", 90);
        let head = petra.chain_head.as_ref().expect("head").height;
        assert_eq!(
            pending_cut(&petra),
            Some(head),
            "the node's own seal at the live head triggers the cut"
        );
    }

    /// The automation never cuts while a vote is open: an interfering
    /// seal would only stale the cut. The trigger re-fires on the commit
    /// that resolves the last open vote.
    #[test]
    fn no_auto_checkpoint_while_a_vote_is_open() {
        let b = grown_chain(AUTO_CHECKPOINT_MIN_LEN - 1);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        // a second, still-open vote holds the cut back
        petra.receive_proposed(91, Surface::Memory, json!({"op": "add_note", "id": 91}), "peer");
        seal_one(&mut petra, &b, "walter", 90);
        assert_eq!(
            pending_cut(&petra),
            None,
            "no cut while another vote is open"
        );
        // resolving the open vote triggers the cut on ITS commit
        let target = petra.chain_head.as_ref().expect("head").height + 1;
        let change = ChainChange::Applied {
            proposal_id: 91,
            surface: Surface::Memory,
            payload: json!({"op": "add_note", "id": 91}),
        };
        let bytes = approval_bytes(&b.republic_id, target, &change);
        let sig = identity_sign(b.key("walter"), &bytes);
        petra.receive_approval(91, "walter", target, &sig);
        petra.chain_sign_and_gossip_approval(91);
        assert_eq!(
            pending_cut(&petra),
            Some(target),
            "the commit that clears the last open vote fires the cut"
        );
    }

    /// A staled cut needs no timer: the very block that staled it re-runs
    /// the trigger and re-proposes at the new head.
    #[test]
    fn a_staled_auto_cut_re_proposes_on_the_next_commit() {
        let b = grown_chain(AUTO_CHECKPOINT_MIN_LEN - 1);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        seal_one(&mut petra, &b, "walter", 90);
        let first_cut = pending_cut(&petra).expect("auto cut pending");
        // an interfering surface vote seals first — the cut goes stale
        // (id 300: well clear of the auto-cut's freshly minted next_id)
        seal_one(&mut petra, &b, "walter", 300);
        let head = petra.chain_head.as_ref().expect("head").height;
        assert_eq!(
            pending_cut(&petra),
            Some(head),
            "the staled cut is re-proposed at the new head"
        );
        assert!(first_cut < head, "the old cut was swept, not resurrected");
    }

    /// WP4b stage 4b: a holder that is BEHIND a served cut re-anchors on
    /// blob + anchor + suffix (hard-verified), and a forged blob is
    /// dropped at the cheap rid check.
    #[test]
    fn a_lagging_holder_re_anchors_on_a_served_blob() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
        let anchor = b.seal(
            2,
            ChainChange::Checkpoint {
                upto: 1,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(anchor.clone());
        b.commit_applied(7, &["petra", "walter"]);
        let suffix_tail = b.blocks.last().expect("tail").clone();

        // the laggard holds only the genesis
        let mut lag = chain_peer("walter", &b, b.blocks[..1].to_vec());
        assert_eq!(lag.chain_head.as_ref().expect("head").height, 0);
        // a forged blob (wrong founding) dies at the rid check
        let mut forged = blob.clone();
        forged.founding_name = "Fake".to_string();
        lag.receive_checkpoint_blob(forged);
        assert!(lag.pending_served_blob.is_none());
        // the served pieces arrive in any order: blob, tail, anchor
        lag.receive_checkpoint_blob(blob.clone());
        lag.receive_block(suffix_tail);
        assert_eq!(lag.chain_head.as_ref().expect("head").height, 0, "waits for the anchor");
        lag.receive_block(anchor);
        assert_eq!(
            lag.chain_head.as_ref().expect("head").height,
            3,
            "re-anchored on blob + anchor + suffix"
        );
        assert!(lag.checkpoint_blob.is_some());
        assert_eq!(
            lag.chain_applied.get(&Surface::Memory).map(|v| v.len()),
            Some(2),
            "pre-cut and post-cut entries both readable"
        );
    }

    /// SECURITY (total-review 2026-07-18): a peer-chosen id must never let
    /// a MEMBERSHIP proposal hijack a surface proposal's approvals — the
    /// same forge the checkpoint arm was hardened against, on the older
    /// membership arm. And symmetrically a surface proposal must not shadow
    /// a pending chain change.
    #[test]
    fn a_membership_proposal_cannot_hijack_a_colliding_surface_id() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        // honest surface proposal id 5, awaiting approvals
        walter.receive_proposed(5, Surface::Memory, json!({"op": "add_note"}), "peer");
        // attacker gossips a membership change under the SAME id
        walter.receive_membership_proposal(5, MembershipOp::Joined, "mallory", &"ab".repeat(32), None, Vec::new(), None);
        // the id still resolves to the SURFACE proposal — approving it can
        // never sign membership bytes
        assert!(matches!(
            walter.proposal_change(5),
            Some(ChainChange::Applied { .. })
        ));
        // the reverse: a surface proposal cannot shadow a pending membership
        let mut walter2 = chain_signer("walter", &b, b.blocks.clone());
        walter2.receive_membership_proposal(6, MembershipOp::Joined, "dora", &"cd".repeat(32), None, Vec::new(), None);
        walter2.receive_proposed(6, Surface::Memory, json!({"op": "add_note"}), "peer");
        assert!(matches!(
            walter2.proposal_change(6),
            Some(ChainChange::Membership { .. })
        ));
        assert!(!walter2.proposals.contains_key(&6), "surface proposal refused");
    }

    /// The supersede walk (shared_memory_real.md §4): sealing one wiki
    /// patch deterministically retires every OVERLAPPING pending patch —
    /// terminal and unattributed (no vote forged: `declined_by` stays
    /// empty) — keeps the DISJOINT one approvable and applying, and a
    /// stale patch learned late (catch-up) registers superseded right
    /// away. Approving a superseded card is refused honestly.
    #[test]
    fn a_sealed_wiki_patch_supersedes_overlapping_pending_patches() {
        const ADD_A: &str = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
        const EDIT_A_1: &str = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n-hello\n+hallo\n world\n";
        const EDIT_A_2: &str = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n-hello\n+servus\n world\n";
        const ADD_B: &str = "diff --git a/b.md b/b.md\nnew file mode 100644\n--- /dev/null\n+++ b/b.md\n@@ -0,0 +1,1 @@\n+disjoint\n";
        let wp = |p: &str| json!({"op": "wiki_patch", "summary": "x", "value": p});

        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        seal_wiki(&mut walter, &b, "petra", 10, wp(ADD_A));
        assert_eq!(
            walter.wiki_tree().get("a.md").map(String::as_str),
            Some("hello\nworld\n"),
            "the fold serves the sealed base"
        );

        // two pending edits of the SAME region, one disjoint add
        walter.receive_proposed(11, Surface::Memory, wp(EDIT_A_1), "petra");
        walter.receive_proposed(12, Surface::Memory, wp(EDIT_A_2), "petra");
        walter.receive_proposed(13, Surface::Memory, wp(ADD_B), "petra");

        seal_wiki(&mut walter, &b, "petra", 11, wp(EDIT_A_1));
        let p12 = walter.proposals.get(&12).cloned().expect("card 12");
        assert_eq!(p12.state, ProposalState::Rejected, "overlap retires");
        assert!(p12.superseded, "…as SUPERSEDED, not declined");
        assert!(p12.declined_by.is_empty(), "no vote is forged");
        assert!(walter.view(12, &p12).superseded);
        assert!(
            matches!(
                walter.cmd_approve(molt_core::ProposalId(12)),
                Err(molt_core::MoltError::AlreadyTerminal(_, _))
            ),
            "approving a superseded card is refused"
        );
        let p13 = walter.proposals.get(&13).cloned().expect("card 13");
        assert_eq!(p13.state, ProposalState::Proposed, "disjoint stays open");
        assert!(!p13.superseded);

        // …and the disjoint one still seals and folds
        seal_wiki(&mut walter, &b, "petra", 13, wp(ADD_B));
        assert_eq!(
            walter.wiki_tree().get("b.md").map(String::as_str),
            Some("disjoint\n")
        );
        assert_eq!(
            walter.wiki_tree().get("a.md").map(String::as_str),
            Some("hallo\nworld\n")
        );

        // a stale patch learned LATE registers superseded immediately
        walter.receive_proposed(14, Surface::Memory, wp(EDIT_A_2), "petra");
        let p14 = walter.proposals.get(&14).cloned().expect("card 14");
        assert_eq!(p14.state, ProposalState::Rejected);
        assert!(p14.superseded);

        // …and the READ serves the same base to GUI and MCP alike
        // (co-equality: one projection, shared_memory_real.md WP-B)
        let snap = walter.snapshot(Surface::Memory, None, None);
        assert_eq!(snap.wiki_rev, 3, "ADD_A + EDIT_A_1 + ADD_B applied");
        assert_eq!(
            snap.wiki_tree,
            vec![
                molt_core::WikiDoc {
                    path: "a.md".to_string(),
                    content: "hallo\nworld\n".to_string()
                },
                molt_core::WikiDoc {
                    path: "b.md".to_string(),
                    content: "disjoint\n".to_string()
                },
            ]
        );
    }

    /// `seal_one`'s wiki twin: drive `payload` through the real chain
    /// machinery to a sealed Applied block.
    fn seal_wiki(
        s: &mut crate::State,
        b: &Builder,
        peer: &str,
        id: u64,
        payload: serde_json::Value,
    ) {
        let target = s.chain_head.as_ref().expect("head").height + 1;
        s.receive_proposed(id, Surface::Memory, payload.clone(), "peer");
        let change = ChainChange::Applied {
            proposal_id: id,
            surface: Surface::Memory,
            payload,
        };
        let bytes = approval_bytes(&b.republic_id, target, &change);
        let sig = identity_sign(b.key(peer), &bytes);
        s.receive_approval(id, peer, target, &sig);
        s.chain_sign_and_gossip_approval(id);
        assert_eq!(s.chain_head.as_ref().expect("head").height, target, "sealed");
    }

    /// The pull-back visibility gate: a record remembers who proposed it,
    /// and `mine` is reader-relative — true only when the reader IS that
    /// member ("" matches nobody).
    #[test]
    fn proposal_views_know_their_proposer_and_mine_is_reader_relative() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        walter.receive_proposed(5, Surface::Memory, json!({"op": "add_note"}), "petra");
        let p = walter.proposals.get(&5).cloned().expect("record");
        assert_eq!(p.by, "petra");
        assert!(!walter.view(5, &p).mine, "petra's proposal is not walter's");
        // walter's own: the record carries his name, the view says mine
        walter.receive_proposed(6, Surface::Memory, json!({"op": "add_note"}), "walter");
        let own = walter.proposals.get(&6).cloned().expect("record");
        assert!(walter.view(6, &own).mine);
        // a pre-field record ("" proposer) is nobody's
        let mut blank = walter.proposals.get(&5).cloned().expect("record");
        blank.by = String::new();
        assert!(!walter.view(5, &blank).mine);
    }

    /// SECURITY: attacker-served checkpoint data with a height-0 anchor or
    /// upto = u64::MAX must be REFUSED, never underflow/overflow into a
    /// process abort (overflow-checks=true).
    #[test]
    fn malicious_checkpoint_heights_are_refused_not_panics() {
        let b = Builder::new(&["petra", "walter"], 2);
        let blob = checkpoint_state(&b.blocks, 0).expect("state@0");
        // a height-0 "checkpoint anchor" (anchor.height - 1 would underflow)
        let anchor0 = ChainBlock {
            height: 0,
            prev: GENESIS_PREV.to_string(),
            change: ChainChange::Checkpoint {
                upto: u64::MAX,
                state_hash: checkpoint_state_hash(&blob),
            },
            sigs: Vec::new(),
        };
        assert!(
            verify_suffix_chain(&blob, &[anchor0], &b.republic_id).is_err(),
            "a height-0 anchor is refused, not an underflow abort"
        );
        // a served blob with upto = u64::MAX (blob.upto + 1 would overflow)
        let mut peer = chain_peer("walter", &b, b.blocks.clone());
        let mut bomb = blob.clone();
        bomb.upto = u64::MAX;
        peer.pending_served_blob = Some(bomb);
        peer.try_adopt_from_blob(); // must not panic
        assert!(peer.pending_served_blob.is_none(), "the overflow blob is dropped");
    }

    /// Review pins: an id collision must never turn the auto-cosign into
    /// an unattended approval of a DIFFERENT change, and the gossip frame
    /// crosses the wire.
    #[test]
    fn a_checkpoint_proposal_never_signs_a_colliding_id() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let hash = checkpoint_state_hash(&checkpoint_state(&b.blocks, 1).expect("state"));
        // id already names a pending MEMBERSHIP change → refused, unsigned
        walter.receive_membership_proposal(5, MembershipOp::Restored, "petra", &b.pk("petra"), None, Vec::new(), None);
        walter.receive_checkpoint_proposal(5, 1, &hash);
        assert!(
            !walter.pending_sigs.contains_key(&5),
            "an occupied id must never be auto-signed"
        );
        assert!(matches!(
            walter.proposal_changes.get(&5),
            Some(ChainChange::Membership { .. })
        ));
        // id already names a SURFACE proposal → refused too
        walter.receive_proposed(6, Surface::Memory, json!({"op": "add_note"}), "peer");
        walter.receive_checkpoint_proposal(6, 1, &hash);
        assert!(!walter.pending_sigs.contains_key(&6));
        // a replayed valid frame does not amplify into more signatures
        walter.receive_checkpoint_proposal(9, 1, &hash);
        let sigs = walter.pending_sigs.get(&9).map(|p| p.sigs.len());
        walter.receive_checkpoint_proposal(9, 1, &hash);
        assert_eq!(walter.pending_sigs.get(&9).map(|p| p.sigs.len()), sigs);
        // the gossip frame is wire-scoped
        assert!(crate::net::crosses_wire(&WorkspaceEvent::CheckpointProposed {
            id: ProposalId(1),
            upto: 1,
            state_hash: hash,
        }));
    }

    /// A checkpoint cut pinned at the old head dies when another block
    /// commits first — dropped on re-base (re-cut needed), never re-signed
    /// into an invalid block.
    #[test]
    fn a_stale_checkpoint_proposal_dies_on_rebase() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        let id = match petra.cmd_propose_checkpoint().expect("propose") {
            molt_core::Reply::Proposed { id } => id.0,
            other => panic!("unexpected: {other:?}"),
        };
        // another applied block races the checkpoint to height 2
        b.commit_applied(7, &["petra", "walter"]);
        petra.receive_block(b.blocks.last().expect("block").clone());
        assert_eq!(petra.chain_head.as_ref().expect("head").height, 2);
        assert!(
            !petra.proposal_changes.contains_key(&id)
                && !petra.pending_sigs.contains_key(&id),
            "the stale cut is dropped, not re-signed"
        );
    }

    /// Review findings, pinned: (1) the anchor must not be circularly
    /// trusted — a blob whose roster is m sock-puppet keys (with the
    /// GENUINE public founding table, so the republic id recomputes!) is
    /// rejected even though its hash and "signatures" are self-consistent;
    /// (2) a checkpoint whose `upto` leaves a gap below its height is
    /// refused (gap blocks would escape blob AND suffix); (3) a SECOND
    /// checkpoint inside a suffix recomputes from the blob base and
    /// verifies.
    #[test]
    fn a_forged_roster_anchor_and_a_gap_upto_are_rejected() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["walter", "dora"]);
        let blob = checkpoint_state(&b.blocks, 2).expect("state@2");

        // sock-puppet forge: genuine founding fields, attacker-owned roster
        let mut forged = blob.clone();
        let (evil_sk1, evil_pk1) = derive_identity_key(&[9u8; 32], "petra");
        let (evil_sk2, evil_pk2) = derive_identity_key(&[8u8; 32], "walter");
        forged.roster = vec![
            MemberIdentity {
                member: "petra".to_string(),
                identity_pk: evil_pk1,
                nostr_pk: "ee".repeat(32),
            },
            MemberIdentity {
                member: "walter".to_string(),
                identity_pk: evil_pk2,
                nostr_pk: "ff".repeat(32),
            },
        ];
        let change = ChainChange::Checkpoint {
            upto: 2,
            state_hash: checkpoint_state_hash(&forged),
        };
        let bytes = approval_bytes(&b.republic_id, 3, &change);
        let anchor = ChainBlock {
            height: 3,
            prev: "00".repeat(32),
            change,
            sigs: vec![
                RosterAttestation { member: "petra".to_string(), sig: identity_sign(&evil_sk1, &bytes) },
                RosterAttestation { member: "walter".to_string(), sig: identity_sign(&evil_sk2, &bytes) },
            ],
        };
        assert!(
            verify_suffix_chain(&forged, &[anchor], &b.republic_id).is_err(),
            "a sock-puppet roster must never bootstrap a rejoiner"
        );

        // a gap upto (blocks between cut and block height) is refused on
        // both verify paths
        let gap = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 1,
                state_hash: checkpoint_state_hash(
                    &checkpoint_state(&b.blocks, 1).expect("state@1"),
                ),
            },
            &["petra", "walter"],
        );
        let mut chain = b.blocks.clone();
        chain.push(gap.clone());
        assert!(verify_chain(&chain).is_err(), "full holders refuse a gap upto");
        assert!(
            verify_suffix_chain(&blob, &[gap], &b.republic_id).is_err(),
            "suffix holders refuse a gap upto"
        );
    }

    /// N1 PIN — the suffix path must run the same structural size check as
    /// `verify_genesis` (`founding_identities.len() == rule_n`): a served
    /// blob whose founding table carries MORE entries than `rule_n` grafts
    /// attacker-owned "founding" keys into the signer set. The forged blob
    /// here is fully self-consistent (id and state hash recomputed over the
    /// 4-entry table, anchor signed by m REAL founding keys) and is checked
    /// against its own id — the trust-the-file restore posture — so only
    /// the size check can reject it. (Under the injective republic-id-v2
    /// layout a grafted table can no longer COLLIDE with the real id, so
    /// this is defense in depth for the paths that pin no external id.)
    #[test]
    fn a_suffix_blob_with_an_oversized_founding_table_is_rejected() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
        let mut forged = blob.clone();
        let (_evil_sk, evil_pk) = derive_identity_key(&[9u8; 32], "evil");
        forged.founding_identities.push(MemberIdentity {
            member: "evil".to_string(),
            identity_pk: evil_pk,
            nostr_pk: "dd".repeat(32),
        });
        forged.republic_id = molt_storage::republic_id(
            &forged.founding_name,
            forged.rule_m,
            forged.rule_n,
            &forged.founding_identities,
        );
        let change = ChainChange::Checkpoint {
            upto: 1,
            state_hash: checkpoint_state_hash(&forged),
        };
        let bytes = approval_bytes(&forged.republic_id, 2, &change);
        let sigs = ["petra", "walter"]
            .iter()
            .map(|name| {
                let (_, sk) = b.keys.iter().find(|(m, _)| m == name).expect("key");
                RosterAttestation {
                    member: (*name).to_string(),
                    sig: identity_sign(sk, &bytes),
                }
            })
            .collect();
        let anchor = ChainBlock {
            height: 2,
            prev: "00".repeat(32),
            change,
            sigs,
        };
        assert!(
            verify_suffix_chain(&forged, &[anchor], &forged.republic_id).is_err(),
            "a founding table larger than rule_n must be rejected"
        );
    }

    /// N1 PIN — the roster⊆founding comparison covers the THIRD anchor: a
    /// blob whose roster entry keeps its member+identity_pk but swaps the
    /// nostr anchor, with the state hash recomputed and the anchor block
    /// re-signed by m real founding keys (insider collusion — the state-hash
    /// check cannot catch a self-consistent re-signature), must still be
    /// rejected: seats are fixed at founding, so every roster entry must be
    /// a LITERAL founding-table entry, transport anchor included.
    #[test]
    fn a_resigned_roster_nostr_anchor_swap_is_rejected() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
        let mut forged = blob.clone();
        forged.roster[0].nostr_pk = "ee".repeat(32); // not petra's founding anchor
        let change = ChainChange::Checkpoint {
            upto: 1,
            state_hash: checkpoint_state_hash(&forged),
        };
        let bytes = approval_bytes(&forged.republic_id, 2, &change);
        let sigs = ["petra", "walter"]
            .iter()
            .map(|name| {
                let (_, sk) = b.keys.iter().find(|(m, _)| m == name).expect("key");
                RosterAttestation {
                    member: (*name).to_string(),
                    sig: identity_sign(sk, &bytes),
                }
            })
            .collect();
        let anchor = ChainBlock {
            height: 2,
            prev: "00".repeat(32),
            change,
            sigs,
        };
        assert!(
            verify_suffix_chain(&forged, &[anchor], &b.republic_id).is_err(),
            "a roster entry whose nostr anchor is not its founding-table anchor must be rejected"
        );
    }

    /// A second checkpoint INSIDE a suffix recomputes from the blob base
    /// and verifies — the chained-compaction path both holder types must
    /// agree on.
    #[test]
    fn a_second_checkpoint_inside_a_suffix_verifies_from_the_blob() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
        let anchor = b.seal(
            2,
            ChainChange::Checkpoint {
                upto: 1,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(anchor);
        b.commit_applied(9, &["petra", "walter"]);
        // the second cut, at the new head
        let state4 = checkpoint_state(&b.blocks, 3).expect("state@3");
        let second = b.seal(
            4,
            ChainChange::Checkpoint {
                upto: 3,
                state_hash: checkpoint_state_hash(&state4),
            },
            &["petra", "walter"],
        );
        b.push(second);
        // full holders accept the chained compaction…
        verify_chain(&b.blocks).expect("full holders verify the chained checkpoints");
        // …and so do suffix holders recomputing the second cut from the blob
        let suffix: Vec<ChainBlock> = b.blocks[2..].to_vec();
        let head = verify_suffix_chain(&blob, &suffix, &b.republic_id)
            .expect("suffix holders verify the second checkpoint from the blob base");
        assert_eq!(head.height, 4);
    }

    /// **A checkpoint SUMMARIZES — it does not archive** (§B.6a, decided
    /// 2026-08-03). The republic's logo changed three times; the blob carries
    /// the CURRENT one, and only that one.
    ///
    /// Asserted by CONTENT, not by count: a summary that kept the FIRST entry
    /// would satisfy a count check just as well, and would be silently,
    /// permanently wrong about what the republic looks like.
    #[test]
    fn a_checkpoint_keeps_the_current_value_of_a_slot_not_its_history() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_org(1, "set_image", "first.png", &["petra", "walter"]);
        b.commit_org(2, "set_image", "second.png", &["petra", "walter"]);
        b.commit_org(3, "set_image", "third.png", &["petra", "walter"]);

        let state = checkpoint_state(&b.blocks, 3).expect("state");
        let org: Vec<&(u64, serde_json::Value)> = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .map(|(_, list)| list.iter().collect())
            .unwrap_or_default();

        assert_eq!(org.len(), 1, "three logos survived the cut: {org:?}");
        assert_eq!(
            org[0].1.get("value").and_then(serde_json::Value::as_str),
            Some("third.png"),
            "the summary kept the wrong logo — a republic would show a superseded image forever"
        );
        // …and every consumed id survives, including the two whose payload
        // was dropped. This is the guard most likely to be lost by accident.
        assert_eq!(
            state.consumed_ids,
            vec![1, 2, 3],
            "a summarized-away payload must still be an un-re-appliable proposal id"
        );
    }

    /// Distinct slots do NOT collide, and a removal supersedes the image it
    /// removes — the two halves of "slot", both of which a naive
    /// keep-the-last-Organization-entry rule would get wrong.
    #[test]
    fn slots_are_independent_and_a_removal_supersedes_its_image() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_org(1, "set_image", "logo.png", &["petra", "walter"]);
        b.commit_org(2, "set_name", "Chess Club Reloaded", &["petra", "walter"]);
        b.commit_org(3, "remove_image", "", &["petra", "walter"]);

        let state = checkpoint_state(&b.blocks, 3).expect("state");
        let (_, org) = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .expect("organization entries");
        let ops: Vec<&str> = org
            .iter()
            .filter_map(|(_, p)| p.get("op").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(
            ops,
            vec!["set_name", "remove_image"],
            "the name and image slots must survive independently, and the removal must \
             supersede the set_image it removes: {org:?}"
        );
    }

    /// **A checkpoint is a summary, not a delete.** Memory's notes are
    /// distinct objects, not superseded state, so every one of them survives
    /// the cut — the rule cannot be read as "keep only the last entry".
    #[test]
    fn accumulating_entries_all_survive_the_cut() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        b.commit_applied(3, &["petra", "walter"]);

        let state = checkpoint_state(&b.blocks, 3).expect("state");
        let (_, notes) = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Memory)
            .expect("memory entries");
        assert_eq!(
            notes.len(),
            3,
            "notes are distinct objects — summarizing them away deletes the shared brain: {notes:?}"
        );
    }

    /// An op no build declares takes the CONSERVATIVE direction: it
    /// accumulates. Dropping something that was not superseded loses data,
    /// and an older node meeting a newer op must not guess otherwise.
    #[test]
    fn an_undeclared_op_accumulates() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_org(1, "set_mascot", "otter", &["petra", "walter"]);
        b.commit_org(2, "set_mascot", "heron", &["petra", "walter"]);

        let state = checkpoint_state(&b.blocks, 2).expect("state");
        let (_, org) = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .expect("organization entries");
        assert_eq!(org.len(), 2, "an undeclared op must not be summarized away: {org:?}");
    }

    /// **The incremental walk and the batch fold must agree on the summary.**
    ///
    /// A proposer computes a cut's `state_hash` with the batch fold; every
    /// verifier re-checks it with the incremental walk inside `verify_chain`.
    /// A rule that reached one and not the other would leave a republic
    /// unable to gather signatures for ANY cut, and nothing would say why —
    /// which is why `fold_state` delegates to `fold_one` rather than
    /// repeating the match. This test is what keeps that true.
    ///
    /// The chain deliberately mixes both kinds: a superseded slot, an
    /// accumulating note, and a second slot.
    #[test]
    fn the_walk_and_the_fold_summarize_identically() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_org(1, "set_image", "first.png", &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        b.commit_org(3, "set_image", "second.png", &["petra", "walter"]);
        b.commit_org(4, "set_charter", "play more chess", &["petra", "walter"]);

        let folded = checkpoint_state(&b.blocks, 4).expect("batch fold");
        let cut = b.seal(
            5,
            ChainChange::Checkpoint {
                upto: 4,
                state_hash: checkpoint_state_hash(&folded),
            },
            &["petra", "walter"],
        );
        let mut chain = b.blocks.clone();
        chain.push(cut);
        let head = verify_chain(&chain).expect(
            "the walk must reach the same summary the fold did — otherwise no cut is signable",
        );
        assert_eq!(head.height, 5);
    }

    /// **A payload the summary dropped is still an un-re-appliable id.**
    ///
    /// The single most likely thing to be lost by accident here: `applied`
    /// shrinks, so it is tempting to let `consumed_ids` shrink with it. That
    /// would turn every superseded logo back into a proposal a suffix holder
    /// would happily apply again — the double-apply guard, silently repealed
    /// for exactly the entries a cut just summarized away.
    #[test]
    fn a_summarized_away_payload_can_never_re_apply() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_org(1, "set_image", "first.png", &["petra", "walter"]);
        b.commit_org(2, "set_image", "second.png", &["petra", "walter"]);
        let blob = checkpoint_state(&b.blocks, 2).expect("state@2");

        // proposal 1's payload is GONE from the summary…
        let (_, org) = blob
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .expect("organization entries");
        assert_eq!(org.len(), 1, "precondition: the first logo was summarized away");
        assert!(!org.iter().any(|(id, _)| *id == 1), "precondition: id 1's payload is dropped");
        // …and it is still consumed
        assert!(blob.consumed_ids.contains(&1), "the dropped payload's id must survive");

        let cut = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(cut);
        b.commit_org(1, "set_image", "resurrected.png", &["petra", "walter"]);
        let suffix: Vec<ChainBlock> = b.blocks[3..].to_vec();
        assert!(
            verify_suffix_chain(&blob, &suffix, &b.republic_id).is_err(),
            "a summarized-away proposal id re-applied in the suffix — the double-apply \
             guard was repealed for exactly the entries the cut dropped"
        );
    }

    /// **A suffix holder folding onto a summarized blob lands where a full
    /// holder folding from the genesis does.** Without it, the first cut
    /// after a prune would disagree across the republic — the pruned nodes
    /// against the ones that kept their history.
    #[test]
    fn a_suffix_holder_summarizes_onto_the_blob_the_same_way() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_org(1, "set_image", "first.png", &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        // cut at 2, then keep changing the SAME slot past the cut
        let blob = checkpoint_state(&b.blocks, 2).expect("state@2");
        let cut = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(cut);
        b.commit_org(4, "set_image", "second.png", &["petra", "walter"]);

        // the full holder's answer…
        let full = checkpoint_state(&b.blocks, 4).expect("full fold@4");
        // …and the pruned holder's, folding the suffix onto the blob
        let suffix: Vec<ChainBlock> = b.blocks[3..].to_vec();
        let from_blob = fold_state(blob, &suffix, 4).expect("suffix fold@4");

        assert_eq!(
            checkpoint_state_hash(&full),
            checkpoint_state_hash(&from_blob),
            "a pruned holder and a full holder disagree about the summarized state"
        );
        let (_, org) = from_blob
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .expect("organization entries");
        assert_eq!(org.len(), 1, "the blob's superseded image survived the second fold: {org:?}");
        assert_eq!(
            org[0].1.get("value").and_then(serde_json::Value::as_str),
            Some("second.png")
        );
    }

    /// WP4b stage 1: two nodes that hold the SAME chain compute the SAME
    /// checkpoint state, canonical bytes and hash — the property that
    /// makes an m-of-n signature over the hash meaningful. Different
    /// content ⇒ different hash; the founding table inside the state
    /// recomputes to the real republic id (the genesis forgery check
    /// survives the genesis block being dropped later); consumed ids ride
    /// sorted.
    ///
    /// The chains deliberately carry BOTH kinds of applied entry: without a
    /// summarized slot in here, the determinism keystone would say nothing
    /// about the one rule most able to break it — every node must drop the
    /// same superseded entries, or a republic silently loses the ability to
    /// compact at all.
    #[test]
    fn checkpoint_state_is_deterministic_and_binds_the_founding() {
        let mut b1 = Builder::new(&["petra", "walter", "dora"], 2);
        b1.commit_applied(2, &["petra", "walter"]);
        b1.commit_applied(1, &["walter", "dora"]);
        b1.commit_org(3, "set_image", "old.png", &["petra", "walter"]);
        b1.commit_org(4, "set_image", "new.png", &["walter", "dora"]);
        let mut b2 = Builder::new(&["petra", "walter", "dora"], 2);
        b2.commit_applied(2, &["petra", "walter"]);
        b2.commit_applied(1, &["walter", "dora"]);
        b2.commit_org(3, "set_image", "old.png", &["petra", "walter"]);
        b2.commit_org(4, "set_image", "new.png", &["walter", "dora"]);

        let s1 = checkpoint_state(&b1.blocks, 4).expect("state 1");
        let s2 = checkpoint_state(&b2.blocks, 4).expect("state 2");
        assert_eq!(
            checkpoint_state_hash(&s1),
            checkpoint_state_hash(&s2),
            "equal chains yield the identical checkpoint hash"
        );
        // the canonical bytes carry the versioned tag (v6 since the relay
        // ledger joined; v5 the working anchors, v4 the summary rule, v3 the
        // ratified pool — each a change in WHAT the same chain hashes to,
        // which is exactly what the tag exists to announce)
        let bytes = molt_core::checkpoint_canonical_bytes(&s1);
        assert!(bytes.starts_with(b"molt-chain-checkpoint-v6\0"));
        // …and the pool is really covered: a summary whose relays were swapped
        // must not hash the same. Without this the tamper-evidence roster-v4
        // gives the genesis would vanish the moment a republic pruned.
        let mut swapped = s1.clone();
        swapped.relays = vec!["wss://not-what-was-ratified.example".to_string()];
        assert_ne!(
            checkpoint_state_hash(&s1),
            checkpoint_state_hash(&swapped),
            "the checkpoint must bind the ratified pool"
        );
        // consumed ids are sorted regardless of commit order, and the
        // summarized-away logo (3) is still among them
        assert_eq!(s1.consumed_ids, vec![1, 2, 3, 4]);
        // the founding table recomputes to the real republic id — the
        // forgery check a suffix bootstrapper will rely on
        assert_eq!(
            molt_storage::republic_id(
                &s1.founding_name,
                s1.rule_m,
                s1.rule_n,
                &s1.founding_identities
            ),
            s1.republic_id
        );
        // a different cut or different content changes the hash
        let shorter = checkpoint_state(&b1.blocks, 3).expect("shorter cut");
        assert_ne!(checkpoint_state_hash(&s1), checkpoint_state_hash(&shorter));

        // acceptance of checkpoint BLOCKS is pinned by the stage-2 tests
        // (a_checkpoint_block_verifies_against_the_own_projection,
        // a_suffix_chain_bootstraps_from_a_checkpoint)
    }

    /// WP2 pin: the catch-up re-gossip relies on the receive side being
    /// idempotent — a duplicated `Proposed` stays ONE pending entry, a
    /// duplicated `Approved` stays ONE signature per member, and neither
    /// resurrects a proposal whose block already committed.
    #[test]
    fn regossiped_proposals_and_approvals_are_idempotent() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let payload = json!({ "op": "add_note", "title": "minutes" });

        // a re-gossiped Proposed lands once
        walter.receive_proposed(1, Surface::Memory, payload.clone(), "peer");
        walter.receive_proposed(1, Surface::Memory, payload.clone(), "peer");
        let pending: Vec<_> = walter
            .proposals
            .iter()
            .filter(|(_, p)| p.state == ProposalState::Proposed)
            .collect();
        assert_eq!(pending.len(), 1, "one entry, not two");

        // a re-gossiped Approved lands as ONE signature for that member
        let change = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Memory,
            payload: payload.clone(),
        };
        let bytes = approval_bytes(&b.republic_id, 1, &change);
        let petra_sig = identity_sign(b.key("petra"), &bytes);
        walter.receive_approval(1, "petra", 1, &petra_sig);
        walter.receive_approval(1, "petra", 1, &petra_sig);
        let sigs = &walter.pending_sigs.get(&1).expect("pending set").sigs;
        assert_eq!(sigs.len(), 1, "one signature per member: {sigs:?}");

        // walter co-signs — the block seals at 2-of-3
        walter.chain_sign_and_gossip_approval(1);
        assert_eq!(walter.chain_head.as_ref().expect("head").height, 1);
        assert!(
            matches!(walter.proposals.get(&1), Some(p) if p.state == ProposalState::Applied),
            "the proposal committed"
        );

        // LATE re-gossip (another answering peer) must not resurrect it
        walter.receive_proposed(1, Surface::Memory, payload, "peer");
        walter.receive_approval(1, "petra", 1, &petra_sig);
        assert!(
            matches!(walter.proposals.get(&1), Some(p) if p.state == ProposalState::Applied),
            "a committed proposal stays committed"
        );
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            1,
            "no second block for the same proposal"
        );
    }

    /// WP2: whoever answers a `ChainRequest` also re-serves the OPEN
    /// governance state — per open proposal a regular `Proposed` plus the
    /// already-collected `Approved` signatures (verbatim, position-bound —
    /// nothing is re-signed). A reopened member replays those through its
    /// normal receive arms and can then co-sign; the block seals at m.
    #[test]
    fn a_catchup_answer_reserves_open_governance() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        let payload = json!({ "op": "add_note", "title": "minutes" });
        petra
            .cmd_propose(Surface::Memory, payload.clone())
            .expect("petra proposes");

        // what petra's catch-up answer re-gossips: the open proposal and
        // her own collected co-signature
        let bodies = petra.open_governance_events();
        let (mut saw_proposed, mut relayed_sig) = (false, None);
        for body in &bodies {
            match body {
                WorkspaceEvent::Proposed { id, surface, payload: p } => {
                    assert_eq!((id.0, *surface), (1, Surface::Memory));
                    assert_eq!(p, &payload, "the payload rides unchanged");
                    saw_proposed = true;
                }
                WorkspaceEvent::Approved { id, by, height, sig } => {
                    assert_eq!((id.0, by.as_str(), *height), (1, "petra", 1));
                    relayed_sig = Some(sig.clone());
                }
                other => panic!("unexpected re-gossip event: {other:?}"),
            }
        }
        assert!(saw_proposed, "the open proposal is re-served");
        let relayed_sig = relayed_sig.expect("petra's collected signature is re-served");

        // walter — the reopened member: RAM lost the gossip, the chain has
        // only the genesis. The re-gossip restores proposal + count, then
        // his own co-signature seals the block (2-of-2).
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        walter.receive_proposed(1, Surface::Memory, payload, "peer");
        walter.receive_approval(1, "petra", 1, &relayed_sig);
        assert_eq!(
            walter.pending_sigs.get(&1).map(|s| s.sigs.len()),
            Some(1),
            "the reopened member sees the collected approval count"
        );
        walter.chain_sign_and_gossip_approval(1);
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            1,
            "the recovered proposal is fully approvable — the block seals"
        );
    }

    /// A chain-governed member that can also SIGN (holds its identity key).
    fn chain_signer(member: &str, b: &Builder, chain: Vec<ChainBlock>) -> crate::State {
        let mut s = chain_peer(member, b, chain);
        s.identity_sk = Some(b.key(member).clone());
        s
    }

    /// Re-admission (recovery step ❹): a survivor proposes a `Membership{Restored}`
    /// change and, once the threshold of members has signed it (here + "over the
    /// mesh"), a Restored block seals — the group's threshold-gated authorization
    /// of a returning member. Recovery keeps the same anchored identity key.
    #[test]
    fn a_threshold_restored_block_re_admits_a_member() {
        let b = Builder::new(&["petra", "walter"], 2);
        let walter_pk = b.pk("walter");
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        let mut walter = chain_signer("walter", &b, b.blocks.clone());

        // petra proposes re-admitting walter and co-signs (1 of 2 — pending)
        let id = petra.propose_membership(MembershipOp::Restored, "walter", &walter_pk, None, Vec::new(), None);
        assert_eq!(
            petra.chain_head.as_ref().expect("head").height,
            0,
            "one signature does not re-admit"
        );

        // walter learns the proposal + petra's signature, then co-signs
        walter.receive_membership_proposal(id, MembershipOp::Restored, "walter", &walter_pk, None, Vec::new(), None);
        let petra_sig = petra
            .pending_sigs
            .get(&id)
            .expect("petra's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "petra")
            .expect("petra signed")
            .sig
            .clone();
        walter.receive_approval(id, "petra", 1, &petra_sig);
        walter.chain_sign_and_gossip_approval(id);

        // the Restored block seals at 2-of-2
        let head = walter.chain_head.as_ref().expect("head");
        assert_eq!(head.height, 1);
        assert!(
            matches!(
                walter.chain.last().expect("block").change,
                ChainChange::Membership {
                    op: MembershipOp::Restored,
                    ..
                }
            ),
            "the sealed block re-admits the member"
        );
    }

    /// Recovery step ❸: a coordinator re-admits a returning member ONLY on a
    /// valid seat proof against the anchored identity — a forged proof, or a
    /// request that would re-key to a different identity, is refused. A pass
    /// proposes the threshold Restored block.
    #[test]
    fn a_coordinator_re_admits_only_a_valid_seat_proof() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut coord = chain_signer("petra", &b, b.blocks.clone());
        let rid = b.republic_id.clone();
        let ticket = "recovery-ticket-xyz";
        let kp_hex = "beef";

        // the returning member (dora) signs the seat proof with its OWN key
        let good = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
        let id = coord
            .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, ticket, &good, "", &[], "", "")
            .expect("a valid seat proof re-admits");
        assert!(matches!(
            coord.proposal_changes.get(&id),
            Some(ChainChange::Membership {
                op: MembershipOp::Restored,
                ..
            })
        ));
        // a verified request registers the pending recovery (the MLS re-key
        // consumes it the moment the block commits — even synchronously)
        assert!(coord.pending_recovery.contains_key("dora"));

        // a proof signed by the WRONG key (petra forging dora's) is rejected
        let forged = crate::make_seat_proof(b.key("petra"), ticket, kp_hex, &rid, "", &[]);
        assert!(coord
            .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, ticket, &forged, "", &[], "", "")
            .is_err());

        // a request that re-keys the seat to a DIFFERENT identity is rejected —
        // recovery re-derives the SAME key
        assert!(coord
            .verify_and_propose_restore("dora", &b.pk("walter"), kp_hex, ticket, &good, "", &[], "", "")
            .is_err());
    }

    /// The restored member's consent bytes, signed with its own roster key.
    fn consent_for(b: &Builder, member: &str, nostr_pk: &str) -> String {
        molt_storage::identity_sign(
            b.key(member),
            &molt_core::chain::restore_consent_bytes(
                &b.republic_id,
                member,
                &b.pk(member),
                nostr_pk,
            ),
        )
    }

    /// The rejoiner's consent counts as ONE distinct signer (recovery
    /// approval design, 2026-08-08): at m = n the coordinator's single
    /// surviving signature plus a valid consent seals the Restored block —
    /// the case that was a structural dead end before — and the sealed
    /// chain verifies from zero on an adopting reader.
    #[test]
    fn a_consented_restore_seals_at_m_equals_n() {
        let b = Builder::new(&["petra", "walter"], 2);
        let walter_pk = b.pk("walter");
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        let consent = consent_for(&b, "walter", "");
        petra.propose_membership(
            MembershipOp::Restored,
            "walter",
            &walter_pk,
            None,
            Vec::new(),
            Some(consent),
        );
        let head = petra.chain_head.as_ref().expect("head");
        assert_eq!(head.height, 1, "petra's signature + walter's consent reach 2-of-2");
        verify_chain(&petra.chain).expect("an adopting reader accepts the consented block");
    }

    /// Fail-closed on every consent abuse — the whole chain rejects
    /// (verify_chain is all-or-nothing): a forged consent, a consent on a
    /// non-restore change, a double-counted member, and a consent that has
    /// to stand in for EVERY missing signature.
    #[test]
    fn consent_abuse_rejects_the_chain() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let restored = |consent: Option<String>| ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "dora".to_string(),
            identity_pk: b.pk("dora"),
            nostr_pk: None,
            relays: Vec::new(),
            consent,
        };

        // the honest shape: one survivor signature + dora's consent = 2-of-3
        let good = consent_for(&b, "dora", "");
        let mut chain = b.blocks.clone();
        chain.push(b.seal(1, restored(Some(good.clone())), &["petra"]));
        verify_chain(&chain).expect("one survivor + consent reaches m");

        // (a) forged: walter's key cannot consent for dora
        let forged = molt_storage::identity_sign(
            b.key("walter"),
            &molt_core::chain::restore_consent_bytes(
                &b.republic_id,
                "dora",
                &b.pk("dora"),
                "",
            ),
        );
        let mut chain = b.blocks.clone();
        chain.push(b.seal(1, restored(Some(forged)), &["petra"]));
        let err = verify_chain(&chain).expect_err("a forged consent must reject");
        assert!(err.contains("consent"), "the error names the consent: {err}");

        // (b) a consent on a non-restore membership change
        let mut chain = b.blocks.clone();
        chain.push(b.seal(
            1,
            ChainChange::Membership {
                op: MembershipOp::Joined,
                member: "erika".to_string(),
                identity_pk: "aa".repeat(32),
                nostr_pk: None,
                relays: Vec::new(),
                consent: Some(good.clone()),
            },
            &["petra", "walter"],
        ));
        let err = verify_chain(&chain).expect_err("consent on a join must reject");
        assert!(err.contains("non-restore"), "{err}");

        // (c) the restored member must not count twice (consent + signature)
        let mut chain = b.blocks.clone();
        chain.push(b.seal(1, restored(Some(good.clone())), &["dora"]));
        let err = verify_chain(&chain).expect_err("double-counting must reject");
        assert!(err.contains("twice"), "{err}");

        // (d) consent alone is ONE voice — it never reaches m = 2 by itself
        let mut chain = b.blocks.clone();
        chain.push(b.seal(1, restored(Some(good)), &[]));
        let err = verify_chain(&chain).expect_err("consent alone is below threshold");
        assert!(err.contains("threshold"), "{err}");
    }

    /// The approval surface (recovery approval design, 2026-08-08): a
    /// verified request creates a HUMAN-visible proposal record, a survivor
    /// approves it through the PUBLIC `cmd_approve`, and the commit settles
    /// the record to `Applied` with the vote bookkeeping dropped.
    #[test]
    fn a_wire_membership_proposal_is_votable_without_hand_applying() {
        // D3: the applier runs only for the proposer's OWN log, so the wire
        // arm must create the human-facing record itself — without it a
        // receiver held no card, cmd_approve said UnknownProposal, and an
        // m>=3 recovery stalled (coordinator co-sign + rejoiner consent are
        // only 2 distinct signers).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora", "erika"], 3);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let consent = consent_for(&b, "dora", "");
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::MembershipProposed {
                id: ProposalId(5),
                op: MembershipOp::Restored,
                member: "dora".to_string(),
                identity_pk: b.pk("dora"),
                nostr_pk: None,
                relays: Vec::new(),
                consent: Some(consent),
            },
        );
        assert!(
            walter.proposals.contains_key(&5),
            "the receiver holds the votable card"
        );
        walter.cmd_approve(ProposalId(5)).expect("the survivor can approve");
    }

    #[test]
    fn a_membership_proposal_is_a_visible_approvable_record() {
        let b = Builder::new(&["petra", "walter", "dora", "erika"], 3);
        let mut coord = chain_signer("petra", &b, b.blocks.clone());
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let rid = b.republic_id.clone();
        let ticket = "recovery-ticket-xyz";
        let kp_hex = "beef";
        let proof = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
        let consent = consent_for(&b, "dora", "");
        let id = coord
            .verify_and_propose_restore(
                "dora",
                &b.pk("dora"),
                kp_hex,
                ticket,
                &proof,
                "",
                &[],
                &consent,
                "",
            )
            .expect("a valid request proposes");

        // visible on the proposer: a real record with the reserved op
        let rec = coord.proposals.get(&id).expect("the proposer holds a record");
        assert_eq!(rec.payload["op"], "restore_member");
        assert_eq!(rec.payload["member"], "dora");
        assert_eq!(rec.state, ProposalState::Proposed, "2 of 3 voices — still open");

        // …and on a receiver: the gossip's log event creates the SAME record
        let env = walter.make_env(
            "petra".to_string(),
            WorkspaceEvent::MembershipProposed {
                id: ProposalId(id),
                op: MembershipOp::Restored,
                member: "dora".to_string(),
                identity_pk: b.pk("dora"),
                nostr_pk: None,
                relays: Vec::new(),
                consent: Some(consent.clone()),
            },
        );
        walter.apply(&env);
        walter.receive_membership_proposal(
            id,
            MembershipOp::Restored,
            "dora",
            &b.pk("dora"),
            None,
            Vec::new(),
            Some(consent),
        );
        assert_eq!(
            walter.proposals.get(&id).map(|p| p.state),
            Some(ProposalState::Proposed),
            "the receiver sees an open, votable record"
        );
        let petra_sig = coord
            .pending_sigs
            .get(&id)
            .expect("petra's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "petra")
            .expect("petra co-signed")
            .sig
            .clone();
        walter.receive_approval(id, "petra", 1, &petra_sig);

        // the PUBLIC approve — the exact call that answered UnknownProposal
        // before the record existed
        walter.cmd_approve(ProposalId(id)).expect("approve accepts the id");

        // petra + walter + dora's consent = 3-of-4: sealed, settled
        assert_eq!(walter.chain_head.as_ref().expect("head").height, 1);
        assert_eq!(
            walter.proposals.get(&id).map(|p| p.state),
            Some(ProposalState::Applied),
            "the commit settles the record"
        );
        assert!(
            !walter.pending_sigs.contains_key(&id) && !walter.proposal_changes.contains_key(&id),
            "the vote bookkeeping is dropped"
        );
        verify_chain(&walter.chain).expect("the sealed chain verifies from zero");
    }

    /// R6 — the pool is group state any member can move and no member can
    /// move alone: a `set_relays` edit is an ordinary gated Organization
    /// proposal; below threshold the effective pool does not move, at m it
    /// does — for every member folding the same chain.
    #[test]
    fn a_pool_edit_commits_under_threshold_and_moves_the_effective_pool() {
        let pool = vec!["wss://relay.one".to_string()];
        let b = Builder::new_on_relays(&["petra", "walter"], 2, pool.clone());
        // WALTER — not the founder — raises the edit
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({
                    "op": "set_relays",
                    "value": "wss://relay.one wss://relay.three.example",
                }),
            )
            .expect("a member may propose a pool edit");
        let (id, surface, payload) = {
            let (id, rec) = walter.proposals.iter().next().expect("open proposal");
            (*id, rec.surface, rec.payload.clone())
        };
        assert_eq!(
            walter.effective_relays(),
            pool,
            "below threshold the pool must not move"
        );
        // petra learns the proposal + walter's signature, then co-signs
        petra.receive_proposed(id, surface, payload, "peer");
        let walter_sig = walter
            .pending_sigs
            .get(&id)
            .expect("walter's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "walter")
            .expect("walter signed")
            .sig
            .clone();
        petra.receive_approval(id, "walter", 1, &walter_sig);
        petra.chain_sign_and_gossip_approval(id);
        assert_eq!(petra.chain_head.as_ref().expect("head").height, 1, "sealed at m");
        assert_eq!(
            petra.effective_relays(),
            vec!["wss://relay.one".to_string(), "wss://relay.three.example".to_string()],
            "at m the pool moves"
        );
    }

    /// R6 make-before-break (found LIVE 2026-08-09): a pool edit sharing NO
    /// relay with the effective pool is refused outright. The commit that
    /// moves the pool travels over the OLD pool; a member that has not
    /// applied it yet keeps listening there while the members that have
    /// rebuild onto the new pool only — with zero overlap the two sides can
    /// never meet again (a throwaway republic split exactly this way).
    /// A full migration is two votes: add the new relay, then drop the old.
    #[test]
    fn a_pool_edit_sharing_no_relay_with_the_current_pool_is_refused() {
        let pool = vec!["wss://relay.one".to_string()];
        let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let err = walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({
                    "op": "set_relays",
                    "value": "wss://relay.two.example",
                }),
            )
            .expect_err("a zero-overlap pool edit must be refused");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("wss://relay.one"),
            "the refusal names a current relay to keep: {msg}"
        );
        // …and the same target relay passes as make-before-break step one
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({
                    "op": "set_relays",
                    "value": "wss://relay.one wss://relay.two.example",
                }),
            )
            .expect("keeping one shared relay is the legal migration step");
    }

    /// A DECIDED vote appends its summary to its discussion (story
    /// 2026-08-09): the SEALER posts one System message into the patch
    /// channel — so "Discussion" on an accepted vote says what exactly was
    /// decided, and the notice replicates like any chat message instead of
    /// being minted once per node.
    #[test]
    fn a_sealed_vote_appends_its_summary_to_the_discussion() {
        let pool = vec!["wss://relay.one".to_string()];
        let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({
                    "op": "set_relays",
                    "value": "wss://relay.one wss://relay.three.example",
                }),
            )
            .expect("proposes");
        let (id, surface, payload) = {
            let (id, rec) = walter.proposals.iter().next().expect("open proposal");
            (*id, rec.surface, rec.payload.clone())
        };
        petra.receive_proposed(id, surface, payload, "peer");
        let walter_sig = walter
            .pending_sigs
            .get(&id)
            .expect("walter's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "walter")
            .expect("walter signed")
            .sig
            .clone();
        petra.receive_approval(id, "walter", 1, &walter_sig);
        petra.chain_sign_and_gossip_approval(id);
        assert_eq!(petra.chain_head.as_ref().expect("head").height, 1, "sealed at m");
        // the SEALER's log carries the summary, in the vote's own channel
        let sum = petra
            .chat_visible()
            .find(|m| {
                m.kind == molt_core::ChatKind::System
                    && matches!(&m.channel, molt_core::ChannelRef::Patch { id: p } if p.0 == id)
            })
            .expect("the sealed vote posts its summary into the discussion")
            .clone();
        assert!(
            sum.body.contains('✓') && sum.body.contains("relay.three.example"),
            "the summary names the outcome and the decided content: {}",
            sum.body
        );
        // …and the proposer does NOT mint its own copy (it receives the
        // sealer's message over the wire like any chat)
        assert!(
            walter.chat_visible().all(|m| m.kind != molt_core::ChatKind::System),
            "only the sealer appends"
        );
    }

    /// The negative outcome gets the same treatment: the decline that makes
    /// approval unreachable posts the summary, naming the decliner.
    #[test]
    fn a_terminal_decline_appends_its_summary_to_the_discussion() {
        let pool = vec!["wss://relay.one".to_string()];
        let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({ "op": "set_name", "value": "NewName" }),
            )
            .expect("proposes");
        // n = 2, m = 2: one decline makes the threshold unreachable
        walter.cmd_decline(ProposalId(1)).expect("declines");
        let sum = walter
            .chat_visible()
            .find(|m| {
                m.kind == molt_core::ChatKind::System
                    && matches!(&m.channel, molt_core::ChannelRef::Patch { id: p } if p.0 == 1)
            })
            .expect("the terminal decline posts its summary")
            .clone();
        assert!(
            sum.body.contains('⊘')
                && sum.body.contains("walter")
                && sum.body.contains("NewName"),
            "the summary names the outcome, the decliner and the content: {}",
            sum.body
        );
    }

    /// Make-before-break holds at the FOLD, not only at propose (review
    /// 2026-08-09): the propose gate is local courtesy — a peer on another
    /// build can gossip a zero-overlap edit, and two individually-legal
    /// pending edits can compose into one. The fold is the only place every
    /// node passes deterministically, so an applied `set_relays` sharing no
    /// relay with the pool accumulated SO FAR is a no-op — a pure function
    /// of chain content, identical on every holder.
    #[test]
    fn a_zero_overlap_pool_block_folds_as_a_no_op() {
        let r_a = "wss://relay.one".to_string();
        let r_b = "wss://relay.two.example".to_string();
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, vec![r_a.clone()]);
        let block = |b: &Builder, h, value: &str| {
            b.seal(
                h,
                ChainChange::Applied {
                    proposal_id: h,
                    surface: Surface::Organization,
                    payload: serde_json::json!({ "op": "set_relays", "value": value }),
                },
                &["petra", "walter"],
            )
        };
        // height 1: zero overlap with [A] — must keep [A]
        let zero = block(&b, 1, &r_b);
        b.push(zero);
        let walter = chain_signer("walter", &b, b.blocks.clone());
        assert_eq!(walter.effective_relays(), vec![r_a.clone()], "zero overlap folds as no-op");
        // height 2: [A B] overlaps via A — applies; height 3: [B] overlaps
        // via B — applies. The legal two-vote migration lands on [B].
        let step = block(&b, 2, &format!("{r_a} {r_b}"));
        b.push(step);
        let done = block(&b, 3, &r_b);
        b.push(done);
        let walter = chain_signer("walter", &b, b.blocks.clone());
        assert_eq!(walter.effective_relays(), vec![r_b], "the two-vote migration applies");
    }

    /// Charter features D5: a `set_features` edit is an ordinary gated
    /// Organization proposal — below threshold the effective set does not
    /// move, at m it does, for every member folding the same chain. The
    /// legacy baseline (D6) is Shared Memory: this republic was founded
    /// pre-v5 (`features: None`), so `memory` is on and everything else off
    /// until voted in.
    #[test]
    fn a_feature_edit_commits_under_threshold_and_moves_the_effective_set() {
        let pool = vec!["wss://relay.one".to_string()];
        let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        assert_eq!(
            walter.effective_features(),
            vec!["memory".to_string()],
            "the legacy baseline is Shared Memory"
        );
        // WALTER — not the founder — raises the edit, deliberately unsorted
        // with a duplicate: the proposal is stored canonicalized
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({
                    "op": "set_features",
                    "value": "quests memory quests",
                }),
            )
            .expect("a member may propose a feature edit");
        let (id, surface, payload) = {
            let (id, rec) = walter.proposals.iter().next().expect("open proposal");
            (*id, rec.surface, rec.payload.clone())
        };
        assert_eq!(
            payload.get("value").and_then(serde_json::Value::as_str),
            Some("memory quests"),
            "the proposal carries the canonical set"
        );
        assert_eq!(
            walter.effective_features(),
            vec!["memory".to_string()],
            "below threshold the set must not move"
        );
        petra.receive_proposed(id, surface, payload, "peer");
        let walter_sig = walter
            .pending_sigs
            .get(&id)
            .expect("walter's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "walter")
            .expect("walter signed")
            .sig
            .clone();
        petra.receive_approval(id, "walter", 1, &walter_sig);
        petra.chain_sign_and_gossip_approval(id);
        assert_eq!(petra.chain_head.as_ref().expect("head").height, 1, "sealed at m");
        assert_eq!(
            petra.effective_features(),
            vec!["memory".to_string(), "quests".to_string()],
            "at m the set moves"
        );
    }

    /// Enable-only at propose time: dropping an enabled feature, re-enabling
    /// the current set unchanged, and an unknown key are all refused before
    /// anything reaches the members.
    #[test]
    fn a_feature_edit_that_shrinks_repeats_or_invents_is_refused() {
        let pool = vec!["wss://relay.one".to_string()];
        let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let propose = |st: &mut crate::State, value: &str| {
            st.cmd_propose(
                Surface::Organization,
                serde_json::json!({ "op": "set_features", "value": value }),
            )
        };
        let err = propose(&mut walter, "quests").expect_err("dropping memory must be refused");
        assert!(format!("{err:?}").contains("memory: cannot be disabled"), "{err:?}");
        let err = propose(&mut walter, "memory").expect_err("a no-op must be refused");
        assert!(format!("{err:?}").contains("already enabled"), "{err:?}");
        let err = propose(&mut walter, "memory kanban").expect_err("an unknown key must be refused");
        assert!(format!("{err:?}").contains("unknown feature: kanban"), "{err:?}");
        let err = propose(&mut walter, "").expect_err("an empty edit must be refused");
        assert!(format!("{err:?}").contains("nothing to enable"), "{err:?}");
    }

    /// Enable-only holds at the FOLD, not only at propose: the fold is a
    /// UNION, so a hand-built block that "drops" a feature (bypassing every
    /// propose-time gate) folds as pure addition on every holder. This is
    /// the deterministic twin — without it, "features can never be switched
    /// off" would be local courtesy.
    #[test]
    fn a_feature_dropping_block_folds_as_a_union() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        // height 1: "quests" alone — as a REPLACEMENT it would drop memory
        let drop = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 1,
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_features", "value": "quests" }),
            },
            &["petra", "walter"],
        );
        b.push(drop);
        let walter = chain_signer("walter", &b, b.blocks.clone());
        assert_eq!(
            walter.effective_features(),
            vec!["memory".to_string(), "quests".to_string()],
            "a dropping block folds as a union - nothing is ever disabled"
        );
    }

    /// shared_memory_real.md WP-B keystone: memory's applied entries are
    /// ACCUMULATING at a checkpoint cut (`applied_lww_slot` = None), so
    /// the fold over the summarized state is byte-identical to the fold
    /// over the full chain — a cut can never fork the wiki.
    #[test]
    fn a_checkpoint_cut_keeps_the_wiki_fold_identical() {
        const ADD_A: &str = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
        const EDIT_A: &str = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n-hello\n+hallo\n world\n";
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        for (h, id, patch) in [(1u64, 10u64, ADD_A), (2, 11, EDIT_A)] {
            let block = b.seal(
                h,
                ChainChange::Applied {
                    proposal_id: id,
                    surface: Surface::Memory,
                    payload: serde_json::json!({ "op": "wiki_patch", "value": patch }),
                },
                &["petra", "walter"],
            );
            b.push(block);
        }
        let full = chain_signer("walter", &b, b.blocks.clone());
        let full_tree = full.wiki_tree();
        assert_eq!(
            full_tree.get("a.md").map(String::as_str),
            Some("hallo\nworld\n")
        );
        let state = checkpoint_state(&b.blocks, 2).expect("summary");
        let mem: Vec<serde_json::Value> = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Memory)
            .map(|(_, entries)| entries.iter().map(|(_, p)| p.clone()).collect())
            .expect("memory summary");
        assert_eq!(
            molt_core::wiki_fold::wiki_fold(&mem),
            full_tree,
            "a cut keeps the fold byte-identical"
        );
    }

    /// Two racing enables both survive a compaction cut: `set_features`
    /// entries ACCUMULATE in the checkpoint summary (deliberately no
    /// `applied_lww_slot` — an LWW summary would keep only the later value
    /// and silently lose the other vote's addition across the cut).
    #[test]
    fn racing_feature_enables_both_survive_a_checkpoint_cut() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        // two independently-proposed enables, each a superset of the
        // baseline but not of each other (the race)
        for (h, value) in [(1, "memory quests"), (2, "memory vault")] {
            let block = b.seal(
                h,
                ChainChange::Applied {
                    proposal_id: h,
                    surface: Surface::Organization,
                    payload: serde_json::json!({ "op": "set_features", "value": value }),
                },
                &["petra", "walter"],
            );
            b.push(block);
        }
        let state = checkpoint_state(&b.blocks, 2).expect("summary");
        let org = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .map(|(_, entries)| entries)
            .expect("organization summary");
        let kept: Vec<&str> = org
            .iter()
            .filter_map(|(_, p)| p.get("value").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(
            kept,
            vec!["memory quests", "memory vault"],
            "both racing enables survive the summary"
        );
        // …and the union over the summarized entries is the full set
        let walter = chain_signer("walter", &b, b.blocks.clone());
        assert_eq!(
            walter.effective_features(),
            vec!["memory".to_string(), "quests".to_string(), "vault".to_string()],
        );
    }

    /// **A cut must not carry every superseded avatar forever**
    /// (`member_profiles_plan.md` §3): the profile ops hold per-member LWW
    /// slots, so the summary keeps the LATEST picture and description per
    /// seat — one seat's edit never drops another's — and the fold over the
    /// summarized entries equals the fold over the full chain.
    #[test]
    fn a_checkpoint_cut_keeps_only_the_latest_avatar_per_member() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let entries = [
            ("set_member_image", "petra", "old.png", "b2xk"),
            ("set_member_desc", "petra", "typo", ""),
            ("set_member_image", "walter", "walter.png", "d2FsdGVy"),
            ("set_member_image", "petra", "new.png", "bmV3"),
            ("set_member_desc", "petra", "keeps the bees", ""),
        ];
        for (h, (op, member, value, bytes)) in entries.iter().enumerate() {
            let height = u64::try_from(h + 1).expect("small height");
            let mut payload = serde_json::json!({ "op": op, "member": member, "value": value });
            if !bytes.is_empty() {
                payload["bytes_b64"] = serde_json::Value::String((*bytes).to_string());
            }
            let block = b.seal(
                height,
                ChainChange::Applied {
                    proposal_id: height,
                    surface: Surface::Organization,
                    payload,
                },
                &["petra", "walter"],
            );
            b.push(block);
        }
        let state = checkpoint_state(&b.blocks, 5).expect("summary");
        let org: Vec<(Option<u64>, serde_json::Value)> = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .map(|(_, e)| e.iter().map(|(id, p)| (Some(*id), p.clone())).collect())
            .expect("organization summary");
        let kept: Vec<&str> = org
            .iter()
            .filter_map(|(_, p)| p.get("value").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(
            kept,
            vec!["walter.png", "new.png", "keeps the bees"],
            "the cut must keep exactly the latest entry per member and field"
        );

        // the post-cut fold is the live fold
        let full = chain_signer("walter", &b, b.blocks.clone());
        let live: Vec<(String, String, String)> = full
            .member_profiles()
            .iter()
            .map(|(m, p)| ((*m).to_string(), p.image.clone(), p.desc.to_string()))
            .collect();
        let mut cut = chain_signer("walter", &b, vec![b.blocks[0].clone()]);
        cut.chain_applied.insert(Surface::Organization, org);
        let after: Vec<(String, String, String)> = cut
            .member_profiles()
            .iter()
            .map(|(m, p)| ((*m).to_string(), p.image.clone(), p.desc.to_string()))
            .collect();
        assert_eq!(after, live, "a cut must not change what the profiles fold to");
        assert_eq!(
            live,
            vec![
                ("petra".to_string(), "new.png".to_string(), "keeps the bees".to_string()),
                ("walter".to_string(), "walter.png".to_string(), String::new()),
            ]
        );
    }

    /// D7: the engine refuses selecting and proposing on a surface the
    /// charter has not enabled — the co-equal twin of the nav hiding it —
    /// and an enabling block opens the same gate for every holder.
    #[test]
    fn a_disabled_surface_refuses_select_and_propose_until_enabled() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        // legacy baseline {memory}: memory passes, quests is refused
        walter.cmd_select_surface(Surface::Memory).expect("memory is enabled");
        let err = walter
            .cmd_select_surface(Surface::Quests)
            .expect_err("selecting a disabled surface must be refused");
        assert_eq!(format!("{err}"), "quests: not enabled");
        let err = walter
            .cmd_select_view(Surface::Quests, "board".to_string())
            .expect_err("selecting a disabled surface's view must be refused");
        assert_eq!(format!("{err}"), "quests: not enabled");
        let err = walter
            .cmd_propose(Surface::Quests, serde_json::json!({ "op": "x", "value": "y" }))
            .expect_err("proposing on a disabled surface must be refused");
        assert_eq!(format!("{err}"), "quests: not enabled");
        // the enabling block opens the gate
        let enable = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 1,
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
            },
            &["petra", "walter"],
        );
        b.push(enable);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        walter.cmd_select_surface(Surface::Quests).expect("an enabled surface passes");
        // …and the effective set is on the status surface (co-equal read)
        assert_eq!(
            walter.status().features,
            vec!["memory".to_string(), "quests".to_string()],
        );
    }

    /// D7's approve half (review 2026-08-12): a peer's proposal on a
    /// disabled surface lands in the pool (ingest is tolerant — the
    /// enabling block may simply not have applied here yet), but no
    /// signature leaves this node for it, so it can never reach m honest
    /// seats. Once the feature is enabled the same approval passes.
    #[test]
    fn an_approval_on_a_disabled_surface_is_refused_until_enabled() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        // a peer proposal on quests (disabled: legacy baseline is {memory})
        walter.receive_proposed(
            9,
            Surface::Quests,
            serde_json::json!({ "op": "add_quest", "title": "t" }),
            "peer",
        );
        let err = walter
            .cmd_approve(molt_core::ProposalId(9))
            .expect_err("no signature may leave for a disabled surface");
        assert_eq!(format!("{err}"), "quests: not enabled");
        // the enabling block opens the gate for the SAME proposal
        let enable = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 1,
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
            },
            &["petra", "walter"],
        );
        b.push(enable);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        walter.receive_proposed(
            9,
            Surface::Quests,
            serde_json::json!({ "op": "add_quest", "title": "t" }),
            "peer",
        );
        walter
            .cmd_approve(molt_core::ProposalId(9))
            .expect("an enabled surface accepts the approval");
    }

    /// Review 2026-08-12 (mixed versions): an unknown key can become
    /// effective here via a NEWER build's applied block (wire ingest never
    /// runs this build's validate). The enable-only gate must not demand a
    /// key this build cannot name — validate would refuse it — and the
    /// union fold keeps it regardless, so feature governance keeps working.
    #[test]
    fn an_unknown_effective_key_does_not_brick_feature_governance() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let newer = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 1,
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_features", "value": "memory zzz" }),
            },
            &["petra", "walter"],
        );
        b.push(newer);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        assert!(
            walter.effective_features().iter().any(|f| f == "zzz"),
            "the unknown key is effective (union fold keeps it)"
        );
        // this build proposes WITHOUT the key it cannot name — accepted
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({ "op": "set_features", "value": "memory quests" }),
            )
            .expect("an unknown effective key must not brick the gates");
        // …and the fold still keeps zzz alongside the new enable. Select
        // the OPEN card: adoption materialized the applied block's card too
        let (id, surface, payload) = {
            let (id, rec) = walter
                .proposals
                .iter()
                .find(|(_, p)| p.state == ProposalState::Proposed)
                .expect("open proposal");
            (*id, rec.surface, rec.payload.clone())
        };
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        petra.receive_proposed(id, surface, payload, "peer");
        let walter_sig = walter
            .pending_sigs
            .get(&id)
            .expect("walter's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "walter")
            .expect("walter signed")
            .sig
            .clone();
        petra.receive_approval(id, "walter", 2, &walter_sig);
        petra.chain_sign_and_gossip_approval(id);
        assert_eq!(
            petra.effective_features(),
            vec![
                "memory".to_string(),
                "quests".to_string(),
                "zzz".to_string()
            ],
            "the union keeps what this build cannot name"
        );
    }

    /// The mint counter stays ahead of chain-consumed proposal ids (review
    /// 2026-08-12): a holder that adopted its chain WITHOUT the ephemeral
    /// event log (a blob-seeded rejoiner after total loss) would otherwise
    /// mint an id the chain already decided — every peer's ingest refuses
    /// that as a stale resend, so the proposal could never seal: a silent
    /// governance-liveness hole.
    #[test]
    fn a_fresh_adopter_never_mints_a_chain_consumed_proposal_id() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let enable = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 1,
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
            },
            &["petra", "walter"],
        );
        b.push(enable);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({ "op": "set_features", "value": "memory quests vault" }),
            )
            .expect("propose");
        // the OPEN card (adoption materialized the applied block's card too)
        let (id, rec) = walter
            .proposals
            .iter()
            .find(|(_, p)| p.state == ProposalState::Proposed)
            .expect("open proposal");
        assert!(*id > 1, "the consumed id 1 must be skipped, got {id}");
        // and a peer registers it instead of refusing a "stale resend"
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        assert!(
            petra.receive_proposed(*id, rec.surface, rec.payload.clone(), "peer"),
            "the peer registers the freshly minted id"
        );
    }

    /// The baseline rule (D6): a v5 founding's ratified selection IS the
    /// baseline — `Some([])` means "nothing optional", never the legacy
    /// Shared-Memory grandfather, and an explicit selection replaces it.
    #[test]
    fn the_feature_baseline_follows_the_ratified_selection() {
        let b = Builder::new_on_relays(&["petra", "walter"], 2, Vec::new());
        let mut st = chain_signer("walter", &b, b.blocks.clone());
        if let Some(r) = st.replica.as_mut() {
            r.features = Some(Vec::new());
        }
        assert_eq!(
            st.effective_features(),
            Vec::<String>::new(),
            "an explicitly empty selection enables nothing"
        );
        if let Some(r) = st.replica.as_mut() {
            r.features = Some(vec!["wallet".to_string()]);
        }
        assert_eq!(st.effective_features(), vec!["wallet".to_string()]);
    }

    /// R6: an edit that would strand a member — a new pool sharing no relay
    /// with what that member is on record as reaching — is refused at
    /// propose time, naming the member and its relay (the R4 split it would
    /// otherwise commit).
    #[test]
    fn a_pool_edit_that_would_strand_a_member_is_refused() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        // petra is on record as reaching ONLY relay.two
        let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
        let restored = b.seal(
            1,
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member: "petra".to_string(),
                identity_pk: b.pk("petra"),
                nostr_pk: Some(fresh),
                relays: vec!["wss://relay.two.example".to_string()],
                consent: None,
            },
            &["petra", "walter"],
        );
        b.push(restored);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let err = walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({
                    "op": "set_relays",
                    "value": "wss://relay.three.example",
                }),
            )
            .expect_err("a pool that strands a member must be refused");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("petra") && msg.contains("wss://relay.two.example"),
            "the refusal names the stranded member and its relay: {msg}"
        );
    }

    /// R5 — the re-join gate: a declaration that shares no relay with some
    /// member is refused, NAMING the relay the others must add — that
    /// message is the whole feature. The same declaration passes once the
    /// pool carries the relay.
    #[test]
    fn a_rejoin_over_a_foreign_relay_is_refused_naming_it() {
        let ticket = "recovery-ticket-r5";
        let kp_hex = "beef";
        let declared = vec!["wss://relay.two.example".to_string()];

        // republic pool: relay.one only — dora's declared relay bridges nobody
        let b = Builder::new_on_relays(
            &["petra", "walter", "dora"],
            2,
            vec!["wss://relay.one".to_string()],
        );
        let mut coord = chain_signer("petra", &b, b.blocks.clone());
        let proof = crate::make_seat_proof(
            b.key("dora"),
            ticket,
            kp_hex,
            &b.republic_id,
            "",
            &declared,
        );
        let err = coord
            .verify_and_propose_restore(
                "dora",
                &b.pk("dora"),
                kp_hex,
                ticket,
                &proof,
                "",
                &declared,
                "",
                "",
            )
            .expect_err("a declaration bridging nobody must be refused");
        assert!(
            err.contains("wss://relay.two.example") && err.contains("add"),
            "the refusal names the relay the others must add: {err}"
        );

        // the SAME declaration passes once the pool carries the relay
        let b2 = Builder::new_on_relays(
            &["petra", "walter", "dora"],
            2,
            vec!["wss://relay.one".to_string(), "wss://relay.two.example".to_string()],
        );
        let mut coord2 = chain_signer("petra", &b2, b2.blocks.clone());
        let proof2 = crate::make_seat_proof(
            b2.key("dora"),
            ticket,
            kp_hex,
            &b2.republic_id,
            "",
            &declared,
        );
        let id = coord2
            .verify_and_propose_restore(
                "dora",
                &b2.pk("dora"),
                kp_hex,
                ticket,
                &proof2,
                "",
                &declared,
                "",
                "",
            )
            .expect("the same declaration passes once the pool carries it");
        // …and the block carries the seat's OWN declaration (its ledger entry)
        assert!(matches!(
            coord2.proposal_changes.get(&id),
            Some(ChainChange::Membership { relays, .. }) if *relays == declared
        ));
    }

    /// When a `Restored` block commits, the coordinator (the node holding the
    /// pending recovery for that member) consumes it to drive the MLS re-key;
    /// a node without a pending recovery for that member does nothing. Here
    /// there is no runtime group, so the re-key is a logged no-op — but the
    /// trigger CONDITION (consume the pending recovery on commit) is exercised.
    #[test]
    fn a_restored_commit_triggers_the_coordinators_rekey() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let walter_pk = b.pk("walter");
        let mut coord = chain_signer("petra", &b, b.blocks.clone());
        coord.pending_recovery.insert(
            "walter".to_string(),
            PendingRecovery {
                member: "walter".to_string(),
                key_package: "beef".to_string(),
                reply: String::new(),
            },
        );

        // build a Restored block for walter and hand it to the coordinator
        let change = ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "walter".to_string(),
            identity_pk: walter_pk,
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        };
        let block = b.seal(1, change, &["petra", "walter"]);
        coord.receive_block(block);

        assert_eq!(coord.chain_head.as_ref().expect("head").height, 1);
        assert!(
            !coord.pending_recovery.contains_key("walter"),
            "the coordinator consumed the pending recovery on the Restored commit"
        );
    }

    /// **Re-mint failover (decision A1, 2026-07-11), chain level.** When the
    /// recovery coordinator dies, any survivor mints a NEW recovery link and a
    /// complete second recovery round runs — producing a SECOND `Restored`
    /// block for the SAME seat. The chain must accept it: same anchored
    /// `identity_pk` at two consecutive heights (only the MLS leaf re-keys
    /// again; the roster identity never moves). Counter-assertion: a `Restored`
    /// block that re-keys the roster identity to a DIFFERENT key is rejected
    /// (`recovery_ritual.md` §6 — rotation is out of scope; the coordinator's
    /// refusal to *propose* such a change is pinned separately in
    /// `a_coordinator_re_admits_only_a_valid_seat_proof`).
    #[test]
    fn a_second_restored_block_for_the_same_seat_verifies() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let walter_pk = b.pk("walter");
        let restored = ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "walter".to_string(),
            identity_pk: walter_pk.clone(),
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        };
        // round 1: the first recovery attempt's Restored block commits …
        let block = b.seal(1, restored.clone(), &["petra", "walter"]);
        b.push(block);
        // … then the coordinator dies; the re-mint failover runs a COMPLETE
        // second round: a second Restored block for the same seat, same key
        let block = b.seal(2, restored, &["petra", "walter"]);
        b.push(block);
        let head = verify_chain(&b.blocks).expect("two Restored blocks for one seat verify");
        assert_eq!(head.height, 2);
        assert_eq!(
            head.identities
                .iter()
                .find(|i| i.member == "walter")
                .expect("walter stays anchored")
                .identity_pk,
            walter_pk,
            "recovery re-keys the MLS leaf, never the roster identity"
        );

        // counter: a threshold of survivors must NOT be able to swap the seat
        // to a different identity key via a Restored block — hard-reject
        let (_, other_pk) = derive_identity_key(&[42u8; 32], "walter");
        let hijack = ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "walter".to_string(),
            identity_pk: other_pk,
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        };
        let block = b.seal(3, hijack, &["petra", "walter"]);
        b.push(block);
        assert!(
            verify_chain(&b.blocks).is_err(),
            "a Restored block with a non-anchored identity key must be rejected"
        );
    }

    /// A three-member MLS group (`coord`, `walter`, `dora`) — the shape a
    /// coordinator re-keys from.
    fn mls_trio() -> (molt_net::MlsMember, molt_net::MlsMember, molt_net::MlsMember) {
        let key = |n: u8| SigningKey::from_bytes(&[n; 32]);
        let mut coord = molt_net::MlsMember::new(&key(1), "coord").expect("coord");
        let walter = molt_net::MlsMember::new(&key(2), "walter").expect("walter");
        let dora = molt_net::MlsMember::new(&key(3), "dora").expect("dora");
        coord.create_group().expect("create");
        let welcome = coord
            .add_members(&[
                walter.key_package().expect("walter kp"),
                dora.key_package().expect("dora kp"),
            ])
            .expect("add")
            .expect("welcome");
        let (mut walter, mut dora) = (walter, dora);
        walter.join_from_welcome(&welcome).expect("walter joins");
        dora.join_from_welcome(&welcome).expect("dora joins");
        (coord, walter, dora)
    }

    /// **The Nostr re-key seals under the epoch its recipients are still at**
    /// (N4b step 6c, the `9900f36` lesson re-pinned at the production entry
    /// point).
    ///
    /// A receiver's exporter ring reaches BACKWARD only. So a commit whose
    /// outer layer is sealed at the epoch the coordinator just moved TO is
    /// opaque to exactly the members it exists to move forward — and the
    /// whole recovery is undeliverable, silently, because an opaque frame
    /// looks like relay spam. The negative half is the test: the NEW epoch's
    /// exporter must NOT open it.
    #[test]
    fn a_nostr_rekey_commit_opens_for_the_survivors_it_is_meant_for() {
        // dora is the SURVIVOR here — walter is the seat being restored, and
        // its old leaf is evicted by the very commit under test
        let (coord, _walter, survivor) = mls_trio();
        // walter lost everything and re-derives the SAME identity
        let returning =
            molt_net::MlsMember::new(&SigningKey::from_bytes(&[2u8; 32]), "walter").expect("kp");
        let kp = returning.key_package().expect("key package");

        let survivor_secrets = {
            let mut v = vec![survivor.exporter_secret().expect("survivor exporter")];
            v.extend_from_slice(survivor.exporter_ring());
            v
        };
        let mls = std::sync::Mutex::new(coord);
        let rekey = nostr_rekey(&mls, "walter", &kp, 1_759_000_000).expect("the re-key runs");

        // the commit, sealed the way the delivery task seals it…
        let sealed = molt_net::envelope::seal_outer(&rekey.prev_exporter, &rekey.commit)
            .expect("seal the commit");
        assert!(
            molt_net::envelope::open_outer(&survivor_secrets, &sealed).is_ok(),
            "a survivor that has NOT yet merged the commit cannot open it — the whole \
             re-key is undeliverable to exactly the members it is for"
        );
        // …and the counter-case: the epoch the coordinator moved TO must not
        // be what it sealed under, or the assertion above passes by accident
        let new_epoch = mls.lock().expect("lock").exporter_secret().expect("new exporter");
        assert_ne!(
            new_epoch, rekey.prev_exporter,
            "the commit was sealed at the coordinator's NEW epoch — backward-only \
             exporter rings make that opaque to every survivor"
        );
    }

    /// The stamp the commit is KEYED with is the stamp it is carried at.
    ///
    /// `CommitKey(created_at, digest)` breaks a concurrent same-epoch race,
    /// and both ends must derive it from the same value — the 445 receive side
    /// reads the real `created_at` off the wire. A coordinator that let the
    /// outbox pick the publish time would key its own commit at one value
    /// while every receiver keys it at another, and the two would pick
    /// different winners under ONE epoch number, silently.
    #[test]
    fn the_rekey_carries_the_stamp_it_was_keyed_with() {
        let (coord, _survivor, _dora) = mls_trio();
        let returning =
            molt_net::MlsMember::new(&SigningKey::from_bytes(&[2u8; 32]), "walter").expect("kp");
        let kp = returning.key_package().expect("key package");

        let pinned = 1_759_123_456;
        let mls = std::sync::Mutex::new(coord);
        let rekey = nostr_rekey(&mls, "walter", &kp, pinned).expect("the re-key runs");
        assert_eq!(
            rekey.stamp, pinned,
            "the re-key must carry its own pinned stamp — the delivery has no other \
             source for it, and re-reading a clock is exactly the divergence"
        );
    }

    /// The Welcome really admits the returning seat: it is the whole point of
    /// the re-key, and a commit that produced an unusable Welcome would still
    /// satisfy both tests above.
    #[test]
    fn the_rekey_welcome_puts_the_returning_seat_back_in_the_group() {
        let (coord, _walter, mut survivor) = mls_trio();
        let mut returning =
            molt_net::MlsMember::new(&SigningKey::from_bytes(&[2u8; 32]), "walter").expect("kp");
        let kp = returning.key_package().expect("key package");

        let mls = std::sync::Mutex::new(coord);
        let rekey = nostr_rekey(&mls, "walter", &kp, 1_759_000_000).expect("the re-key runs");

        // the survivor merges the commit and reaches the new epoch
        match survivor.decrypt(&rekey.commit).expect("survivor processes the commit") {
            molt_net::mls::MlsIncoming::Commit { .. } => {}
            other => panic!("expected a commit, got {other:?}"),
        }
        returning.join_from_welcome(&rekey.welcome).expect("the seat rejoins");
        // …and the two can now talk, which is what "recovered" means
        let ct = returning.encrypt(b"back").expect("encrypt");
        match survivor.decrypt(&ct).expect("survivor reads the rejoiner") {
            molt_net::mls::MlsIncoming::Application { from, plaintext } => {
                assert_eq!(from, "walter");
                assert_eq!(plaintext, b"back");
            }
            other => panic!("expected an application message, got {other:?}"),
        }
    }

    /// **Re-mint failover, engine level: a survivor (or a restarted, amnesiac
    /// coordinator) adopting a committed `Restored` block it holds NO pending
    /// recovery for is inert.** The chain extends normally, but
    /// `coordinator_rekey` never runs: nothing is recorded (no
    /// `WorkspaceEvent::MlsCommit` broadcast), the mesh window is not armed,
    /// and a pending recovery for a DIFFERENT member is left untouched. This
    /// is the crash-before-re-key case: the block committed, the coordinator
    /// died, and the re-mint failover's second round supplies the re-key.
    #[test]
    fn a_restored_commit_without_a_pending_recovery_is_inert() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let walter_pk = b.pk("walter");
        let mut node = chain_signer("petra", &b, b.blocks.clone());
        // a pending recovery for ANOTHER member must survive walter's commit
        node.pending_recovery.insert(
            "dora".to_string(),
            PendingRecovery {
                member: "dora".to_string(),
                key_package: "beef".to_string(),
                reply: String::new(),
            },
        );
        let seq_before = node.next_seq;

        // a Restored block for walter — committed elsewhere — arrives; this
        // node holds no pending recovery for walter
        let change = ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "walter".to_string(),
            identity_pk: walter_pk,
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        };
        let block = b.seal(1, change, &["petra", "walter"]);
        node.receive_block(block);

        // the chain extends …
        assert_eq!(node.chain_head.as_ref().expect("head").height, 1);
        // … but the re-key trigger stayed inert: no envelope of any kind was
        // recorded (make_env is the only seq stamp, so an MlsCommit broadcast
        // or a chat notice would have advanced next_seq) …
        assert_eq!(node.next_seq, seq_before, "no MlsCommit/notice was recorded");
        // … the recovery mesh window was never armed …
        assert!(node.recovery_mesh_window.is_empty());
        // … and only walter's (absent) entry was consulted — dora's pending
        // recovery is untouched
        assert!(node.pending_recovery.contains_key("dora"));
    }

    /// A rejoiner that lost everything (no chain, no head) bootstraps from the
    /// genesis a survivor serves and then catches up the whole chain — even when
    /// later blocks arrive before the genesis (they buffer until it lands). The
    /// state-recovery core of Phase 4.
    #[test]
    fn a_headless_rejoiner_bootstraps_from_a_served_genesis() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis_block = b.blocks[0].clone();
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let block1 = b.blocks[1].clone();
        let block2 = b.blocks[2].clone();

        let mut rejoiner = crate::tests::plain_state();
        assert!(!rejoiner.is_chain_governed());

        // a block arrives before the genesis — buffered, still headless
        rejoiner.receive_block(block2);
        assert!(!rejoiner.is_chain_governed());
        assert_eq!(rejoiner.pending_blocks.len(), 1);

        // the survivor serves the genesis — adopt it as the root
        rejoiner.receive_block(genesis_block);
        assert!(rejoiner.is_chain_governed(), "adopted the served genesis");
        assert_eq!(rejoiner.chain_head.as_ref().expect("head").height, 0);

        // the middle block fills the gap; the buffered tail drains behind it
        rejoiner.receive_block(block1);
        assert_eq!(
            rejoiner.chain_head.as_ref().expect("head").height,
            2,
            "the rejoiner caught up the full chain from genesis"
        );
        assert!(rejoiner.pending_blocks.is_empty());
    }

    /// The co-equal Chain-History read (`Command::ReadChain`): every committed
    /// block newest first with the right kinds, the checkpoint block visible —
    /// and after the auto-drop, the pruned holder still lists the pre-cut
    /// applied entries as synthetic views from its checkpoint blob (height 0:
    /// the per-entry heights are gone with the history).
    #[test]
    fn read_chain_lists_blocks_newest_first_and_survives_the_prune() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());

        // full holder: genesis + the two applied blocks, newest first
        let molt_core::Reply::Chain { blocks } = walter.cmd_read_chain().expect("read") else {
            panic!("read_chain answers Reply::Chain");
        };
        assert_eq!(
            blocks
                .iter()
                .map(|v| (v.height, v.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![(2, "applied"), (1, "applied"), (0, "genesis")]
        );
        assert_eq!(blocks[0].proposal_id, 2, "the applied view names its proposal");
        assert_eq!(blocks[0].surface, "memory");
        assert_eq!(blocks[0].payload["op"], json!("add_note"));
        assert_eq!(
            blocks[0].signers,
            vec!["petra".to_string(), "walter".to_string()],
            "the signers ride the view in block order"
        );
        assert_eq!(blocks[2].payload, json!("Chess Club"), "the genesis shows the name");
        assert_eq!(blocks[2].surface, "");
        assert_eq!(blocks[2].proposal_id, 0);

        // seal the checkpoint cut at the head (stage-3 mechanics) → auto-drop
        let hash = checkpoint_state_hash(&checkpoint_state(&b.blocks, 2).expect("state"));
        walter.receive_checkpoint_proposal(40, 2, &hash);
        let change = ChainChange::Checkpoint { upto: 2, state_hash: hash };
        let bytes = approval_bytes(&b.republic_id, 3, &change);
        let petra_sig = identity_sign(b.key("petra"), &bytes);
        walter.receive_approval(40, "petra", 3, &petra_sig);
        assert_eq!(walter.chain.len(), 1, "history below the cut is dropped");

        // pruned holder: the real anchor keeps its height, then the synthetic
        // pre-cut applied views (newest first, signers gone), genesis last
        let molt_core::Reply::Chain { blocks } = walter.cmd_read_chain().expect("read") else {
            panic!("read_chain answers Reply::Chain");
        };
        assert_eq!(
            blocks
                .iter()
                .map(|v| (v.height, v.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![(3, "checkpoint"), (0, "applied"), (0, "applied"), (0, "genesis")]
        );
        assert_eq!(blocks[0].payload, json!(2), "the checkpoint view shows the upto");
        assert_eq!(blocks[0].signers.len(), 2, "the anchor block keeps its m signers");
        assert_eq!(blocks[1].proposal_id, 2, "pre-cut entries stay listed, newest first");
        assert_eq!(blocks[2].proposal_id, 1);
        assert_eq!(blocks[1].surface, "memory");
        assert!(
            blocks[1].signers.is_empty() && blocks[2].signers.is_empty(),
            "the pre-cut block signatures are gone with the history"
        );
        assert_eq!(blocks[3].payload, json!("Chess Club"));
        assert_eq!(
            blocks[3].signers,
            vec!["petra".to_string(), "walter".to_string()],
            "the genesis view rebuilds from the blob's founding table"
        );
    }

    // ---- the wiki export bundle verifier (wiki_export_plan.md) ------------
    //
    // The bundle is a SUBSET of the chain (genesis + every Membership block +
    // every applied wiki patch), so `prev` links and contiguous heights are
    // gone by construction. What survives is what each block's own m
    // signatures cover: `republic_id ‖ height ‖ change` against the roster
    // valid at that height. These pin exactly that, and the fold equality.

    /// One `wiki_patch` payload in the shape a Memory proposal carries.
    fn wiki_payload(patch: &str) -> serde_json::Value {
        json!({ "op": "wiki_patch", "value": patch })
    }

    const WIKI_ADD_A: &str = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
    const WIKI_ADD_B: &str = "diff --git a/notes/b.md b/notes/b.md\nnew file mode 100644\n--- /dev/null\n+++ b/notes/b.md\n@@ -0,0 +1,1 @@\n+second\n";

    /// Commit an applied `wiki_patch` block at the next height.
    fn commit_wiki(b: &mut Builder, proposal_id: u64, patch: &str, signers: &[&str]) {
        let height = u64::try_from(b.blocks.len()).expect("small chain");
        let change = ChainChange::Applied {
            proposal_id,
            surface: Surface::Memory,
            payload: wiki_payload(patch),
        };
        let block = b.seal(height, change, signers);
        b.push(block);
    }

    /// The fixture every bundle test shares: a real 2-of-2 chain that
    /// carries both things the bundle must survive — a non-wiki block in
    /// the middle (dropped from the bundle, so heights have gaps) and a
    /// roster that MOVES (a recovery with consent, then a joined seat whose
    /// key signs the second patch).
    ///
    /// h0 genesis · h1 wiki patch · h2 org edit · h3 restored (consent) ·
    /// h4 joined dora · h5 wiki patch signed by dora.
    fn wiki_fixture() -> Builder {
        let mut b = Builder::new(&["petra", "walter"], 2);
        commit_wiki(&mut b, 1, WIKI_ADD_A, &["petra", "walter"]);
        b.commit_org(2, "set_name", "Chess Club 2", &["petra", "walter"]);
        // walter recovers: petra signs, walter's own consent is the second
        // voice (the m = n recovery path)
        let consent = identity_sign(
            b.key("walter"),
            &molt_core::chain::restore_consent_bytes(
                &b.republic_id,
                "walter",
                &b.pk("walter"),
                "dd".repeat(32).as_str(),
            ),
        );
        let height = u64::try_from(b.blocks.len()).expect("small chain");
        let restored = b.seal(
            height,
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member: "walter".to_string(),
                identity_pk: b.pk("walter"),
                nostr_pk: Some("dd".repeat(32)),
                relays: Vec::new(),
                consent: Some(consent),
            },
            &["petra"],
        );
        b.push(restored);
        // dora joins with her own key and co-signs the second patch
        let (dora_sk, dora_pk) = derive_identity_key(&[9u8; 32], "dora");
        let height = u64::try_from(b.blocks.len()).expect("small chain");
        let joined = b.seal(
            height,
            ChainChange::Membership {
                op: MembershipOp::Joined,
                member: "dora".to_string(),
                identity_pk: dora_pk,
                nostr_pk: None,
                relays: Vec::new(),
                consent: None,
            },
            &["petra", "walter"],
        );
        b.push(joined);
        b.keys.push(("dora".to_string(), dora_sk));
        commit_wiki(&mut b, 3, WIKI_ADD_B, &["walter", "dora"]);
        b
    }

    /// The tree the fixture's two patches fold to.
    fn wiki_fixture_tree() -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([
            ("a.md".to_string(), "hello\nworld\n".to_string()),
            ("notes/b.md".to_string(), "second\n".to_string()),
        ])
    }

    /// Serialize the bundle the writer would ship for `blocks`.
    fn bundle_json(blocks: &[ChainBlock]) -> String {
        let bundle = crate::wiki_export::bundle_from_chain(blocks).expect("the chain has a genesis");
        serde_json::to_string(&bundle).expect("bundle serializes")
    }

    #[test]
    fn a_wiki_export_bundle_verifies_against_its_tree() {
        let b = wiki_fixture();
        verify_chain(&b.blocks).expect("the fixture is a real chain");
        let json = bundle_json(&b.blocks);
        let report = verify_wiki_export(&json, &wiki_fixture_tree()).expect("the bundle verifies");
        assert_eq!(report.republic_id, b.republic_id);
        assert_eq!(report.name, "Chess Club");
        assert_eq!((report.rule_m, report.rule_n), (2, 2));
        assert_eq!(report.patches, 2, "both wiki patches ride along");
        assert_eq!(report.membership_blocks, 2, "restored + joined ride along");
        assert_eq!(report.files, 2);
        assert_eq!(
            report.members,
            vec!["petra".to_string(), "walter".to_string(), "dora".to_string()],
            "the roster walk ends at the post-join roster"
        );
        // the org edit is NOT in the bundle: its content never leaves
        assert!(
            !json.contains("set_name"),
            "only wiki patches and membership blocks are exported"
        );
    }

    #[test]
    fn a_tampered_file_in_the_tree_fails_verification() {
        let b = wiki_fixture();
        let json = bundle_json(&b.blocks);
        let mut tree = wiki_fixture_tree();
        tree.insert("a.md".to_string(), "hello\nWORLD\n".to_string());
        let err = verify_wiki_export(&json, &tree).expect_err("a flipped byte must fail");
        assert!(err.contains("a.md"), "the fault names the file: {err}");
        // an EXTRA file the fold never produced is caught too
        let mut tree = wiki_fixture_tree();
        tree.insert("stray.md".to_string(), "smuggled".to_string());
        assert!(verify_wiki_export(&json, &tree).is_err(), "a stray file must fail");
    }

    #[test]
    fn a_tampered_patch_payload_fails_verification() {
        let mut b = wiki_fixture();
        // rewrite the first patch's content without re-signing
        if let ChainChange::Applied { payload, .. } = &mut b.blocks[1].change {
            *payload = wiki_payload(WIKI_ADD_A.replace("world", "welt").as_str());
        }
        let json = bundle_json(&b.blocks);
        assert!(
            verify_wiki_export(&json, &wiki_fixture_tree()).is_err(),
            "the m signatures cover the patch bytes"
        );
    }

    #[test]
    fn a_forged_or_removed_signature_fails_verification() {
        // removed: the patch drops below the threshold
        let mut b = wiki_fixture();
        b.blocks[5].sigs.truncate(1);
        assert!(
            verify_wiki_export(&bundle_json(&b.blocks), &wiki_fixture_tree()).is_err(),
            "one signature is below m = 2"
        );
        // forged: a signature that does not verify counts for nobody
        let mut b = wiki_fixture();
        b.blocks[5].sigs[0].sig = "00".repeat(64);
        assert!(
            verify_wiki_export(&bundle_json(&b.blocks), &wiki_fixture_tree()).is_err(),
            "a forged signature must not count"
        );
        // a signer outside the roster cannot lift a block to threshold
        let mut b = wiki_fixture();
        let (mallory_sk, _) = derive_identity_key(&[42u8; 32], "mallory");
        let bytes = approval_bytes(&b.republic_id, 5, &b.blocks[5].change);
        b.blocks[5].sigs[0] = RosterAttestation {
            member: "mallory".to_string(),
            sig: identity_sign(&mallory_sk, &bytes),
        };
        assert!(
            verify_wiki_export(&bundle_json(&b.blocks), &wiki_fixture_tree()).is_err(),
            "a stranger's signature is not a roster approval"
        );
    }

    #[test]
    fn a_forged_recovery_consent_fails_verification() {
        let mut b = wiki_fixture();
        if let ChainChange::Membership { consent, .. } = &mut b.blocks[3].change {
            *consent = Some("11".repeat(64));
        }
        assert!(
            verify_wiki_export(&bundle_json(&b.blocks), &wiki_fixture_tree()).is_err(),
            "a consent that does not verify is not the second voice"
        );
    }

    #[test]
    fn an_omitted_membership_block_fails_the_later_patch() {
        let b = wiki_fixture();
        let mut bundle =
            crate::wiki_export::bundle_from_chain(&b.blocks).expect("the chain has a genesis");
        // drop the Joined block — dora's signature on the last patch then
        // belongs to nobody in the roster
        bundle.blocks.retain(|block| {
            !matches!(
                &block.change,
                ChainChange::Membership { op: MembershipOp::Joined, .. }
            )
        });
        let json = serde_json::to_string(&bundle).expect("bundle serializes");
        assert!(
            verify_wiki_export(&json, &wiki_fixture_tree()).is_err(),
            "without the identity history the later patch cannot verify"
        );
    }

    #[test]
    fn reordered_or_duplicate_heights_fail_verification() {
        let b = wiki_fixture();
        let base =
            crate::wiki_export::bundle_from_chain(&b.blocks).expect("the chain has a genesis");
        // reordered
        let mut bundle = base.clone();
        bundle.blocks.reverse();
        assert!(
            verify_wiki_export(
                &serde_json::to_string(&bundle).expect("serialize"),
                &wiki_fixture_tree()
            )
            .is_err(),
            "blocks must arrive in ascending height order"
        );
        // duplicated
        let mut bundle = base.clone();
        let dup = bundle.blocks[0].clone();
        bundle.blocks.insert(1, dup);
        assert!(
            verify_wiki_export(
                &serde_json::to_string(&bundle).expect("serialize"),
                &wiki_fixture_tree()
            )
            .is_err(),
            "a repeated block must not fold twice"
        );
    }

    #[test]
    fn a_non_wiki_block_in_the_bundle_is_refused() {
        let b = wiki_fixture();
        let mut bundle =
            crate::wiki_export::bundle_from_chain(&b.blocks).expect("the chain has a genesis");
        bundle.blocks.push(b.blocks[2].clone()); // the org edit
        assert!(
            verify_wiki_export(
                &serde_json::to_string(&bundle).expect("serialize"),
                &wiki_fixture_tree()
            )
            .is_err(),
            "the bundle carries wiki patches and membership blocks, nothing else"
        );
    }

    #[test]
    fn a_forged_genesis_id_fails_the_bundle() {
        let mut b = wiki_fixture();
        if let ChainChange::Genesis { republic_id, .. } = &mut b.blocks[0].change {
            *republic_id = "deadbeef".to_string();
        }
        assert!(
            verify_wiki_export(&bundle_json(&b.blocks), &wiki_fixture_tree()).is_err(),
            "the genesis id must re-derive from the roster content"
        );
    }

    #[test]
    fn a_foreign_bundle_format_is_refused() {
        let b = wiki_fixture();
        let json = bundle_json(&b.blocks).replace("molt-wiki-export-v1", "molt-wiki-export-v9");
        assert!(
            verify_wiki_export(&json, &wiki_fixture_tree()).is_err(),
            "an unknown format tag is not verified on hope"
        );
    }
}
