// SPDX-License-Identifier: GPL-3.0-or-later

//! The republic's **persistent-change chain** — the one converged,
//! self-authenticating record of everything that mutates shared republic
//! state. It is a *single-branch* sequence of threshold-signed commit blocks
//! ("git patches"): block 0 is the founding constitution, and every later
//! block is one gated change that reached its m-of-n approval, appended as the
//! next step at the head.
//!
//! What is **not** here is as load-bearing as what is: chat and the
//! propose/approve *deliberation* are **ephemeral** — they never become blocks
//! (transport concept: chat is flüchtig). Only the committed change, with its
//! member signatures bundled, joins the chain. A member who deliberately
//! persists content into the brain does so through a *gated* change, so that
//! promotion is itself a block.
//!
//! This module holds only the **types and their canonical bytes** — the exact
//! same split the founding roster already follows: [`roster_canonical_bytes`]
//! lives here, its Ed25519 verification lives in `molt-engine`. So the chain's
//! hashing (`molt-storage`) and signature checking (`molt-engine::chain`) sit a
//! layer up; this file has no crypto and no I/O.
//!
//! ## Load-bearing invariants
//!
//! * **Position-bound signatures.** Each of the m signatures on a block is over
//!   `republic_id ‖ height ‖ change` — it authenticates the block's *exact*
//!   sequence number. A block cannot be moved, reordered or spliced onto a
//!   different history without the members re-signing (which is exactly what a
//!   "re-base" onto a new slot means). Reorder/splice is therefore dead: the
//!   height is inside the signed bytes and the genesis is content-fixed.
//! * **Genesis reuses the roster bytes.** Block 0's signatures *are* the
//!   founding attestations over [`roster_canonical_bytes`], so founding needs no
//!   new signing path and a rejoiner verifies the constitution the same way
//!   every member ratified it.
//! * **`prev` is a structural link, not signed.** Each block records the hash of
//!   the previous block's [`block_link_bytes`]; the chain verifier checks the
//!   links form an unbroken line from the genesis. Swapping a block's signature
//!   set breaks the link (the sigs are folded into the hashed bytes).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{roster_canonical_bytes, MemberId, MemberIdentity, RosterAttestation, Surface};

/// The `prev` of the genesis block: 32 zero bytes as lowercase hex. A chain
/// roots here and nowhere else.
pub const GENESIS_PREV: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// The kind of membership transition a [`ChainChange::Membership`] block
/// enacts. Additive-only, like [`crate::WorkspaceEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipOp {
    /// A seat filled via invite: the member and its anchored identity join the
    /// roster.
    Joined,
    /// A member re-keyed after recovery: the seat keeps its handle but adopts a
    /// freshly derived identity key (the old device is gone).
    Restored,
}

/// The persistent change one chain block commits. Additive-only: a new kind of
/// gated mutation appends a variant; an older reader that meets an unknown
/// variant must refuse to extend the chain (applying a partial history would
/// fork shared state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChainChange {
    /// Block 0 only: the sealed founding constitution. Its block signatures are
    /// the roster attestations, and its canonical bytes are exactly
    /// [`roster_canonical_bytes`] — so the founding *is* the first commit block.
    Genesis {
        /// The republic's display name (folded into `republic_id`).
        name: String,
        /// The neutral, content-derived republic id (the roster salt).
        republic_id: String,
        /// Approval threshold (m) for every later block.
        rule_m: u8,
        /// Founding member count (n) — the genesis is unanimous (n-of-n).
        rule_n: u8,
        /// name → identity key, founder first, then invite order.
        identities: Vec<MemberIdentity>,
        /// The ratified free-text charter.
        agenda: String,
    },
    /// A gated surface transition that reached threshold and is applied.
    Applied {
        /// The proposal this commits — unique across the chain (no double-apply).
        proposal_id: u64,
        /// The gated target surface.
        surface: Surface,
        /// The surface-specific transition payload.
        payload: Value,
    },
    /// A membership change (invite add or recovery re-key).
    Membership {
        /// Add a fresh seat, or re-key an existing one.
        op: MembershipOp,
        /// The affected member handle.
        member: MemberId,
        /// The member's anchored Ed25519 identity key, lowercase hex.
        identity_pk: String,
    },
    /// WP4b: a threshold-signed compaction cut. m-of-n members signed the
    /// SHA-256 of the deterministically serialized republic state after
    /// block `upto` ([`checkpoint_canonical_bytes`]) — confirming the
    /// CORRECTNESS of the compaction, sign-what-you-see (every signer
    /// recomputes the bytes from its own chain before signing). Once
    /// committed, blocks `<= upto` may be dropped locally; a newcomer
    /// bootstraps from checkpoint + suffix (`docs/chain/log_compaction.md`
    /// Teil B).
    Checkpoint {
        /// The last folded-in block: the checkpoint attests the state
        /// AFTER applying block `upto`.
        upto: u64,
        /// SHA-256 (lowercase hex) over the canonical state bytes.
        state_hash: String,
    },
}

