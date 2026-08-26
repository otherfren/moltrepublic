// SPDX-License-Identifier: GPL-3.0-or-later

//! MLS group encryption (transport concept §1, §3.3, §6) — the confidentiality
//! layer whose ciphertext *is* the SMP payload.
//!
//! This module wraps OpenMLS (RFC 9420) behind a small, molt-shaped API used
//! in two places:
//!
//! * the **founding ritual** ([`crate::invite`], driven by the engine): each
//!   member advertises a [`MlsMember::key_package`] in its `JoinRequest`; the
//!   founder [`MlsMember::create_group`]s and [`MlsMember::add_members`] to
//!   produce one Welcome; every member [`MlsMember::join_from_welcome`]s and
//!   finishes the ritual already inside the group;
//! * the **runtime transport** (the supervisor): [`MlsMember::encrypt`] turns a
//!   serialized workspace event into an MLS application message, and
//!   [`MlsMember::decrypt`] reverses it, authenticating the sender.
//!
//! Ciphersuite is fixed to `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519` —
//! the RFC's mandatory-to-implement suite, matching our Ed25519 identity keys
//! and X25519 exactly. The signer is built **from the member's derived
//! identity key** (not MLS-generated), so the KeyPackage credential and the
//! genesis identity table anchor the *same* key (concept §3.3, "one identity,
//! two anchors").
//!
//! **Provider & persistence.** OpenMLS keeps all secret state (ratchets, the
//! secret tree, key packages, the signer) inside its `StorageProvider`. We use
//! the pure-Rust [`OpenMlsRustCrypto`] provider and snapshot its byte-keyed map
//! into an opaque blob ([`MlsMember::snapshot`]) that the caller seals into
//! `transport.state`. This file must never accrete history: MLS deletes key
//! material on purpose (that deletion *is* forward secrecy), so the snapshot is
//! always the *current* state, atomically overwritten — never appended (concept
//! §6). Founding-time state is sealed durably and synchronously at genesis (the
//! engine's `materialize_workspace`). The **write-ahead** ordering for the
//! *running* traffic — persist the advanced ratchet before a ciphertext leaves /
//! before an inbound plaintext reaches the engine — is the contract the future
//! supervisor integration must honour (`encrypt`/`decrypt` mutate, then the
//! caller persists `snapshot()` before releasing the result); it is **not yet
//! wired**, because nothing encrypts the running traffic with the group yet
//! (the last open T2 piece).

use ed25519_dalek::SigningKey;
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use openmls_traits::types::SignatureScheme;
use openmls_traits::OpenMlsProvider;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::HashMap;
use tls_codec::{Deserialize as _, Serialize as _};

/// The one ciphersuite MoltRepublic speaks (RFC 9420 mandatory-to-implement).
const SUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// The sender-ratchet window, set EXPLICITLY on both the create and join
/// configs (delivery guarantee §4.6). BOTH directions are widened from the
/// openmls defaults (5 / 1000):
///
/// * forward 100_000 — every send advances the sender ratchet whether it is
///   delivered or not, so a deaf leg can swallow thousands of generations;
///   the first frame after the heal arrives with the whole gap at once, and
///   a receiver refusing the jump kills the leg forever (V5). A forward
///   skip costs only chain derivations, no stored keys.
/// * backward (out-of-order tolerance) 5_000 — the deaf window's ORIGINAL
///   frames sit in the server queue and arrive after the resubscribe/rotate
///   heal, by which time the receiver has usually consumed the fresh
///   RESENDS (higher generations). With the default 5, every stored
///   original was `TooDistantInThePast`-discarded (the 2026-07-28 live
///   validation) and content had to wait for the next resend backoff; with
///   the window it decrypts the moment the leg heals. Cost: up to 5k
///   retained unused ratchet keys per sender (~hundreds of KB, bounded);
///   used keys are still deleted on first use, so replay stays rejected.
fn ratchet_window() -> SenderRatchetConfiguration {
    SenderRatchetConfiguration::new(5_000, 100_000)
}

/// The snapshot schema version (bumped on any incompatible blob layout).
const SNAPSHOT_VERSION: u8 = 4;

/// Everything that can go wrong inside the MLS layer. All variants are local
/// bugs or corrupt/hostile wire input — never a transient network condition.
#[derive(Debug, thiserror::Error)]
pub enum MlsError {
    /// An OpenMLS operation failed (create/add/join/encrypt/decrypt).
    #[error("mls: {0}")]
    Mls(String),
    /// A wire message (KeyPackage, Welcome, application message) did not parse
    /// or did not authenticate — untrusted input, dropped, never a panic.
    #[error("mls wire: {0}")]
    Wire(String),
    /// A group operation was attempted before the group exists (create/join).
    #[error("mls: no group yet")]
    NoGroup,
    /// The persisted snapshot could not be encoded/decoded.
    #[error("mls snapshot: {0}")]
    Snapshot(String),
}

/// What a decrypted inbound MLS message turned out to be.
#[derive(Debug)]
pub enum MlsIncoming {
    /// An application message: the authenticated sender's identity (the member
    /// handle, from the leaf credential) and the plaintext payload.
    Application {
        /// The sending member's handle (credential identity bytes → UTF-8).
        from: String,
        /// The decrypted application payload (a serialized workspace event).
        plaintext: Vec<u8>,
    },
    /// A membership change (Add/Remove/Update) was merged — the epoch advanced.
    /// `readmitted` carries the identities the commit ADDED (a recovery
    /// re-key re-adds the recovered seat): the caller must forget those
    /// members' accept windows BEFORE it processes anything of the new
    /// epoch, and this merge is the one point ordered before every frame
    /// the new incarnation can produce (its frames cannot decrypt earlier —
    /// live incident 2026-08-09 §2, field rerun 2026-08-17).
    Commit {
        /// Member handles the merged commit added to the group.
        readmitted: Vec<String>,
    },
    /// We had merged our OWN commit, a concurrent one won the tiebreak, and
    /// we rewound onto it. **The work our commit carried is gone** — a
    /// recovery re-key in particular, whose Welcome the rejoiner may already
    /// hold for a branch nobody is on. The caller must re-issue it against
    /// the new epoch; treating this like an ordinary merge strands the
    /// member it was for (review finding 2026-07-31). `readmitted` reports
    /// the WINNING commit's adds, exactly like [`MlsIncoming::Commit`].
    CommitRewound {
        /// Member handles the winning commit added to the group.
        readmitted: Vec<String>,
    },
    /// A same-epoch commit that LOST the deterministic tiebreak
    /// ([`CommitKey`]): our own commit stands, this one is superseded. The
    /// sender will rewind to ours; its proposals are re-decided at the new
    /// epoch by the chain layer, never replayed.
    CommitSuperseded,
    /// A bare proposal was stored, awaiting its commit.
    Proposal,
    /// The message claims an epoch **ahead** of this group's — its re-key
    /// commit is still in flight. The message was NOT consumed: buffer it and
    /// feed the SAME bytes back through [`MlsMember::decrypt`] once a commit
    /// merges (the transport's cross-epoch retry). NB the epoch header is
    /// unauthenticated pre-decryption — a forgery can claim a future epoch, so
    /// the buffer must be bounded and shed-tolerant.
    FutureEpoch,
}

/// The engine's answer to "may this group-data change be applied?" —
/// defined here (molt-net), implemented by molt-engine, handed into the
/// runtime, exactly like [`crate::EngineSink`]. It exists because the chain
/// lives in engine state and the layering forbids molt-net reading it
/// (`nostr_n05_engine_inventory.md` §5).
///
/// **Drop BEFORE merge.** A commit that changes group data is authorized by
/// a threshold-decided chain block, and that authorization is checked
/// before `merge_staged_commit` — never merge-then-reject. Merging first
/// would advance the epoch on a change the republic never decided, and
/// every node that refused it would then be on a different epoch: the
/// permanent split concept §5 warns about.
///
/// TEST-ONLY until N6 wires it (`nostr_n4_plan.md`): no implementor and no
/// caller exist outside the keystone test, so the seam is `cfg(test)` rather
/// than a public API that promises a gate nothing enforces yet (review M12).
#[cfg(test)]
pub trait ChainOracle: Send + Sync + 'static {
    /// Does a threshold-decided chain block with this hash authorize a
    /// group-data change? Pure over the applied chain.
    fn authorizes(&self, block_hash: &str) -> bool;
    /// The current verified head `(height, hash)` — for staleness checks
    /// and the AAD binding.
    fn head(&self) -> Option<(u64, String)>;
}

/// How many past epochs' exporter secrets stay available for the OUTER
/// envelope layer (§10.4, K = 3). Epochs change only on membership and
/// recovery, so three covers the recent ones; beyond the ring an event is
/// epoch-opaque and must be reported loudly (G4), never silently skipped.
/// The ring holds OUTER secrets only — the inner MLS layer keeps
/// `max_past_epochs = 0`, so an evicted leaf's old-epoch message stays
/// rejected (the asymmetry, concept §6).
// 8 (was 3, 2026-08-24): the ring is the HEALING WINDOW — a laggard is
// reachable by an epoch-correct commit resend only while the commit's
// exporter is still in every sender's ring (`detached_reattach.md` §7).
// Cost: 32 bytes per entry and a bounded forward-secrecy trade on the
// OUTER metadata layer only (the inner MLS ratchet is untouched).
pub const EXPORTER_RING_K: usize = 8;

/// The epoch a serialized MLS group message was made AT — for a commit, the
/// epoch its still-behind recipients sit on. `None` when the bytes do not
/// parse as a group message (never guess an epoch).
pub fn wire_epoch(bytes: &[u8]) -> Option<u64> {
    let msg = MlsMessageIn::tls_deserialize_exact(bytes).ok()?;
    let protocol: ProtocolMessage = match msg.extract() {
        MlsMessageBodyIn::PrivateMessage(m) => m.into(),
        MlsMessageBodyIn::PublicMessage(m) => m.into(),
        _ => return None,
    };
    Some(protocol.epoch().as_u64())
}

/// The NIP-EE exporter label and length: the outer sealing key of a kind-445
/// event is `export_secret("nostr", &[], 32)` of the epoch.
const EXPORTER_LABEL: &str = "nostr";
const EXPORTER_LEN: usize = 32;

/// Why a group-data change was refused. Both cases are hard drops — there
/// is no "apply provisionally" path, for the same reason `verify_chain` is
/// all-or-nothing: a partially-trusted change could fork state.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GroupDataRefused {
    /// The commit carries no chain-block binding at all.
    #[error("group-data change carries no chain-block binding")]
    Unbound,
    /// The binding names no threshold-decided block of the applied chain.
    #[error("no threshold-decided block authorizes this group-data change")]
    NotAuthorized,
}

/// The stamp to use while the transport carries no per-event timestamp (the
/// loopback mesh today; the Nostr carrier's `created_at` replaces it in N4).
///
/// Both sides MUST use it — the send side in
/// [`MlsMember::restore_member`] and the receive side in
/// [`MlsMember::decrypt_at`]. With equal timestamps the order degrades to
/// the digest alone: still deterministic, still symmetric (exactly one of
/// two distinct commits has the lower digest), only grindable — a committer
/// could search for bytes that hash low. That is the honest cost of having
/// no authenticated timestamp yet, and it is bounded: whoever grinds wins a
/// race they were already party to, nothing more.
pub const NO_CARRIER_STAMP: u64 = 0;

