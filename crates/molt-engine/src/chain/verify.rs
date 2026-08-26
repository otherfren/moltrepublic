// SPDX-License-Identifier: GPL-3.0-or-later

//! **Verification of the persistent commit-block chain** (see
//! [`molt_core::chain`]) — the pure half of [`crate::chain`]: no `State`, no
//! I/O. This is the security layer that makes a handed-over chain
//! *self-authenticating*: a member — or a rejoiner who fetched the chain
//! from an untrusted peer — checks it here with no live mesh and no trust in
//! the deliverer.
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
//!
//! Also here: the pure projections a verified chain yields without a holder
//! ([`working_anchors`], [`declared_relays`], [`effective_relays_of_served`],
//! [`chain_block_view`]) and the detached wiki-export verifier.

use super::*;

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
pub(super) fn chain_block_view(block: &ChainBlock) -> molt_core::ChainBlockView {
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
            // HARD-REJECTED (review C7): seats are fixed at founding (product
            // decision 2026-07-11) and a joined-later seat is not in the
            // founding table, so the first checkpoint after such a block
            // stranded every pruned holder (`walk_suffix_chain` requires
            // every roster entry there). The variant stays reserved; a
            // chain carrying it is refused whole, like any unknown change.
            let _ = (identities, member, identity_pk);
            return Err("membership op `joined` is not supported: seats are fixed at founding".to_string());
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
pub(super) fn block_signers(
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
                "block {} counts {member} twice - consent plus a roster signature",
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
    pub(super) seen: BTreeSet<u64>,
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
    pub(super) fn step(&mut self, block: &ChainBlock) -> Result<(), String> {
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
pub(super) fn fold_state(
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