/// One committed block in the persistent chain: a threshold-signed change plus
/// its position (`height`) and a structural link to its predecessor (`prev`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainBlock {
    /// The block's sequence number: 0 = genesis, strictly monotonic, no gaps.
    pub height: u64,
    /// Hash (lowercase hex of SHA-256 over [`block_link_bytes`]) of the previous
    /// block; [`GENESIS_PREV`] for block 0.
    pub prev: String,
    /// The persistent change this block enacts.
    pub change: ChainChange,
    /// The member signatures over [`approval_bytes`]: n-of-n at the genesis
    /// (the founding attestations), m-of-n for every later block.
    pub sigs: Vec<RosterAttestation>,
}

/// Append a `u32`-length-prefixed byte string (same framing as
/// [`roster_canonical_bytes`], so the layouts stay siblings).
fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&u32::try_from(b.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(b);
}

/// **What the m members sign.** Position-bound: the block's `height` is folded
/// in, so a signature authenticates the change *at that exact sequence number*.
///
/// The genesis is special — its bytes are exactly [`roster_canonical_bytes`],
/// so the founding attestations validate a genesis block with no new signing
/// path. `height`/`republic_id` arguments are ignored for the genesis (it is
/// always height 0 and carries its own id).
pub fn approval_bytes(republic_id: &str, height: u64, change: &ChainChange) -> Vec<u8> {
    match change {
        ChainChange::Genesis {
            name: _,
            republic_id: rid,
            rule_m,
            rule_n,
            identities,
            agenda,
        } => roster_canonical_bytes(rid, *rule_m, *rule_n, identities, agenda),
        ChainChange::Applied {
            proposal_id,
            surface,
            payload,
        } => {
            let mut out = Vec::new();
            out.extend_from_slice(b"molt-chain-change-v1\0");
            put_bytes(&mut out, republic_id.as_bytes());
            out.extend_from_slice(&height.to_le_bytes());
            out.push(1); // variant tag: applied
            out.extend_from_slice(&proposal_id.to_le_bytes());
            put_bytes(&mut out, surface.as_str().as_bytes());
            // serde_json has no `preserve_order` here, so `Value::Object` is a
            // BTreeMap and this serialization is canonical across members.
            put_bytes(&mut out, &serde_json::to_vec(payload).unwrap_or_default());
            out
        }
        ChainChange::Membership {
            op,
            member,
            identity_pk,
        } => {
            let mut out = Vec::new();
            out.extend_from_slice(b"molt-chain-change-v1\0");
            put_bytes(&mut out, republic_id.as_bytes());
            out.extend_from_slice(&height.to_le_bytes());
            out.push(2); // variant tag: membership
            out.push(match op {
                MembershipOp::Joined => 0,
                MembershipOp::Restored => 1,
            });
            put_bytes(&mut out, member.as_bytes());
            put_bytes(&mut out, identity_pk.as_bytes());
            out
        }
        ChainChange::Checkpoint { upto, state_hash } => {
            let mut out = Vec::new();
            out.extend_from_slice(b"molt-chain-change-v1\0");
            put_bytes(&mut out, republic_id.as_bytes());
            out.extend_from_slice(&height.to_le_bytes());
            out.push(3); // variant tag: checkpoint
            out.extend_from_slice(&upto.to_le_bytes());
            put_bytes(&mut out, state_hash.as_bytes());
            out
        }
    }
}