/// The deterministic total order over commits of the SAME epoch
/// (`docs_archive/transport/nostr_n3_plan.md` §1, after MDK's `CommitOrderingKey`).
/// Every node computes it identically from the commit's own bytes and the
/// timestamp its sender stamped, so all of them pick the same winner without
/// talking: **lowest key wins**.
///
/// Timestamp first (concept §1: "lowest `created_at`, then lowest event id"),
/// the digest LAST — a digest-first order would be grindable, since a
/// committer can cheaply search for bytes that hash low.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CommitKey {
    /// The sender's `created_at` (seconds) — the carrier event's timestamp.
    pub created_at: u64,
    /// SHA-256 of the commit bytes: the tiebreak that no clock can forge and
    /// that every node derives from the same bytes.
    pub digest: [u8; 32],
}

impl CommitKey {
    /// The key of `commit_bytes` stamped at `created_at`.
    pub fn new(created_at: u64, commit_bytes: &[u8]) -> CommitKey {
        use sha2::Digest as _;
        let digest: [u8; 32] = sha2::Sha256::digest(commit_bytes).into();
        CommitKey { created_at, digest }
    }
}

/// One node's live MLS membership: the provider (holding all secret state), the
/// signer built from the node's identity key, the node's own handle, and — once
/// created or joined — the group.
pub struct MlsMember {
    provider: OpenMlsRustCrypto,
    signer: SignatureKeyPair,
    name: String,
    /// The PRIOR-STATE SLOT (N3 §1): the snapshot taken immediately before
    /// merging our OWN commit, the epoch it was built on, and that commit's
    /// [`CommitKey`]. It lets us REWIND when a concurrent same-epoch commit
    /// wins the tiebreak — without it, a losing committer would sit in a
    /// state nobody else shares (a silent permanent fork). Cleared as soon
    /// as the epoch moves on.
    /// The fourth element names the leaves that commit REMOVED: a rewind
    /// onto a commit from one of them is refused (an evicted device would
    /// otherwise undo its own eviction with a back-dated commit).
    prior: Option<(u64, Vec<u8>, CommitKey, Vec<String>)>,
    /// The anchored identity key per roster member — the ONLY signature key
    /// a leaf added for that member may carry. `None` = no authority set
    /// (a test group, a workspace without a chain): adds go unchecked, as
    /// they did before the review 2026-08-25 finding. The engine sets it
    /// from the chain's identity table at every runtime build.
    roster_keys: Option<BTreeMap<String, Vec<u8>>>,
    /// While a rewind is applying the winning commit: the leaves the merged
    /// commit removed, whose commit must not win (see `prior`).
    rewind_forbidden: Vec<String>,
    /// The bounded ring of PAST epochs' exporter secrets (newest first), for
    /// the outer envelope layer only — see [`EXPORTER_RING_K`].
    exporter_ring: Vec<[u8; EXPORTER_LEN]>,
    /// WHICH epoch each ring entry belonged to, head-aligned with
    /// [`Self::exporter_ring`] (v3; a legacy snapshot restores with this
    /// empty — its old tail entries stay usable for the opening ladder but
    /// cannot be addressed by epoch). What lets a commit RESEND be sealed
    /// under the epoch it was made at (`detached_reattach.md` §7).
    exporter_ring_epochs: Vec<u64>,
    group: Option<MlsGroup>,
}

/// The self-contained persistence blob: the provider's whole key-value map plus
/// the handles needed to rehydrate the signer and group. bincode (not JSON):
/// the storage keys are `Vec<u8>`, which JSON object keys cannot represent.
/// v2 (N4 §6.1) appends the exporter ring — bincode is positional, so the
/// version byte at offset 0 is what [`MlsMember::restore`] dispatches on;
/// never reorder the leading field.
#[derive(Serialize, Deserialize)]
struct MlsSnapshot {
    version: u8,
    name: String,
    signer_pub: Vec<u8>,
    group_id: Vec<u8>,
    storage: Vec<u8>,
    exporter_ring: Vec<[u8; EXPORTER_LEN]>,
    /// v3: the ring entries' epochs, head-aligned (bincode is positional —
    /// appended field, dispatched by the version byte).
    exporter_ring_epochs: Vec<u64>,
    /// v4: the PRIOR-STATE SLOT (N3 §1) rides the snapshot — a node that
    /// restarted between two concurrent same-epoch commits decided the
    /// tiebreak differently from one that did not (no slot = the loser's
    /// commit is refused instead of rewound onto): a silent fork among
    /// survivors (review 2026-08-25 M3).
    prior: Option<(u64, Vec<u8>, CommitKey, Vec<String>)>,
}

/// The v3 layout (ring + epochs, no prior slot).
#[derive(Serialize, Deserialize)]
struct MlsSnapshotV3 {
    version: u8,
    name: String,
    signer_pub: Vec<u8>,
    group_id: Vec<u8>,
    storage: Vec<u8>,
    exporter_ring: Vec<[u8; EXPORTER_LEN]>,
    exporter_ring_epochs: Vec<u64>,
}

/// The v2 layout (ring, no epochs) — kept so every blob written before the
/// v3 bump keeps restoring; its epochs restore empty (the ladder still
/// works, epoch-addressed resends fall back to one-back).
#[derive(Serialize, Deserialize)]
struct MlsSnapshotV2 {
    version: u8,
    name: String,
    signer_pub: Vec<u8>,
    group_id: Vec<u8>,
    storage: Vec<u8>,
    exporter_ring: Vec<[u8; EXPORTER_LEN]>,
}

/// The pre-N4 blob layout (no ring) — kept so every `transport.state.mls`
/// written before the v2 bump keeps restoring, with the ring empty exactly
/// as those builds behaved.
#[derive(Serialize, Deserialize)]
struct MlsSnapshotV1 {
    version: u8,
    name: String,
    signer_pub: Vec<u8>,
    group_id: Vec<u8>,
    storage: Vec<u8>,
}

impl MlsMember {
    /// Build a member from its derived Ed25519 identity key and handle. The
    /// signer wraps the *same* key the genesis identity table anchors — the
    /// 32-byte seed and public key go in verbatim (`from_raw`), never
    /// regenerated. No group yet: the caller either [`create_group`] (founder)
    /// or [`join_from_welcome`] (joiner).
    ///
    /// [`create_group`]: MlsMember::create_group
    /// [`join_from_welcome`]: MlsMember::join_from_welcome
    pub fn new(signing_key: &SigningKey, name: &str) -> Result<MlsMember, MlsError> {
        let provider = OpenMlsRustCrypto::default();
        let signer = SignatureKeyPair::from_raw(
            SignatureScheme::ED25519,
            signing_key.to_bytes().to_vec(),
            signing_key.verifying_key().to_bytes().to_vec(),
        );
        signer
            .store(provider.storage())
            .map_err(|e| MlsError::Mls(format!("storing signer: {e:?}")))?;
        Ok(MlsMember {
            provider,
            signer,
            name: name.to_string(),
            group: None,
            prior: None,
            roster_keys: None,
            rewind_forbidden: Vec::new(),
            exporter_ring: Vec::new(),
            exporter_ring_epochs: Vec::new(),
        })
    }

    fn credential(&self) -> CredentialWithKey {
        CredentialWithKey {
            credential: BasicCredential::new(self.name.as_bytes().to_vec()).into(),
            signature_key: self.signer.public().into(),
        }
    }

    /// The wire-serialized KeyPackage this member advertises so the founder can
    /// add it. Its private halves are already in this member's provider — the
    /// same provider must live until the Welcome is processed, then be
    /// snapshotted.
    pub fn key_package(&self) -> Result<Vec<u8>, MlsError> {
        let bundle = KeyPackage::builder()
            .build(SUITE, &self.provider, &self.signer, self.credential())
            .map_err(|e| MlsError::Mls(format!("building key package: {e:?}")))?;
        bundle
            .key_package()
            .tls_serialize_detached()
            .map_err(|e| MlsError::Wire(format!("serializing key package: {e}")))
    }

    /// Founder: create the group with this member as its sole initial leaf.
    /// Idempotent guard: a second call is a bug and errors rather than silently
    /// replacing a live group.
    pub fn create_group(&mut self) -> Result<(), MlsError> {
        if self.group.is_some() {
            return Err(MlsError::Mls("group already exists".into()));
        }
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(SUITE)
            // ship the ratchet tree in-band so a joiner needs nothing else
            .use_ratchet_tree_extension(true)
            // NO past-epoch receive window (`max_past_epochs` stays 0): the
            // recovery re-key exists to EVICT a possibly-compromised device,
            // and past-epoch keys would let that evicted leaf keep speaking as
            // the member, authenticated, until further re-keys (pinned by
            // `the_evicted_leaf_cannot_speak_after_the_rekey`). The price: a
            // delayed pre-re-key message crossing the commit is dropped
            // (chat is ephemeral; chain blocks have catch-up; the delivery
            // guarantee's resend re-encrypts at the current epoch). Forward-
            // racing messages are covered separately by the FutureEpoch retry.
            .sender_ratchet_configuration(ratchet_window())
            .build();
        let group = MlsGroup::new(&self.provider, &self.signer, &config, self.credential())
            .map_err(|e| MlsError::Mls(format!("creating group: {e:?}")))?;
        self.group = Some(group);
        Ok(())
    }

    /// Founder: add every member from its wire KeyPackage in a single commit and
    /// return the one Welcome that covers them all (empty input → no commit,
    /// `None`). The commit itself needs no distribution: at founding the founder
    /// is the only prior member and merges it locally. Every added member
    /// processes the same Welcome.
    pub fn add_members(&mut self, key_packages: &[Vec<u8>]) -> Result<Option<Vec<u8>>, MlsError> {
        if key_packages.is_empty() {
            return Ok(None);
        }
        let mut parsed = Vec::with_capacity(key_packages.len());
        for kp in key_packages {
            let kp_in = KeyPackageIn::tls_deserialize_exact(kp)
                .map_err(|e| MlsError::Wire(format!("parsing key package: {e}")))?;
            let validated = kp_in
                .validate(self.provider.crypto(), ProtocolVersion::Mls10)
                .map_err(|e| MlsError::Wire(format!("invalid key package: {e:?}")))?;
            parsed.push(validated);
        }
        // this add advances the epoch, so its outgoing exporter secret has
        // to enter the ring — otherwise catch-up across a founding-time add
        // hits a hole nobody can strip
        self.retire_exporter();
        let group = self.group.as_mut().ok_or(MlsError::NoGroup)?;
        let (_commit, welcome, _group_info) = group
            .add_members(&self.provider, &self.signer, &parsed)
            .map_err(|e| MlsError::Mls(format!("adding members: {e:?}")))?;
        group
            .merge_pending_commit(&self.provider)
            .map_err(|e| MlsError::Mls(format!("merging add commit: {e:?}")))?;
        let bytes = welcome
            .to_bytes()
            .map_err(|e| MlsError::Wire(format!("serializing welcome: {e}")))?;
        Ok(Some(bytes))
    }