/// The projected republic state a checkpoint attests — everything a
/// suffix-only bootstrapper needs, and nothing a node could disagree on:
/// every field is derived from the totally ordered chain, so equal chains
/// yield equal structs yield equal [`checkpoint_canonical_bytes`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointState {
    /// The founding display name (genesis).
    pub founding_name: String,
    /// Approval threshold m (genesis; immutable for the republic's life).
    pub rule_m: u8,
    /// Founding member count n (genesis).
    pub rule_n: u8,
    /// The GENESIS identity table, founding order — lets every verifier
    /// RECOMPUTE `republic_id` from content (the genesis forgery check
    /// survives the genesis block being dropped).
    pub founding_identities: Vec<MemberIdentity>,
    /// The ratified founding charter (genesis).
    pub agenda: String,
    /// The content-derived republic id (must equal the recomputation).
    pub republic_id: String,
    /// The CURRENT roster after every membership block `<= upto`, in
    /// chain order (deterministic — the block sequence is totally ordered).
    pub roster: Vec<MemberIdentity>,
    /// The applied projection: per surface (in [`Surface::ALL`] order,
    /// surfaces without entries included empty), the `(proposal id,
    /// payload)` list in block order.
    pub applied: Vec<(Surface, Vec<(u64, Value)>)>,
    /// Every proposal id consumed by an `Applied` block `<= upto`, sorted —
    /// seeds the double-apply guard of a suffix verifier.
    pub consumed_ids: Vec<u64>,
    /// The last folded-in block height.
    pub upto: u64,
}

/// **What checkpoint signers hash.** The canonical, versioned
/// serialization of [`CheckpointState`] (`molt-chain-checkpoint-v2` — v2
/// covers each member's `nostr_pk` third anchor in BOTH tables; under v1 a
/// served checkpoint's roster anchor could be swapped without changing the
/// state hash, so the tamper-evidence roster-v3 gives the genesis vanished
/// the moment a republic pruned) —
/// same length-prefixed framing as [`roster_canonical_bytes`], so the
/// layouts stay siblings. JSON payloads serialize canonically because
/// `serde_json::Map` is a BTreeMap here (no `preserve_order` feature —
/// pinned by `serde_json_object_serializes_with_sorted_keys`).
pub fn checkpoint_canonical_bytes(s: &CheckpointState) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"molt-chain-checkpoint-v2\0");
    put_bytes(&mut out, s.republic_id.as_bytes());
    put_bytes(&mut out, s.founding_name.as_bytes());
    out.push(s.rule_m);
    out.push(s.rule_n);
    out.extend_from_slice(&u64::try_from(s.founding_identities.len()).unwrap_or(0).to_le_bytes());
    for i in &s.founding_identities {
        put_bytes(&mut out, i.member.as_bytes());
        put_bytes(&mut out, i.identity_pk.as_bytes());
        put_bytes(&mut out, i.nostr_pk.as_bytes());
    }
    put_bytes(&mut out, s.agenda.as_bytes());
    out.extend_from_slice(&u64::try_from(s.roster.len()).unwrap_or(0).to_le_bytes());
    for i in &s.roster {
        put_bytes(&mut out, i.member.as_bytes());
        put_bytes(&mut out, i.identity_pk.as_bytes());
        put_bytes(&mut out, i.nostr_pk.as_bytes());
    }
    for (surface, entries) in &s.applied {
        put_bytes(&mut out, surface.as_str().as_bytes());
        out.extend_from_slice(&u64::try_from(entries.len()).unwrap_or(0).to_le_bytes());
        for (id, payload) in entries {
            out.extend_from_slice(&id.to_le_bytes());
            put_bytes(&mut out, &serde_json::to_vec(payload).unwrap_or_default());
        }
    }
    out.extend_from_slice(&u64::try_from(s.consumed_ids.len()).unwrap_or(0).to_le_bytes());
    for id in &s.consumed_ids {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out.extend_from_slice(&s.upto.to_le_bytes());
    out
}

/// **What a block's content hash is taken over** — the bytes whose SHA-256
/// becomes the *next* block's `prev`. It commits to the height, the `prev`
/// link, the [`approval_bytes`] and the exact signature set (sorted by member),
/// so neither the change nor its signatures can be altered without breaking the
/// chain's `prev` links downstream.
pub fn block_link_bytes(republic_id: &str, block: &ChainBlock) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"molt-chain-block-v1\0");
    out.extend_from_slice(&block.height.to_le_bytes());
    put_bytes(&mut out, block.prev.as_bytes());
    put_bytes(&mut out, &approval_bytes(republic_id, block.height, &block.change));
    let mut sigs: Vec<(&str, &str)> = block
        .sigs
        .iter()
        .map(|a| (a.member.as_str(), a.sig.as_str()))
        .collect();
    sigs.sort_unstable();
    for (member, sig) in sigs {
        put_bytes(&mut out, member.as_bytes());
        put_bytes(&mut out, sig.as_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ident(name: &str, pk: &str) -> MemberIdentity {
        MemberIdentity {
            member: name.to_string(),
            identity_pk: pk.to_string(),
            nostr_pk: "cc".repeat(32),
        }
    }

    /// The determinism keystone the canonical-bytes comments PROMISE (and a
    /// total-review finding flagged missing): `serde_json::Value::Object`
    /// must serialize with sorted keys, i.e. the `preserve_order` feature
    /// must NOT be active anywhere in the build graph. If a dependency ever
    /// enables it, `Value::Object` becomes insertion-ordered and two nodes
    /// building the same logical payload differently would produce diverging
    /// `approval_bytes`/`checkpoint_canonical_bytes` — silent signature and
    /// convergence failure. This pins it at compile+test time.
    #[test]
    fn serde_json_object_serializes_with_sorted_keys() {
        // insertion order z, a, m — canonical output must reorder to a, m, z
        let mut v = serde_json::Value::Object(serde_json::Map::new());
        if let serde_json::Value::Object(m) = &mut v {
            m.insert("z".to_string(), json!(1));
            m.insert("a".to_string(), json!(2));
            m.insert("m".to_string(), json!(3));
        }
        assert_eq!(
            serde_json::to_string(&v).expect("serialize"),
            r#"{"a":2,"m":3,"z":1}"#,
            "serde_json preserve_order is ON — canonical signed bytes are no longer deterministic"
        );
        // and a byte-for-byte re-parse → re-serialize is stable
        let reparsed: serde_json::Value =
            serde_json::from_str(r#"{"m":3,"z":1,"a":2}"#).expect("parse");
        assert_eq!(
            serde_json::to_vec(&reparsed).expect("serialize"),
            r#"{"a":2,"m":3,"z":1}"#.as_bytes()
        );
    }

    /// The genesis approval bytes must equal the roster bytes every member
    /// already signed at founding — otherwise the attestations would not
    /// validate a genesis block.
    #[test]
    fn genesis_approval_bytes_are_the_roster_bytes() {
        let identities = vec![ident("petra", "aa"), ident("walter", "bb")];
        let change = ChainChange::Genesis {
            name: "Chess Club".to_string(),
            republic_id: "f00".to_string(),
            rule_m: 2,
            rule_n: 2,
            identities: identities.clone(),
            agenda: "play".to_string(),
        };
        assert_eq!(
            approval_bytes("f00", 0, &change),
            roster_canonical_bytes("f00", 2, 2, &identities, "play"),
        );
    }

    /// Height is inside the signed bytes: the *same* change at a different
    /// sequence number signs different bytes (so it cannot be re-slotted
    /// without re-signing).
    #[test]
    fn approval_bytes_are_height_bound() {
        let change = ChainChange::Applied {
            proposal_id: 7,
            surface: Surface::Memory,
            payload: json!({"op": "add_note", "title": "t"}),
        };
        assert_ne!(
            approval_bytes("f00", 3, &change),
            approval_bytes("f00", 4, &change),
        );
    }

    /// The block link commits to the signatures: swapping a sig changes the
    /// hashed bytes (so a downstream `prev` would no longer match).
    #[test]
    fn block_link_bytes_bind_the_signature_set() {
        let block = ChainBlock {
            height: 2,
            prev: GENESIS_PREV.to_string(),
            change: ChainChange::Applied {
                proposal_id: 1,
                surface: Surface::Quests,
                payload: json!({"op": "add_quest"}),
            },
            sigs: vec![RosterAttestation {
                member: "petra".to_string(),
                sig: "1111".to_string(),
            }],
        };
        let mut tampered = block.clone();
        tampered.sigs[0].sig = "2222".to_string();
        assert_ne!(
            block_link_bytes("f00", &block),
            block_link_bytes("f00", &tampered),
        );
    }

    /// Sorting the sig set makes the link bytes independent of collection order.
    #[test]
    fn block_link_bytes_ignore_signature_order() {
        let mk = |sigs: Vec<RosterAttestation>| ChainBlock {
            height: 5,
            prev: "ab".to_string(),
            change: ChainChange::Membership {
                op: MembershipOp::Joined,
                member: "dora".to_string(),
                identity_pk: "cc".to_string(),
            },
            sigs,
        };
        let a = mk(vec![
            RosterAttestation { member: "petra".to_string(), sig: "11".to_string() },
            RosterAttestation { member: "walter".to_string(), sig: "22".to_string() },
        ]);
        let b = mk(vec![
            RosterAttestation { member: "walter".to_string(), sig: "22".to_string() },
            RosterAttestation { member: "petra".to_string(), sig: "11".to_string() },
        ]);
        assert_eq!(block_link_bytes("f00", &a), block_link_bytes("f00", &b));
    }
}