    /// Approver: **restore** a member's seat — remove its lost leaf (found by
    /// the credential handle) and add the rejoiner's fresh KeyPackage in ONE
    /// commit. Returns `(commit, welcome)`: the commit is broadcast to the OTHER
    /// existing members (they merge it to advance the epoch and drop the old
    /// leaf), the Welcome brings the rejoiner in. The rejoiner re-derives the
    /// SAME identity from its phrase, so the new leaf's credential equals the
    /// removed one — a re-key of the same seat, not a new member (concept §3.3).
    /// `created_at` is the stamp that enters this commit's [`CommitKey`]:
    /// the timestamp of the CARRIER EVENT the commit rides on, so the
    /// tiebreak every other node computes matches ours exactly. There is
    /// deliberately no wall-clock default — a locally-read clock on the send
    /// side and a wire value on the receive side are not the same order, and
    /// two nodes would pick different winners (review finding 2026-07-31).
    /// While the transport carries no timestamp, pass
    /// [`NO_CARRIER_STAMP`] here AND to [`decrypt_at`](MlsMember::decrypt_at)
    /// — symmetric on both sides, which is what convergence needs.
    pub fn restore_member(
        &mut self,
        member: &str,
        new_key_package: &[u8],
        created_at: u64,
    ) -> Result<(Vec<u8>, Vec<u8>), MlsError> {
        let kp_in = KeyPackageIn::tls_deserialize_exact(new_key_package)
            .map_err(|e| MlsError::Wire(format!("parsing key package: {e}")))?;
        let validated = kp_in
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)
            .map_err(|e| MlsError::Wire(format!("invalid key package: {e:?}")))?;
        let group = self.group.as_mut().ok_or(MlsError::NoGroup)?;
        // the leaf whose credential handle is this member (its lost identity)
        let old_leaf = group
            .members()
            .find(|m| m.credential.serialized_content() == member.as_bytes())
            .map(|m| m.index)
            .ok_or_else(|| MlsError::Mls(format!("no leaf anchors {member}")))?;
        // remove + add as INLINE proposals in ONE commit (the commit-builder
        // inlines them, so a recipient needs no prior proposal distribution)
        let bundle = group
            .commit_builder()
            .propose_removals([old_leaf])
            .propose_adds([validated])
            .load_psks(self.provider.storage())
            .map_err(|e| MlsError::Mls(format!("loading psks: {e:?}")))?
            // ship the ratchet tree in the Welcome so the rejoiner needs nothing
            // else (the group's join config may not carry the create-time flag)
            .use_ratchet_tree_extension(true)
            .build(
                self.provider.rand(),
                self.provider.crypto(),
                &self.signer,
                |_| true,
            )
            .map_err(|e| MlsError::Mls(format!("building restore commit: {e:?}")))?
            .stage_commit(&self.provider)
            .map_err(|e| MlsError::Mls(format!("staging restore commit: {e:?}")))?;
        let (commit, welcome, _group_info) = bundle.into_messages();
        let commit_bytes = commit
            .to_bytes()
            .map_err(|e| MlsError::Wire(format!("serializing restore commit: {e}")))?;
        // PRIOR-STATE SLOT before we merge our own commit: a concurrent
        // same-epoch commit may win the tiebreak, and then we must rewind to
        // exactly this state instead of forking the group (N3 §1)
        self.arm_prior_slot(created_at, &commit_bytes, vec![member.to_string()])?;
        self.retire_exporter();
        let group = self.group.as_mut().ok_or(MlsError::NoGroup)?;
        group
            .merge_pending_commit(&self.provider)
            .map_err(|e| MlsError::Mls(format!("merging restore commit: {e:?}")))?;
        let welcome_bytes = welcome
            .ok_or_else(|| MlsError::Mls("restore produced no welcome".into()))?
            .to_bytes()
            .map_err(|e| MlsError::Wire(format!("serializing restore welcome: {e}")))?;
        Ok((commit_bytes, welcome_bytes))
    }

    /// Joiner: enter the group from the founder's Welcome bytes. The ratchet
    /// tree rides the Welcome (`use_ratchet_tree_extension`), so nothing else is
    /// needed.
    pub fn join_from_welcome(&mut self, welcome_bytes: &[u8]) -> Result<(), MlsError> {
        if self.group.is_some() {
            return Err(MlsError::Mls("already in a group".into()));
        }
        let msg = MlsMessageIn::tls_deserialize_exact(welcome_bytes)
            .map_err(|e| MlsError::Wire(format!("parsing welcome: {e}")))?;
        let welcome = match msg.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            other => {
                return Err(MlsError::Wire(format!(
                    "expected a welcome, got {other:?}"
                )))
            }
        };
        // same policy as create_group: NO past-epoch receive window — the
        // eviction property of a recovery re-key outranks delayed delivery —
        // and the same widened forward window (§4.6)
        let config = MlsGroupJoinConfig::builder()
            .sender_ratchet_configuration(ratchet_window())
            .build();
        let staged = StagedWelcome::new_from_welcome(&self.provider, &config, welcome, None)
            .map_err(|e| MlsError::Mls(format!("staging welcome: {e:?}")))?;
        let group = staged
            .into_group(&self.provider)
            .map_err(|e| MlsError::Mls(format!("joining from welcome: {e:?}")))?;
        self.group = Some(group);
        Ok(())
    }

    /// Encrypt one application payload into an MLS message (wire bytes). Mutates
    /// the ratchet — the caller must persist [`snapshot`](MlsMember::snapshot)
    /// before the ciphertext leaves the node (write-ahead, concept §6).
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, MlsError> {
        let group = self.group.as_mut().ok_or(MlsError::NoGroup)?;
        let out = group
            .create_message(&self.provider, &self.signer, plaintext)
            .map_err(|e| MlsError::Mls(format!("encrypting: {e:?}")))?;
        out.to_bytes()
            .map_err(|e| MlsError::Wire(format!("serializing message: {e}")))
    }

    /// Decrypt one inbound MLS message. Application messages return the
    /// authenticated sender handle and plaintext; membership commits are merged
    /// (epoch advances); proposals are stored. Mutates state — the caller must
    /// persist [`snapshot`](MlsMember::snapshot) before an application plaintext
    /// reaches the engine (write-ahead, concept §6). Untrusted input: parse and
    /// verification failures are `Err`, never panics.
    pub fn decrypt(&mut self, wire: &[u8]) -> Result<MlsIncoming, MlsError> {
        self.decrypt_at(wire, NO_CARRIER_STAMP)
    }

    /// [`decrypt`](MlsMember::decrypt) with the carrier event's `created_at`.
    ///
    /// **Use this for anything that may carry a commit.** The timestamp is
    /// half of the [`CommitKey`] that breaks a concurrent same-epoch commit
    /// race, and it must be the value EVERY node sees — the wire timestamp,
    /// never a local clock read, or two nodes could pick different winners
    /// and fork exactly where this mechanism exists to converge (N3 §1).
    pub fn decrypt_at(&mut self, wire: &[u8], created_at: u64) -> Result<MlsIncoming, MlsError> {
        let msg = MlsMessageIn::tls_deserialize_exact(wire)
            .map_err(|e| MlsError::Wire(format!("parsing message: {e}")))?;
        let protocol: ProtocolMessage = match msg.extract() {
            MlsMessageBodyIn::PrivateMessage(m) => m.into(),
            MlsMessageBodyIn::PublicMessage(m) => m.into(),
            other => return Err(MlsError::Wire(format!("not a group message: {other:?}"))),
        };
        let group = self.group.as_mut().ok_or(MlsError::NoGroup)?;
        // a message claiming an epoch we have NOT reached yet is not an error —
        // its re-key commit is still in flight. Nothing is consumed; the caller
        // buffers the bytes and feeds them back once a commit merges (the
        // transport's cross-epoch retry). The claimed epoch is unauthenticated
        // at this point, so the caller's buffer must be bounded.
        if protocol.epoch().as_u64() > group.epoch().as_u64() {
            return Ok(MlsIncoming::FutureEpoch);
        }
        // CONCURRENT COMMIT (N3 §1): a message built on the epoch our OWN
        // in-flight commit was built on. Both commits are valid, both nodes
        // merged their own — without a shared rule they now hold different
        // key schedules under the same epoch number, forever and silently.
        // The rule: lowest CommitKey wins. We either keep ours (the other
        // side rewinds to it) or rewind to the state our commit was built on
        // and apply theirs.
        // …and ONLY for a commit: an application message of the prior epoch
        // is a forward-secrecy question, not a race. Treating one as a
        // commit would rewind the group into a state that can still read
        // old traffic — the exact hole `max_past_epochs = 0` exists to
        // close. The content type rides in the cleartext framing header.
        if let Some((prior_epoch, _, own_key, _)) = &self.prior {
            if protocol.epoch().as_u64() == *prior_epoch
                && protocol.content_type() == openmls::framing::ContentType::Commit
            {
                let foreign = CommitKey::new(created_at, wire);
                if *own_key <= foreign {
                    return Ok(MlsIncoming::CommitSuperseded);
                }
                return self.rewind_and_apply(wire, created_at);
            }
        }
        let processed = group
            .process_message(&self.provider, protocol)
            .map_err(|e| MlsError::Wire(format!("processing message: {e:?}")))?;
        let from = String::from_utf8_lossy(processed.credential().serialized_content()).into_owned();
        // during a REWIND only: the commit we are about to undo removed
        // these leaves — one of them re-deciding the epoch with a lower
        // (back-dated, self-chosen) key would undo its own eviction
        if self.rewind_forbidden.contains(&from) {
            return Err(MlsError::Wire(format!("removed leaf {from}: cannot win the epoch")));
        }
        match processed.into_content() {
            // NOTE (review 2026-07-31): application traffic must NOT clear
            // the slot. Doing so re-opened the bystander fork: a member that
            // merged the loser, then accepted one message, could no longer
            // take the winning commit and was stranded alone forever. The
            // slot is already bounded — `arm_prior_slot` runs on EVERY merge,
            // so it always points exactly one epoch back — and a replay of
            // the commit we applied is a no-op, because the tiebreak treats
            // an equal key as superseded. Only a strictly LOWER key rewinds,
            // which is precisely the convergence rule.
            ProcessedMessageContent::ApplicationMessage(app) => Ok(MlsIncoming::Application {
                from,
                plaintext: app.into_bytes(),
            }),
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                // WHO does this commit add, read BEFORE the merge consumes
                // it: a recovery re-key re-adds the recovered seat, and the
                // caller must reset that member's accept window in-stream
                // (see [`MlsIncoming::Commit`]).
                let readmitted: Vec<String> = staged
                    .add_proposals()
                    .map(|add| {
                        String::from_utf8_lossy(
                            add.add_proposal()
                                .key_package()
                                .leaf_node()
                                .credential()
                                .serialized_content(),
                        )
                        .into_owned()
                    })
                    .collect();
                // AUTHORIZATION, before anything is merged: a leaf added for
                // a roster member must carry that member's anchored identity
                // key — the same pairing the founder enforced at join. A
                // refusal leaves the epoch untouched (the commit was only
                // staged).
                if let Some(roster) = &self.roster_keys {
                    for add in staged.add_proposals() {
                        let leaf = add.add_proposal().key_package().leaf_node();
                        let name =
                            String::from_utf8_lossy(leaf.credential().serialized_content())
                                .into_owned();
                        let key = leaf.signature_key().as_slice();
                        if roster.get(&name).map(Vec::as_slice) != Some(key) {
                            return Err(MlsError::Wire(format!(
                                "re-key refused: leaf for {name} is not its anchored key"
                            )));
                        }
                    }
                }
                // WHO this commit removes, read BEFORE the merge drops the
                // leaves: a later rewind must not hand the epoch to one of them
                let removed: Vec<String> = {
                    let group = self.group.as_ref().ok_or(MlsError::NoGroup)?;
                    let gone: Vec<_> = staged
                        .remove_proposals()
                        .map(|r| r.remove_proposal().removed())
                        .collect();
                    group
                        .members()
                        .filter(|m| gone.contains(&m.index))
                        .map(|m| {
                            String::from_utf8_lossy(m.credential.serialized_content()).into_owned()
                        })
                        .collect()
                };
                let retiring = self.exporter_secret().ok();
                let retiring_epoch = self.epoch();
                // BYSTANDERS CONVERGE TOO (review finding 2026-07-31): a node
                // that authored no commit still has to survive a race — it
                // merges whichever of two concurrent commits arrives first,
                // and without a rewind slot the loser's branch would be its
                // permanent home. Arming the slot on EVERY merge gives every
                // node the same rule as the committers.
                self.arm_prior_slot(created_at, wire, removed)?;
                let group = self.group.as_mut().ok_or(MlsError::NoGroup)?;
                group
                    .merge_staged_commit(&self.provider, *staged)
                    .map_err(|e| MlsError::Mls(format!("merging commit: {e:?}")))?;
                if let Some(secret) = retiring {
                    if self.exporter_ring.first() != Some(&secret) {
                        self.exporter_ring.insert(0, secret);
                        self.exporter_ring.truncate(EXPORTER_RING_K);
                        self.exporter_ring_epochs.insert(0, retiring_epoch);
                        self.exporter_ring_epochs.truncate(EXPORTER_RING_K);
                    }
                }
                Ok(MlsIncoming::Commit { readmitted })
            }
            ProcessedMessageContent::ProposalMessage(p) => {
                group
                    .store_pending_proposal(self.provider.storage(), *p)
                    .map_err(|e| MlsError::Mls(format!("storing proposal: {e:?}")))?;
                Ok(MlsIncoming::Proposal)
            }
            ProcessedMessageContent::ExternalJoinProposalMessage(_) => Ok(MlsIncoming::Proposal),
        }
    }

    /// The N3 §5 gate: may a group-data change carrying `block_hash` be
    /// applied? Call this BEFORE processing such a commit — a refusal must
    /// drop the commit with the epoch untouched. Test-only until N6 wires
    /// it (see [`ChainOracle`]).
    #[cfg(test)]
    pub fn authorize_group_data(
        &self,
        oracle: &dyn ChainOracle,
        block_hash: Option<&str>,
    ) -> Result<(), GroupDataRefused> {
        let hash = block_hash.ok_or(GroupDataRefused::Unbound)?;
        if oracle.authorizes(hash) {
            Ok(())
        } else {
            Err(GroupDataRefused::NotAuthorized)
        }
    }

    /// This epoch's exporter secret — the OUTER envelope key of a kind-445
    /// event (§10.11: `ChaCha20Poly1305(exporter_secret, …)`). It
    /// authenticates nothing and grants no MLS read capability.
    pub fn exporter_secret(&self) -> Result<[u8; EXPORTER_LEN], MlsError> {
        let group = self.group.as_ref().ok_or(MlsError::NoGroup)?;
        let raw = group
            .export_secret(self.provider.crypto(), EXPORTER_LABEL, &[], EXPORTER_LEN)
            .map_err(|e| MlsError::Mls(format!("exporting secret: {e:?}")))?;
        raw.try_into()
            .map_err(|_| MlsError::Mls("exporter secret has the wrong length".into()))
    }

    /// The PAST epochs' exporter secrets still held for the outer layer,
    /// newest first (bounded by [`EXPORTER_RING_K`]).
    pub fn exporter_ring(&self) -> &[[u8; EXPORTER_LEN]] {
        &self.exporter_ring
    }

    /// Push the CURRENT epoch's exporter secret into the ring — called right
    /// before an epoch change, so the secret that just became "past" stays
    /// strippable for the outer layer.
    fn retire_exporter(&mut self) {
        let epoch = self.epoch();
        if let Ok(secret) = self.exporter_secret() {
            if self.exporter_ring.first() != Some(&secret) {
                self.exporter_ring.insert(0, secret);
                self.exporter_ring.truncate(EXPORTER_RING_K);
                self.exporter_ring_epochs.insert(0, epoch);
                self.exporter_ring_epochs.truncate(EXPORTER_RING_K);
            }
        }
    }

    /// The exporter secret of ONE specific epoch — the current one, or a
    /// ring entry whose epoch label matches (head-aligned; legacy tail
    /// entries without labels are not addressable). What an epoch-correct
    /// commit RESEND seals under (`detached_reattach.md` §7).
    pub fn exporter_for_epoch(&self, epoch: u64) -> Option<[u8; EXPORTER_LEN]> {
        if self.group.is_some() && self.epoch() == epoch {
            return self.exporter_secret().ok();
        }
        let i = self.exporter_ring_epochs.iter().position(|e| *e == epoch)?;
        self.exporter_ring.get(i).copied()
    }

    /// Snapshot the CURRENT (pre-merge) state into the prior slot, so a
    /// losing concurrent commit can be rewound (N3 §1).
    fn arm_prior_slot(
        &mut self,
        created_at: u64,
        commit_bytes: &[u8],
        removed: Vec<String>,
    ) -> Result<(), MlsError> {
        let epoch = self.epoch();
        let snapshot = self.snapshot()?;
        self.prior = Some((epoch, snapshot, CommitKey::new(created_at, commit_bytes), removed));
        Ok(())
    }

    /// Rewind to the prior slot and apply the WINNING foreign commit instead
    /// of our own. The snapshot is the exact state our commit was built on,
    /// so processing the winner there is what every node that never saw our
    /// commit does.
    fn rewind_and_apply(
        &mut self,
        winner: &[u8],
        created_at: u64,
    ) -> Result<MlsIncoming, MlsError> {
        let Some((_, snapshot, _, removed)) = self.prior.clone() else {
            return Err(MlsError::NoGroup);
        };
        // TRANSACTIONAL: the "winner" is unauthenticated until it processes
        // (the content type is cleartext framing, so anything can claim to
        // be a commit). Keep the current state; only commit to the rewind
        // once the winner really applies, otherwise a forged frame could
        // roll a node back at will.
        let current = self.snapshot()?;
        let armed = self.prior.take(); // DISARM first: the winner re-enters
                                       // decrypt, and a still-armed slot
                                       // would send it straight back here
        let rewound = MlsMember::restore(&snapshot)?;
        self.provider_swap(rewound);
        self.rewind_forbidden = removed;
        let outcome = self.decrypt_at(winner, created_at);
        self.rewind_forbidden.clear();
        match outcome {
            // the winner merged — but say WHOSE work was dropped doing so
            Ok(MlsIncoming::Commit { readmitted }) => Ok(MlsIncoming::CommitRewound { readmitted }),
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                // the "winner" did not apply after all — undo the rewind and
                // re-arm, so a forged frame cannot roll this node back
                let back = MlsMember::restore(&current)?;
                self.provider_swap(back);
                self.prior = armed;
                Err(e)
            }
        }
    }

    /// Adopt another member value's secret state (provider + signer + group)
    /// into `self`, keeping the handle. The one place group state is
    /// replaced wholesale — used by the rewind path only.
    fn provider_swap(&mut self, other: MlsMember) {
        self.provider = other.provider;
        self.signer = other.signer;
        self.group = other.group;
    }

    /// The anchored identity signature key per roster member (raw bytes,
    /// as `key_package_binding` reports them). Once set, a commit that adds
    /// a leaf for `member` under any OTHER key is refused before the merge:
    /// without it any current leaf could re-key any seat under a key it
    /// holds and speak as that member (review 2026-08-25, HIGH).
    pub fn set_roster_keys(&mut self, keys: BTreeMap<String, Vec<u8>>) {
        self.roster_keys = Some(keys);
    }

    /// This node's own handle.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether a group has been created or joined.
    pub fn has_group(&self) -> bool {
        self.group.is_some()
    }

    /// The current epoch (0 before any membership change), for dedup/debugging.
    pub fn epoch(&self) -> u64 {
        self.group.as_ref().map(|g| g.epoch().as_u64()).unwrap_or(0)
    }

    /// Serialize the entire MLS state (provider storage + handles) into an
    /// opaque blob for `transport.state`. Requires a group (only meaningful once
    /// founding sealed). The blob is always the *current* state — the caller
    /// overwrites, never appends.
    pub fn snapshot(&self) -> Result<Vec<u8>, MlsError> {
        let group = self.group.as_ref().ok_or(MlsError::NoGroup)?;
        let storage = {
            let map = self
                .provider
                .storage()
                .values
                .read()
                .map_err(|_| MlsError::Snapshot("storage lock poisoned".into()))?;
            bincode::serialize(&*map)
                .map_err(|e| MlsError::Snapshot(format!("encoding storage: {e}")))?
        };
        let snap = MlsSnapshot {
            version: SNAPSHOT_VERSION,
            name: self.name.clone(),
            signer_pub: self.signer.public().to_vec(),
            group_id: group.group_id().as_slice().to_vec(),
            storage,
            exporter_ring: self.exporter_ring.clone(),
            exporter_ring_epochs: self.exporter_ring_epochs.clone(),
            prior: self.prior.clone(),
        };
        bincode::serialize(&snap).map_err(|e| MlsError::Snapshot(format!("encoding snapshot: {e}")))
    }

    /// Rehydrate a member from a [`snapshot`](MlsMember::snapshot) blob. Fully
    /// self-contained: the signer's private half round-trips inside the provider
    /// storage, so no external key is needed. Dispatches on the version byte
    /// at offset 0: v2 carries the exporter ring (N4 §6.1); a v1 blob (pre-N4)
    /// restores with an empty ring — past-epoch catch-up falls back to the
    /// ACK/rewind layer until the ring re-fills, exactly as those builds ran.
    pub fn restore(blob: &[u8]) -> Result<MlsMember, MlsError> {
        let snap: MlsSnapshot = match blob.first() {
            Some(&SNAPSHOT_VERSION) => bincode::deserialize(blob)
                .map_err(|e| MlsError::Snapshot(format!("decoding snapshot: {e}")))?,
            Some(3) => {
                let v3: MlsSnapshotV3 = bincode::deserialize(blob)
                    .map_err(|e| MlsError::Snapshot(format!("decoding v3 snapshot: {e}")))?;
                MlsSnapshot {
                    version: SNAPSHOT_VERSION,
                    name: v3.name,
                    signer_pub: v3.signer_pub,
                    group_id: v3.group_id,
                    storage: v3.storage,
                    exporter_ring: v3.exporter_ring,
                    exporter_ring_epochs: v3.exporter_ring_epochs,
                    prior: None,
                }
            }
            Some(2) => {
                let v2: MlsSnapshotV2 = bincode::deserialize(blob)
                    .map_err(|e| MlsError::Snapshot(format!("decoding v2 snapshot: {e}")))?;
                MlsSnapshot {
                    version: SNAPSHOT_VERSION,
                    name: v2.name,
                    signer_pub: v2.signer_pub,
                    group_id: v2.group_id,
                    storage: v2.storage,
                    exporter_ring: v2.exporter_ring,
                    exporter_ring_epochs: Vec::new(),
                    prior: None,
                }
            }
            Some(1) => {
                let v1: MlsSnapshotV1 = bincode::deserialize(blob)
                    .map_err(|e| MlsError::Snapshot(format!("decoding v1 snapshot: {e}")))?;
                MlsSnapshot {
                    version: SNAPSHOT_VERSION,
                    name: v1.name,
                    signer_pub: v1.signer_pub,
                    group_id: v1.group_id,
                    storage: v1.storage,
                    exporter_ring: Vec::new(),
                    exporter_ring_epochs: Vec::new(),
                    prior: None,
                }
            }
            other => {
                return Err(MlsError::Snapshot(format!(
                    "unsupported snapshot version {other:?}"
                )));
            }
        };
        let provider = OpenMlsRustCrypto::default();
        {
            let map: HashMap<Vec<u8>, Vec<u8>> = bincode::deserialize(&snap.storage)
                .map_err(|e| MlsError::Snapshot(format!("decoding storage: {e}")))?;
            *provider
                .storage()
                .values
                .write()
                .map_err(|_| MlsError::Snapshot("storage lock poisoned".into()))? = map;
        }
        let signer =
            SignatureKeyPair::read(provider.storage(), &snap.signer_pub, SignatureScheme::ED25519)
                .ok_or_else(|| MlsError::Snapshot("signer missing from restored storage".into()))?;
        let group_id = GroupId::from_slice(&snap.group_id);
        let group = MlsGroup::load(provider.storage(), &group_id)
            .map_err(|e| MlsError::Snapshot(format!("loading group: {e:?}")))?
            .ok_or_else(|| MlsError::Snapshot("group missing from restored storage".into()))?;
        Ok(MlsMember {
            provider,
            signer,
            name: snap.name,
            group: Some(group),
            // v4 carries the slot; older blobs restore without one
            prior: snap.prior,
            roster_keys: None,
            rewind_forbidden: Vec::new(),
            // v2 blobs carry the ring; a v1 blob restored it empty above
            exporter_ring: snap.exporter_ring,
            exporter_ring_epochs: snap.exporter_ring_epochs,
        })
    }
}

/// The identity a wire `KeyPackage` commits to: its credential identity (the
/// member handle bytes) and its signature public key. The founder uses this to
/// enforce that a joiner's KeyPackage matches the exact `(name, key)` it
/// anchored in the roster — otherwise a joiner could send a MAC-valid
/// `JoinRequest` for one handle but a KeyPackage credentialed as another, and
/// authenticate inside the group under a handle the roster does not bind
/// ("one identity, two anchors", concept §3.3). Validates the KeyPackage
/// itself (self-consistency, signature, lifetime) as a side effect.
pub fn key_package_binding(kp_bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), MlsError> {
    let provider = OpenMlsRustCrypto::default();
    let kp_in = KeyPackageIn::tls_deserialize_exact(kp_bytes)
        .map_err(|e| MlsError::Wire(format!("parsing key package: {e}")))?;
    let kp = kp_in
        .validate(provider.crypto(), ProtocolVersion::Mls10)
        .map_err(|e| MlsError::Wire(format!("invalid key package: {e:?}")))?;
    let leaf = kp.leaf_node();
    let identity = leaf.credential().serialized_content().to_vec();
    let sig_key = leaf.signature_key().as_slice().to_vec();
    Ok((identity, sig_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// The full ritual-shaped flow through the module API: founder creates,
    /// adds two members from their wire KeyPackages, each joins from the one
    /// Welcome, and all three exchange authenticated application messages.
    /// Assert a decrypted message is an application message from `who`.
    fn assert_app(incoming: MlsIncoming, who: &str, body: &[u8]) {
        match incoming {
            MlsIncoming::Application { from, plaintext } => {
                assert_eq!(from, who, "authenticated sender");
                assert_eq!(plaintext, body);
            }
            other => panic!("expected an application message, got {other:?}"),
        }
    }

    /// N4 §6.1 (the N3 §5.5 debt): the exporter ring survives the
    /// snapshot/restore cycle. Without this, a restarted node cannot strip
    /// the outer layer of any 445 sealed under a recent prior epoch and the
    /// catch-up silently degrades to the ACK/rewind layer.
    #[test]
    fn the_exporter_ring_survives_snapshot_and_restore() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        founder.create_group().expect("create group");
        founder
            .add_members(&[bob.key_package().expect("bob kp")])
            .expect("add")
            .expect("welcome");
        assert!(
            !founder.exporter_ring().is_empty(),
            "the add_members epoch change must have retired a secret into the ring"
        );
        let ring_before = founder.exporter_ring().to_vec();

        let blob = founder.snapshot().expect("snapshot");
        let restored = MlsMember::restore(&blob).expect("restore");
        assert_eq!(
            restored.exporter_ring(),
            ring_before.as_slice(),
            "the ring must round-trip through the snapshot"
        );
    }

    /// A pre-N4 (version-1) snapshot blob still restores — with an empty
    /// ring, exactly the old behavior. The version byte is the first byte of
    /// the blob; nothing else about the v1 layout is reinterpreted.
    #[test]
    fn a_legacy_v1_snapshot_blob_still_restores_with_an_empty_ring() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        founder.create_group().expect("create group");
        founder
            .add_members(&[bob.key_package().expect("bob kp")])
            .expect("add")
            .expect("welcome");

        let v2_blob = founder.snapshot().expect("snapshot");
        let snap: MlsSnapshot = bincode::deserialize(&v2_blob).expect("decode own snapshot");
        let v1_blob = bincode::serialize(&MlsSnapshotV1 {
            version: 1,
            name: snap.name,
            signer_pub: snap.signer_pub,
            group_id: snap.group_id,
            storage: snap.storage,
        })
        .expect("encode v1 twin");

        let restored = MlsMember::restore(&v1_blob).expect("a v1 blob must restore");
        assert!(
            restored.exporter_ring().is_empty(),
            "a legacy blob carries no ring — it re-fills as epochs advance"
        );
        assert_eq!(restored.name(), "founder");
    }

    /// Delivery guarantee §4.6 (V5): a receiver must survive a FORWARD jump
    /// far beyond openmls's default `maximum_forward_distance = 1000` — a
    /// deaf leg can swallow thousands of sender-ratchet generations (every
    /// send advances it, delivered or not) and the first message after the
    /// heal arrives with that whole gap at once. With the default config
    /// this decrypt fails and the leg is dead forever.
    #[test]
    fn a_forward_jump_beyond_the_old_default_window_still_decrypts() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        founder.create_group().expect("create group");
        let welcome = founder
            .add_members(&[bob.key_package().expect("bob kp")])
            .expect("add")
            .expect("welcome");
        let mut bob = bob;
        bob.join_from_welcome(&welcome).expect("bob joins");

        // 3000 sends into the void (a deaf queue) — only the last arrives
        let mut last = Vec::new();
        for _ in 0..3000 {
            last = founder.encrypt(b"into the void").expect("encrypt");
        }
        assert_app(
            bob.decrypt(&last).expect("a >1000-generation jump must decrypt"),
            "founder",
            b"into the void",
        );
    }

    /// §4.6 backward window (the 2026-07-28 live validation): the deaf
    /// window's server-stored ORIGINALS arrive after the receiver already
    /// consumed the fresh resends — generations far BEHIND its ratchet.
    /// The default tolerance of 5 discarded them (`TooDistantInThePast`)
    /// and forced every late frame onto the next resend backoff.
    #[test]
    fn frames_far_behind_the_ratchet_still_decrypt_after_the_heal() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        founder.create_group().expect("create group");
        let welcome = founder
            .add_members(&[bob.key_package().expect("bob kp")])
            .expect("add")
            .expect("welcome");
        let mut bob = bob;
        bob.join_from_welcome(&welcome).expect("bob joins");

        // 100 originals go into the (deaf) queue; the RESEND of the last
        // one is consumed first, advancing bob's ratchet far past them
        let originals: Vec<Vec<u8>> =
            (0..100).map(|i| founder.encrypt(format!("msg-{i}").as_bytes()).expect("encrypt")).collect();
        let resend = founder.encrypt(b"resend of the tail").expect("encrypt resend");
        assert_app(bob.decrypt(&resend).expect("the fresh resend decrypts"), "founder", b"resend of the tail");
        // …then the queue delivers the stored originals: ~100 generations
        // behind, every one must still decrypt (old default: discard at >5)
        assert_app(
            bob.decrypt(&originals[0]).expect("a 100-generation-late original decrypts"),
            "founder",
            b"msg-0",
        );
        assert_app(
            bob.decrypt(&originals[50]).expect("mid-window late original decrypts"),
            "founder",
            b"msg-50",
        );
    }

    #[test]
    fn founder_adds_two_members_and_all_three_chat() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        let cara = MlsMember::new(&key(3), "cara").expect("cara");

        founder.create_group().expect("create group");
        let kps = vec![
            bob.key_package().expect("bob kp"),
            cara.key_package().expect("cara kp"),
        ];
        let welcome = founder.add_members(&kps).expect("add").expect("a welcome");

        let mut bob = bob;
        let mut cara = cara;
        bob.join_from_welcome(&welcome).expect("bob joins");
        cara.join_from_welcome(&welcome).expect("cara joins");

        // founder → the others
        let ct = founder.encrypt(b"agenda: fix the fence").expect("encrypt");
        for m in [&mut bob, &mut cara] {
            assert_app(m.decrypt(&ct).expect("decrypt"), "founder", b"agenda: fix the fence");
        }

        // a member → founder
        let ct = bob.encrypt(b"+1").expect("encrypt");
        assert_app(founder.decrypt(&ct).expect("decrypt"), "bob", b"+1");
    }

    /// Recovery (concept §3.3): an existing member removes a lost member's leaf
    /// and adds its RE-DERIVED KeyPackage in one commit. The other members merge
    /// the commit; the rejoiner joins from the welcome; the re-keyed seat chats;
    /// and the old, removed leaf is locked out of the new epoch.
    #[test]
    fn restore_member_rekeys_the_seat_and_the_rejoiner_chats() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        let cara = MlsMember::new(&key(3), "cara").expect("cara");
        founder.create_group().expect("create");
        let welcome = founder
            .add_members(&[
                bob.key_package().expect("bob kp"),
                cara.key_package().expect("cara kp"),
            ])
            .expect("add")
            .expect("welcome");
        let mut bob = bob;
        let mut cara = cara;
        bob.join_from_welcome(&welcome).expect("bob joins");
        cara.join_from_welcome(&welcome).expect("cara joins");

        // bob lost its workspace; a fresh node re-derives the SAME identity (key 2)
        let bob2 = MlsMember::new(&key(2), "bob").expect("bob2");
        let bob2_kp = bob2.key_package().expect("bob2 kp");

        // cara (an existing member, NOT the founder) approves: remove old bob + add bob2
        let (commit, welcome2) = cara.restore_member("bob", &bob2_kp, NO_CARRIER_STAMP).expect("restore");

        // every OTHER existing member merges the commit to advance the epoch
        match founder.decrypt(&commit).expect("founder processes the restore commit") {
            MlsIncoming::Commit { .. } => {}
            other => panic!("expected a commit, got {other:?}"),
        }
        // the rejoiner joins from the welcome
        let mut bob2 = bob2;
        bob2.join_from_welcome(&welcome2).expect("bob2 joins");

        // the re-keyed seat is live in both directions
        let ct = bob2.encrypt(b"back from the dead").expect("enc");
        assert_app(founder.decrypt(&ct).expect("dec"), "bob", b"back from the dead");
        assert_app(cara.decrypt(&ct).expect("dec"), "bob", b"back from the dead");
        let ct = founder.encrypt(b"welcome back bob").expect("enc");
        assert_app(bob2.decrypt(&ct).expect("dec"), "founder", b"welcome back bob");

        // the OLD bob leaf was removed — the new-epoch message shows up only as
        // an opaque future-epoch header (no plaintext, no authentication), and
        // even fed the very commit that removed it, the stale leaf never
        // reaches an epoch that would decrypt it: the lock-out holds
        match bob.decrypt(&ct) {
            Ok(MlsIncoming::FutureEpoch) => {}
            other => panic!("expected an opaque future-epoch hold, got {other:?}"),
        }
        let _ = bob.decrypt(&commit); // removal commit: unprocessable or self-removing
        assert!(
            !matches!(bob.decrypt(&ct), Ok(MlsIncoming::Application { .. })),
            "the removed leaf is locked out of the new epoch"
        );
    }

    /// **Cross-epoch delivery, forward direction.** A message encrypted at an
    /// epoch the receiver has NOT reached yet (its re-key commit is still in
    /// flight) classifies as [`MlsIncoming::FutureEpoch`] — the transport
    /// buffers it and retries after the commit merges instead of dropping it —
    /// and the SAME ciphertext decrypts normally once the epoch caught up.
    #[test]
    fn a_future_epoch_message_classifies_for_retry_and_decrypts_after_the_commit() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        let cara = MlsMember::new(&key(3), "cara").expect("cara");
        founder.create_group().expect("create");
        let welcome = founder
            .add_members(&[
                bob.key_package().expect("bob kp"),
                cara.key_package().expect("cara kp"),
            ])
            .expect("add")
            .expect("welcome");
        let mut bob = bob;
        let mut cara = cara;
        bob.join_from_welcome(&welcome).expect("bob joins");
        cara.join_from_welcome(&welcome).expect("cara joins");

        // bob's seat is re-keyed by cara → cara is at N+1, the founder still at N
        let bob2 = MlsMember::new(&key(2), "bob").expect("bob2");
        let (commit, _welcome2) =
            cara.restore_member("bob", &bob2.key_package().expect("kp"), NO_CARRIER_STAMP).expect("restore");
        let ct = cara.encrypt(b"raced ahead of the commit").expect("enc");

        // ❶ ahead of the commit: classified for retry, not dropped as an error
        match founder.decrypt(&ct) {
            Ok(MlsIncoming::FutureEpoch) => {}
            other => panic!("expected FutureEpoch, got {other:?}"),
        }
        // ❷ the commit lands …
        match founder.decrypt(&commit).expect("merge the commit") {
            MlsIncoming::Commit { .. } => {}
            other => panic!("expected a commit, got {other:?}"),
        }
        // ❸ … and the SAME ciphertext now decrypts, sender authenticated
        assert_app(
            founder.decrypt(&ct).expect("decrypts at the caught-up epoch"),
            "cara",
            b"raced ahead of the commit",
        );
    }

    /// **Security: the EVICTED device cannot keep speaking.** The recovery
    /// re-key exists to evict a possibly-compromised lost device. A message the
    /// removed leaf encrypts at its OLD epoch must be rejected by every
    /// survivor — keeping past-epoch receive keys around (`max_past_epochs`)
    /// would let the stolen device keep speaking as the member, authenticated,
    /// for an unbounded wall-clock window (epochs only advance on re-keys).
    #[test]
    fn the_evicted_leaf_cannot_speak_after_the_rekey() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        let cara = MlsMember::new(&key(3), "cara").expect("cara");
        founder.create_group().expect("create");
        let welcome = founder
            .add_members(&[
                bob.key_package().expect("bob kp"),
                cara.key_package().expect("cara kp"),
            ])
            .expect("add")
            .expect("welcome");
        let mut bob = bob;
        let mut cara = cara;
        bob.join_from_welcome(&welcome).expect("bob joins");
        cara.join_from_welcome(&welcome).expect("cara joins");

        // bob's (compromised) device is evicted by the recovery re-key
        let bob2 = MlsMember::new(&key(2), "bob").expect("bob2");
        let (commit, _welcome2) =
            cara.restore_member("bob", &bob2.key_package().expect("kp"), NO_CARRIER_STAMP).expect("restore");
        match founder.decrypt(&commit).expect("merge") {
            MlsIncoming::Commit { .. } => {}
            other => panic!("expected a commit, got {other:?}"),
        }

        // the stolen device keeps encrypting at its old epoch — every survivor
        // (the re-keyer AND a member that merged the broadcast commit) must
        // reject it, never attribute it to the member
        let stolen = bob.encrypt(b"i was evicted but still talk").expect("enc");
        assert!(
            !matches!(founder.decrypt(&stolen), Ok(MlsIncoming::Application { .. })),
            "a survivor that merged the re-key must reject the evicted leaf's sends"
        );
        let stolen = bob.encrypt(b"second try").expect("enc");
        assert!(
            !matches!(cara.decrypt(&stolen), Ok(MlsIncoming::Application { .. })),
            "the re-keying coordinator must reject the evicted leaf's sends"
        );
    }

    /// **The prior slot survives a snapshot round trip (M3).** A node that
    /// restarted between two concurrent same-epoch commits must decide the
    /// tiebreak exactly like one that did not — else survivors fork.
    #[test]
    fn the_prior_slot_survives_snapshot_and_restore() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        let cara = MlsMember::new(&key(3), "cara").expect("cara");
        founder.create_group().expect("create");
        let welcome = founder
            .add_members(&[
                bob.key_package().expect("bob kp"),
                cara.key_package().expect("cara kp"),
            ])
            .expect("add")
            .expect("welcome");
        let mut cara = cara;
        cara.join_from_welcome(&welcome).expect("cara joins");
        let bob2 = MlsMember::new(&key(2), "bob").expect("bob2");
        let (_commit, _) = cara
            .restore_member("bob", &bob2.key_package().expect("kp"), 100)
            .expect("restore");
        assert!(cara.prior.is_some(), "the committer armed its slot");
        let blob = cara.snapshot().expect("snapshot");
        let restored = MlsMember::restore(&blob).expect("restore");
        assert_eq!(
            restored.prior.as_ref().map(|p| (p.0, p.2, p.3.clone())),
            cara.prior.as_ref().map(|p| (p.0, p.2, p.3.clone())),
            "epoch, key and removed leaves ride the snapshot"
        );
        // and a pre-v4 blob still restores, without a slot
        let v3 = MlsSnapshotV3 {
            version: 3,
            name: "cara".into(),
            signer_pub: cara.signer.public().to_vec(),
            group_id: cara.group.as_ref().expect("group").group_id().as_slice().to_vec(),
            storage: bincode::deserialize::<MlsSnapshot>(&blob).expect("v4").storage,
            exporter_ring: cara.exporter_ring.clone(),
            exporter_ring_epochs: cara.exporter_ring_epochs.clone(),
        };
        let old = MlsMember::restore(&bincode::serialize(&v3).expect("v3 bytes")).expect("v3");
        assert!(old.prior.is_none());
    }

    /// **A leaf added under a key that is not the member's anchored identity
    /// key is refused before the merge.** Any current leaf could otherwise
    /// re-key any seat under a key it holds and speak as that member
    /// (review 2026-08-25, HIGH). A re-key under the genuine key merges.
    #[test]
    fn an_added_leaf_must_carry_the_anchored_identity_key() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        let cara = MlsMember::new(&key(3), "cara").expect("cara");
        founder.create_group().expect("create");
        let welcome = founder
            .add_members(&[
                bob.key_package().expect("bob kp"),
                cara.key_package().expect("cara kp"),
            ])
            .expect("add")
            .expect("welcome");
        let mut cara = cara;
        cara.join_from_welcome(&welcome).expect("cara joins");
        let roster: BTreeMap<String, Vec<u8>> = [(1u8, "founder"), (2, "bob"), (3, "cara")]
            .into_iter()
            .map(|(k, n)| (n.to_string(), key(k).verifying_key().to_bytes().to_vec()))
            .collect();
        founder.set_roster_keys(roster);

        // the genuine re-key (bob's new device, same identity) merges
        let bob2 = MlsMember::new(&key(2), "bob").expect("bob2");
        let (commit, _) = cara
            .restore_member("bob", &bob2.key_package().expect("kp"), NO_CARRIER_STAMP)
            .expect("restore");
        assert!(matches!(founder.decrypt(&commit), Ok(MlsIncoming::Commit { .. })));
        let epoch = founder.epoch();

        // cara re-keys bob's seat under HER OWN fresh key: refused, epoch untouched
        let mallory = MlsMember::new(&key(9), "bob").expect("mallory as bob");
        let (forged, _) = cara
            .restore_member("bob", &mallory.key_package().expect("kp"), NO_CARRIER_STAMP)
            .expect("cara can build it");
        assert!(
            founder.decrypt(&forged).is_err(),
            "a leaf for bob under a foreign key is refused"
        );
        assert_eq!(founder.epoch(), epoch, "nothing merged");
    }

    /// **An evicted leaf cannot undo its eviction with a back-dated commit.**
    /// The prior slot stays armed until the next epoch change and the
    /// tiebreak stamp is publisher-chosen: the evicted device published a
    /// same-epoch commit stamped one second before the re-key and every
    /// survivor rewound onto it (review 2026-08-25, HIGH).
    #[test]
    fn an_evicted_leaf_cannot_undo_its_eviction_with_a_back_dated_commit() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        let cara = MlsMember::new(&key(3), "cara").expect("cara");
        founder.create_group().expect("create");
        let welcome = founder
            .add_members(&[
                bob.key_package().expect("bob kp"),
                cara.key_package().expect("cara kp"),
            ])
            .expect("add")
            .expect("welcome");
        let mut bob = bob;
        let mut cara = cara;
        bob.join_from_welcome(&welcome).expect("bob joins");
        cara.join_from_welcome(&welcome).expect("cara joins");

        // the re-key evicts bob's old device at stamp 100
        let bob2 = MlsMember::new(&key(2), "bob").expect("bob2");
        let (rekey, _) = cara
            .restore_member("bob", &bob2.key_package().expect("kp"), 100)
            .expect("restore");
        assert!(matches!(founder.decrypt_at(&rekey, 100), Ok(MlsIncoming::Commit { .. })));
        let epoch = founder.epoch();

        // the evicted device, still at the old epoch, commits with a LOWER
        // stamp — the tiebreak would hand it the epoch
        let cara2 = MlsMember::new(&key(3), "cara").expect("cara2");
        let (undo, _) = bob
            .restore_member("cara", &cara2.key_package().expect("kp"), 99)
            .expect("the old device can still build a commit");
        let outcome = founder.decrypt_at(&undo, 99);
        assert!(
            !matches!(outcome, Ok(MlsIncoming::CommitRewound { .. })),
            "a removed leaf must not win the epoch: {outcome:?}"
        );
        assert_eq!(founder.epoch(), epoch, "the re-key stands");
        let stolen = bob.encrypt(b"still here?").expect("enc");
        assert!(!matches!(founder.decrypt(&stolen), Ok(MlsIncoming::Application { .. })));
    }

    /// **Cross-epoch delivery, backward direction — deliberately NOT
    /// supported.** A message encrypted at the PREVIOUS epoch that arrives
    /// after the receiver already merged the re-key commit is rejected:
    /// keeping past-epoch receive keys (`max_past_epochs`) would equally let a
    /// just-evicted device keep speaking as the member (see
    /// [`the_evicted_leaf_cannot_speak_after_the_rekey`]), so forward secrecy
    /// wins. A delayed pre-re-key message is dropped — chat is ephemeral and
    /// chain blocks have catch-up.
    #[test]
    fn an_old_epoch_message_is_rejected_after_a_rekey() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        let cara = MlsMember::new(&key(3), "cara").expect("cara");
        founder.create_group().expect("create");
        let welcome = founder
            .add_members(&[
                bob.key_package().expect("bob kp"),
                cara.key_package().expect("cara kp"),
            ])
            .expect("add")
            .expect("welcome");
        let mut bob = bob;
        let mut cara = cara;
        bob.join_from_welcome(&welcome).expect("bob joins");
        cara.join_from_welcome(&welcome).expect("cara joins");

        // cara re-keys bob's seat and is at N+1; the founder, still at N,
        // has a chat in flight that was encrypted before the commit
        let bob2 = MlsMember::new(&key(2), "bob").expect("bob2");
        let (_commit, _welcome2) =
            cara.restore_member("bob", &bob2.key_package().expect("kp"), NO_CARRIER_STAMP).expect("restore");
        let delayed = founder.encrypt(b"sent before the re-key").expect("enc");

        // the delayed epoch-N message arriving AFTER the re-key is rejected
        assert!(
            !matches!(cara.decrypt(&delayed), Ok(MlsIncoming::Application { .. })),
            "past-epoch messages must not decrypt after a re-key"
        );
    }

    /// A member snapshotted after founding rehydrates and still decrypts a
    /// message minted after the snapshot — proving the ratchet state (not just
    /// a static key) round-tripped through `transport.state`.
    #[test]
    fn snapshot_restore_survives_and_still_decrypts() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        founder.create_group().expect("create group");
        let welcome = founder
            .add_members(&[bob.key_package().expect("bob kp")])
            .expect("add")
            .expect("a welcome");
        let mut bob = bob;
        bob.join_from_welcome(&welcome).expect("bob joins");

        let blob = bob.snapshot().expect("snapshot");
        drop(bob);
        let mut bob = MlsMember::restore(&blob).expect("restore");

        let ct = founder.encrypt(b"after restart").expect("encrypt");
        assert_app(bob.decrypt(&ct).expect("decrypt"), "founder", b"after restart");

        // and the restored member can still send (signer round-tripped)
        let ct = bob.encrypt(b"back online").expect("encrypt");
        assert_app(founder.decrypt(&ct).expect("decrypt"), "bob", b"back online");
    }

    /// The reopen guarantee: a snapshot taken **mid-session** (after the ratchet
    /// has advanced through real traffic) restores to the SAME advanced state, so
    /// chat resumes without replay-rejection — this is what lets a reopened
    /// workspace continue its mesh.
    #[test]
    fn snapshot_mid_session_resumes_the_advanced_ratchet() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        founder.create_group().expect("create group");
        let welcome = founder
            .add_members(&[bob.key_package().expect("bob kp")])
            .expect("add")
            .expect("a welcome");
        let mut bob = bob;
        bob.join_from_welcome(&welcome).expect("bob joins");

        // advance BOTH ratchets through a few rounds of live traffic
        for i in 0..3u8 {
            let ct = founder.encrypt(&[b'f', i]).expect("f enc");
            assert_app(bob.decrypt(&ct).expect("b dec"), "founder", &[b'f', i]);
            let ct = bob.encrypt(&[b'b', i]).expect("b enc");
            assert_app(founder.decrypt(&ct).expect("f dec"), "bob", &[b'b', i]);
        }

        // snapshot mid-session (as a clean close does), drop, restore both
        let f_blob = founder.snapshot().expect("f snapshot");
        let b_blob = bob.snapshot().expect("b snapshot");
        drop(founder);
        drop(bob);
        let mut founder = MlsMember::restore(&f_blob).expect("restore founder");
        let mut bob = MlsMember::restore(&b_blob).expect("restore bob");

        // resume: the next message uses the NEXT generation, accepted (not a replay)
        let ct = founder.encrypt(b"resumed after reopen").expect("enc");
        assert_app(bob.decrypt(&ct).expect("dec"), "founder", b"resumed after reopen");
        let ct = bob.encrypt(b"still here").expect("enc");
        assert_app(founder.decrypt(&ct).expect("dec"), "bob", b"still here");
    }

    /// A forged/garbage application message is rejected, never panics.
    #[test]
    fn garbage_ciphertext_is_rejected() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        founder.create_group().expect("create group");
        assert!(founder.decrypt(b"not an mls message").is_err());
        assert!(founder.decrypt(&[]).is_err());
    }

    /// A solo founder (n = 1) has a group but no one to add.
    #[test]
    fn solo_founder_creates_a_group_with_no_welcome() {
        let mut founder = MlsMember::new(&key(1), "solo").expect("founder");
        founder.create_group().expect("create group");
        assert!(founder.has_group());
        assert_eq!(founder.add_members(&[]).expect("add none"), None);
    }

    /// A KeyPackage's binding is exactly the (handle, signature key) it was
    /// built with — so the founder can hold a joiner to the identity it
    /// anchored, and a mismatch (different handle or key) is detectable.
    #[test]
    fn key_package_binding_exposes_the_committed_identity() {
        let sk = key(7);
        let member = MlsMember::new(&sk, "bob").expect("member");
        let (id, sig) = key_package_binding(&member.key_package().expect("kp")).expect("binding");
        assert_eq!(id, b"bob");
        assert_eq!(hex::encode(&sig), hex::encode(sk.verifying_key().to_bytes()));
        // a different member commits to a different handle
        let eve = MlsMember::new(&key(8), "eve").expect("m");
        let (id2, _) = key_package_binding(&eve.key_package().expect("kp")).expect("binding");
        assert_eq!(id2, b"eve");
        // garbage is rejected, never panics
        assert!(key_package_binding(&[0xde, 0xad]).is_err());
    }

    /// A garbage KeyPackage (well-formed bytes, but not an MLS KeyPackage) is
    /// rejected before it touches the group — the founder cannot be tricked into
    /// KEYSTONE (N3 §1, `mdk_evaluation.md` §2.4) — CONCURRENT COMMITS MUST
    /// NOT FORK THE GROUP. Two members that each commit at the SAME epoch
    /// without having seen the other (two recoveries running at once) end up
    /// in different key schedules; if each merely merges its own and drops
    /// the other's, the two states diverge **permanently and silently** —
    /// same epoch number, different secrets, no error anywhere. Both nodes
    /// must converge on the SAME commit and still be able to talk.
    #[test]
    fn concurrent_same_epoch_commits_converge_instead_of_forking() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        founder.create_group().expect("create group");
        let mut alice = MlsMember::new(&key(2), "alice").expect("alice");
        let mut bob = MlsMember::new(&key(3), "bob").expect("bob");
        let carol = MlsMember::new(&key(4), "carol").expect("carol");
        let dave = MlsMember::new(&key(5), "dave").expect("dave");
        let welcome = founder
            .add_members(&[
                alice.key_package().expect("kp"),
                bob.key_package().expect("kp"),
                carol.key_package().expect("kp"),
                dave.key_package().expect("kp"),
            ])
            .expect("add")
            .expect("welcome");
        alice.join_from_welcome(&welcome).expect("alice joins");
        bob.join_from_welcome(&welcome).expect("bob joins");

        // carol and dave return on fresh devices at the same time: alice
        // coordinates carol's re-key, bob coordinates dave's — neither has
        // seen the other's commit, so both are built on the SAME epoch
        let carol_again = MlsMember::new(&key(4), "carol").expect("carol again");
        let dave_again = MlsMember::new(&key(5), "dave").expect("dave again");
        let epoch_before = alice.epoch();
        assert_eq!(bob.epoch(), epoch_before, "both start from the same epoch");
        // the stamps are the CARRIER EVENTS' created_at — the same values
        // every node sees, which is what makes the tiebreak agree everywhere
        let (stamp_a, stamp_b) = (1_760_000_000, 1_760_000_001);
        let (commit_a, _w) = alice
            .restore_member("carol", &carol_again.key_package().expect("kp"), stamp_a)
            .expect("alice commits carol's re-key");
        let (commit_b, _w) = bob
            .restore_member("dave", &dave_again.key_package().expect("kp"), stamp_b)
            .expect("bob commits dave's re-key");

        // …and the commits cross on the wire
        let to_alice = alice
            .decrypt_at(&commit_b, stamp_b)
            .expect("alice takes bob's commit");
        let to_bob = bob
            .decrypt_at(&commit_a, stamp_a)
            .expect("bob takes alice's commit");
        // alice's stamp is lower, so HER commit is the winner everywhere
        assert!(
            matches!(to_alice, MlsIncoming::CommitSuperseded),
            "alice keeps her own winning commit: {to_alice:?}"
        );
        assert!(
            matches!(to_bob, MlsIncoming::CommitRewound { .. }),
            "bob rewinds onto alice's — and is TOLD his own was rolled back: {to_bob:?}"
        );
        assert!(
            matches!(
                to_alice,
                MlsIncoming::Commit { .. }
                    | MlsIncoming::CommitSuperseded
                    | MlsIncoming::CommitRewound { .. }
            ),
            "a same-epoch commit is a known outcome, not an error: {to_alice:?}"
        );

        assert_eq!(alice.epoch(), bob.epoch(), "the epochs must not diverge");
        // the real divergence detector: same epoch NUMBER means nothing if
        // the key schedules differ — a message must still cross
        let wire = alice.encrypt(b"are we still one group?").expect("alice encrypts");
        match bob.decrypt(&wire).expect("bob decrypts") {
            MlsIncoming::Application { from, plaintext } => {
                assert_eq!(from, "alice");
                assert_eq!(plaintext, b"are we still one group?");
            }
            other => panic!("the group forked — bob cannot read alice: {other:?}"),
        }
    }

    /// The rewind is TRANSACTIONAL and the tiebreak is commit-only. The
    /// content type rides in cleartext framing, so anything can CLAIM to be
    /// a same-epoch commit; if such a frame could roll a node back, an
    /// attacker would hold a state-reset button (and a rewound node can read
    /// traffic its own commit had already sealed off). Garbage that claims
    /// the race must leave the state exactly as it was.
    #[test]
    fn a_forged_same_epoch_commit_cannot_roll_the_state_back() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        founder.create_group().expect("create group");
        let mut alice = MlsMember::new(&key(2), "alice").expect("alice");
        let carol = MlsMember::new(&key(4), "carol").expect("carol");
        let welcome = founder
            .add_members(&[
                alice.key_package().expect("kp"),
                carol.key_package().expect("kp"),
            ])
            .expect("add")
            .expect("welcome");
        alice.join_from_welcome(&welcome).expect("alice joins");

        let carol_again = MlsMember::new(&key(4), "carol").expect("carol again");
        let (_commit, _w) = alice
            .restore_member("carol", &carol_again.key_package().expect("kp"), 5_000)
            .expect("alice commits");
        let after = alice.epoch();

        // a frame stamped BEFORE alice's commit (so it would win the
        // tiebreak) but which is not a processable commit at all
        for junk in [vec![0u8; 0], vec![0xde, 0xad, 0xbe, 0xef], vec![0x01; 64]] {
            let _ = alice.decrypt_at(&junk, 1);
            assert_eq!(alice.epoch(), after, "the state must not move");
        }
        // …and the real race still works afterwards: the slot is re-armed,
        // so a genuine lower-keyed commit is still honoured
        assert!(alice.encrypt(b"still me").is_ok(), "the group still works");
    }

    /// KEYSTONE (N3 §2, concept §6 finding 1) — THE ASYMMETRY: the bounded
    /// ring of past exporter secrets lets a laggard STRIP THE OUTER layer of
    /// an event published before a re-key, while the INNER MLS layer still
    /// REJECTS an evicted leaf's old-epoch message. Keeping outer secrets is
    /// therefore not a re-opening of the eviction hole — the two secrets do
    /// different jobs, and this test is what says so.
    #[test]
    fn the_exporter_ring_strips_the_outer_layer_but_mls_still_rejects_the_old_epoch() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        founder.create_group().expect("create group");
        let mut alice = MlsMember::new(&key(2), "alice").expect("alice");
        let mut evicted = MlsMember::new(&key(3), "evicted").expect("evicted");
        let welcome = founder
            .add_members(&[
                alice.key_package().expect("kp"),
                evicted.key_package().expect("kp"),
            ])
            .expect("add")
            .expect("welcome");
        alice.join_from_welcome(&welcome).expect("alice joins");
        evicted.join_from_welcome(&welcome).expect("evicted joins");

        // the epoch's outer secret, and a message sealed under it
        let old_secret = alice.exporter_secret().expect("exporter secret");
        assert_eq!(founder.exporter_secret().expect("same group"), old_secret);
        let old_epoch_wire = evicted.encrypt(b"from the old epoch").expect("encrypt");

        // …then the seat is re-keyed: the epoch (and the exporter) advance
        let back = MlsMember::new(&key(3), "evicted").expect("returning");
        let (commit, _w) = founder
            .restore_member("evicted", &back.key_package().expect("kp"), 9_000)
            .expect("re-key");
        alice.decrypt_at(&commit, 9_000).expect("alice merges the re-key");
        let new_secret = alice.exporter_secret().expect("new exporter secret");
        assert_ne!(new_secret, old_secret, "the exporter rotates with the epoch");

        // OUTER: the ring still holds the previous secret, so a pre-commit
        // event is not an opaque blob — catch-up across one re-key works
        assert!(
            alice.exporter_ring().contains(&old_secret),
            "the last K exporter secrets stay available for the outer layer"
        );
        // INNER: …and the evicted leaf's old-epoch message is STILL rejected
        assert!(
            alice.decrypt_at(&old_epoch_wire, 8_000).is_err(),
            "max_past_epochs = 0 must still refuse an old-epoch message"
        );

        // the ring is bounded at K: after K further re-keys the oldest is gone
        let mut member = alice;
        for round in 0..EXPORTER_RING_K {
            let again = MlsMember::new(&key(3), "evicted").expect("returning");
            let (c, _w) = founder
                .restore_member(
                    "evicted",
                    &again.key_package().expect("kp"),
                    10_000 + u64::try_from(round).unwrap_or(0),
                )
                .expect("re-key");
            member
                .decrypt_at(&c, 10_000 + u64::try_from(round).unwrap_or(0))
                .expect("merge");
        }
        assert!(
            !member.exporter_ring().contains(&old_secret),
            "beyond the ring an old epoch is opaque — reported loudly, never silently skipped"
        );
        assert!(member.exporter_ring().len() <= EXPORTER_RING_K);
    }

    /// KEYSTONE (N3 §5) — the `ChainOracle` seam is a HARD gate, and it is
    /// consulted BEFORE the merge: an unauthorized group-data change is
    /// dropped with the epoch untouched. Merging first and rejecting after
    /// would advance this node past a change the republic never decided,
    /// while every node that refused it stayed behind — the permanent
    /// epoch split concept §5 exists to prevent.
    #[test]
    fn an_unauthorized_group_data_commit_is_dropped_before_the_merge() {
        struct Oracle {
            allowed: &'static str,
        }
        impl ChainOracle for Oracle {
            fn authorizes(&self, block_hash: &str) -> bool {
                block_hash == self.allowed
            }
            fn head(&self) -> Option<(u64, String)> {
                Some((7, self.allowed.to_string()))
            }
        }
        let oracle = Oracle { allowed: "abc123" };
        assert!(oracle.authorizes("abc123"), "the decided block authorizes");
        assert!(!oracle.authorizes("beef99"), "any other hash does not");
        assert!(!oracle.authorizes(""), "and neither does an absent binding");
        assert_eq!(oracle.head().map(|(h, _)| h), Some(7));

        // the gate itself: a group-data commit is applied only when its
        // block binding is authorized, and a refusal leaves the epoch alone
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        founder.create_group().expect("create group");
        let before = founder.epoch();
        assert_eq!(
            founder.authorize_group_data(&oracle, Some("beef99")),
            Err(GroupDataRefused::NotAuthorized),
            "an unauthorized binding is refused"
        );
        assert_eq!(
            founder.authorize_group_data(&oracle, None),
            Err(GroupDataRefused::Unbound),
            "…as is a group-data commit with no binding at all"
        );
        assert_eq!(founder.epoch(), before, "a refusal never moves the epoch");
        assert_eq!(founder.authorize_group_data(&oracle, Some("abc123")), Ok(()));
    }

    /// KEYSTONE (review finding 2026-07-31, CRITICAL) — convergence through
    /// the PRODUCTION entry points, and for a BYSTANDER too.
    ///
    /// The first version of this mechanism passed its keystone while being
    /// broken in the wired path: the test drove explicit-stamp variants that
    /// nothing outside tests called, while production stamped its own commit
    /// from a local clock and every foreign commit with 0 — so the local
    /// commit ALWAYS lost, both racers rewound onto each other's branch, and
    /// each silently reverted the eviction it had just performed. This test
    /// uses exactly what the engine uses.
    #[test]
    fn concurrent_commits_converge_through_the_production_entry_points() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        founder.create_group().expect("create group");
        let mut alice = MlsMember::new(&key(2), "alice").expect("alice");
        let mut bob = MlsMember::new(&key(3), "bob").expect("bob");
        // …and a BYSTANDER that authors no commit at all: the majority of a
        // real republic. It must land on the same branch as the committers.
        let mut chris = MlsMember::new(&key(6), "chris").expect("chris");
        // a second bystander, to see the commits in the OTHER order, and a
        // third that accepts traffic BETWEEN the two commits
        let mut dana = MlsMember::new(&key(7), "dana").expect("dana");
        let mut erik = MlsMember::new(&key(8), "erik").expect("erik");
        let carol = MlsMember::new(&key(4), "carol").expect("carol");
        let dave = MlsMember::new(&key(5), "dave").expect("dave");
        let welcome = founder
            .add_members(&[
                alice.key_package().expect("kp"),
                bob.key_package().expect("kp"),
                chris.key_package().expect("kp"),
                dana.key_package().expect("kp"),
                erik.key_package().expect("kp"),
                carol.key_package().expect("kp"),
                dave.key_package().expect("kp"),
            ])
            .expect("add")
            .expect("welcome");
        for m in [&mut alice, &mut bob, &mut chris, &mut dana, &mut erik] {
            m.join_from_welcome(&welcome).expect("join");
        }

        let carol_again = MlsMember::new(&key(4), "carol").expect("carol again");
        let dave_again = MlsMember::new(&key(5), "dave").expect("dave again");
        // the ENGINE's call shape — one stamp source on both sides
        let (commit_a, _w) = alice
            .restore_member("carol", &carol_again.key_package().expect("kp"), NO_CARRIER_STAMP)
            .expect("alice commits");
        let (commit_b, _w) = bob
            .restore_member("dave", &dave_again.key_package().expect("kp"), NO_CARRIER_STAMP)
            .expect("bob commits");

        // A bystander that accepts TRAFFIC BETWEEN the two commits must still
        // be able to take the winner afterwards. This runs FIRST, while bob
        // is still on his own branch — a "close the window on traffic" rule
        // stranded exactly this node (review finding 2026-07-31).
        erik.decrypt(&commit_b).expect("erik merges bob's first");
        let bob_talk = bob.encrypt(b"between the commits").expect("bob talks");
        assert!(
            matches!(erik.decrypt(&bob_talk), Ok(MlsIncoming::Application { .. })),
            "erik reads bob's branch, which he just joined"
        );
        erik.decrypt(&commit_a).expect("…and can still take the winner");

        // …and the ENGINE's receive shape (`decrypt`, not `decrypt_at`).
        // The bystanders see the commits in opposite orders: whichever one
        // is merged first, the other must still be able to take it there.
        alice.decrypt(&commit_b).expect("alice takes bob's");
        bob.decrypt(&commit_a).expect("bob takes alice's");
        chris.decrypt(&commit_a).expect("chris takes alice's first");
        chris.decrypt(&commit_b).expect("…then bob's");

        assert_eq!(alice.epoch(), bob.epoch(), "the committers agree on the epoch");
        assert_eq!(chris.epoch(), alice.epoch(), "…and so does the bystander");
        // the real detector: one key schedule, or nothing crosses
        let wire = alice.encrypt(b"one group?").expect("alice encrypts");
        for (who, m) in [("bob", &mut bob), ("chris", &mut chris), ("erik", &mut erik)] {
            match m.decrypt(&wire).expect("decrypt") {
                MlsIncoming::Application { from, plaintext } => {
                    assert_eq!(from, "alice");
                    assert_eq!(plaintext, b"one group?");
                }
                other => panic!("{who} forked away from alice: {other:?}"),
            }
        }
        // …and the bystander that saw them in the OTHER order lands there too
        dana.decrypt(&commit_b).expect("dana takes bob's first");
        dana.decrypt(&commit_a).expect("…then alice's");
        assert_eq!(dana.epoch(), alice.epoch(), "order of arrival must not matter");
    }

    /// KEYSTONE (review findings 2026-07-31) — the race window CLOSES, and
    /// the loser is TOLD.
    ///
    /// Without an expiry the prior-state slot held a full pre-eviction key
    /// schedule forever, so replaying a commit the relay still holds could
    /// roll a node back at any later time. And a coordinator whose re-key
    /// lost the tiebreak saw an ordinary `Commit` — while the member it
    /// re-keyed already held a Welcome for a branch nobody is on.
    #[test]
    fn the_race_window_closes_and_the_rewound_committer_is_told() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        founder.create_group().expect("create group");
        let mut alice = MlsMember::new(&key(2), "alice").expect("alice");
        let mut bob = MlsMember::new(&key(3), "bob").expect("bob");
        let carol = MlsMember::new(&key(4), "carol").expect("carol");
        let dave = MlsMember::new(&key(5), "dave").expect("dave");
        let welcome = founder
            .add_members(&[
                alice.key_package().expect("kp"),
                bob.key_package().expect("kp"),
                carol.key_package().expect("kp"),
                dave.key_package().expect("kp"),
            ])
            .expect("add")
            .expect("welcome");
        alice.join_from_welcome(&welcome).expect("alice joins");
        bob.join_from_welcome(&welcome).expect("bob joins");

        let carol_again = MlsMember::new(&key(4), "carol").expect("carol again");
        let dave_again = MlsMember::new(&key(5), "dave").expect("dave again");
        let (commit_a, _w) = alice
            .restore_member("carol", &carol_again.key_package().expect("kp"), 100)
            .expect("alice commits");
        let (commit_b, _w) = bob
            .restore_member("dave", &dave_again.key_package().expect("kp"), 200)
            .expect("bob commits");

        // bob's stamp is higher, so bob LOSES and must hear about it: the
        // dave re-key he already Welcomed is gone
        assert!(
            matches!(bob.decrypt_at(&commit_a, 100), Ok(MlsIncoming::CommitRewound { .. })),
            "a rewound committer must not see an ordinary merge"
        );
        assert!(matches!(
            alice.decrypt_at(&commit_b, 200),
            Ok(MlsIncoming::CommitSuperseded)
        ));

        // REPLAY IS A NO-OP: an equal key is "superseded", never a rewind —
        // so re-sending bytes the relay still holds cannot roll a node back.
        // The detector is not the epoch NUMBER (a rewind returns to the same
        // number on a different key schedule) but whether alice's branch
        // still reaches bob.
        for stamp in [1u64, 50, 100, 200, 999] {
            let _ = bob.decrypt_at(&commit_a, stamp);
            let _ = bob.decrypt_at(&commit_b, stamp);
            let probe = alice.encrypt(b"still one group").expect("encrypt");
            assert!(
                matches!(bob.decrypt_at(&probe, 400), Ok(MlsIncoming::Application { .. })),
                "replay at stamp {stamp} knocked bob off alice's branch"
            );
        }
    }

    /// building the group around a bogus member — and a real add still works
    /// afterwards (the group state was not left half-mutated).
    #[test]
    fn garbage_key_package_is_rejected() {
        let mut founder = MlsMember::new(&key(1), "founder").expect("founder");
        founder.create_group().expect("create group");
        assert!(founder.add_members(&[vec![0xde, 0xad, 0xbe, 0xef]]).is_err());
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        assert!(founder
            .add_members(&[bob.key_package().expect("kp")])
            .is_ok());
    }
}
