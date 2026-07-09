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
use std::collections::HashMap;
use tls_codec::{Deserialize as _, Serialize as _};

/// The one ciphersuite MoltRepublic speaks (RFC 9420 mandatory-to-implement).
const SUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// The snapshot schema version (bumped on any incompatible blob layout).
const SNAPSHOT_VERSION: u8 = 1;

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
    /// The MLS-core milestone has no post-founding commits, but decrypt stays
    /// robust so a future recovery rejoin cannot wedge the receiver.
    Commit,
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

/// One node's live MLS membership: the provider (holding all secret state), the
/// signer built from the node's identity key, the node's own handle, and — once
/// created or joined — the group.
pub struct MlsMember {
    provider: OpenMlsRustCrypto,
    signer: SignatureKeyPair,
    name: String,
    group: Option<MlsGroup>,
}

/// The self-contained persistence blob: the provider's whole key-value map plus
/// the handles needed to rehydrate the signer and group. bincode (not JSON):
/// the storage keys are `Vec<u8>`, which JSON object keys cannot represent.
#[derive(Serialize, Deserialize)]
struct MlsSnapshot {
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
            // (chat is ephemeral; chain blocks have catch-up). Forward-racing
            // messages are covered separately by the FutureEpoch retry.
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
    pub fn restore_member(
        &mut self,
        member: &str,
        new_key_package: &[u8],
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
        group
            .merge_pending_commit(&self.provider)
            .map_err(|e| MlsError::Mls(format!("merging restore commit: {e:?}")))?;
        let commit_bytes = commit
            .to_bytes()
            .map_err(|e| MlsError::Wire(format!("serializing restore commit: {e}")))?;
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
        // eviction property of a recovery re-key outranks delayed delivery
        let config = MlsGroupJoinConfig::builder().build();
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
        let processed = group
            .process_message(&self.provider, protocol)
            .map_err(|e| MlsError::Wire(format!("processing message: {e:?}")))?;
        let from = String::from_utf8_lossy(processed.credential().serialized_content()).into_owned();
        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => Ok(MlsIncoming::Application {
                from,
                plaintext: app.into_bytes(),
            }),
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                group
                    .merge_staged_commit(&self.provider, *staged)
                    .map_err(|e| MlsError::Mls(format!("merging commit: {e:?}")))?;
                Ok(MlsIncoming::Commit)
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
        };
        bincode::serialize(&snap).map_err(|e| MlsError::Snapshot(format!("encoding snapshot: {e}")))
    }

    /// Rehydrate a member from a [`snapshot`](MlsMember::snapshot) blob. Fully
    /// self-contained: the signer's private half round-trips inside the provider
    /// storage, so no external key is needed.
    pub fn restore(blob: &[u8]) -> Result<MlsMember, MlsError> {
        let snap: MlsSnapshot = bincode::deserialize(blob)
            .map_err(|e| MlsError::Snapshot(format!("decoding snapshot: {e}")))?;
        if snap.version != SNAPSHOT_VERSION {
            return Err(MlsError::Snapshot(format!(
                "unsupported snapshot version {}",
                snap.version
            )));
        }
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
        let (commit, welcome2) = cara.restore_member("bob", &bob2_kp).expect("restore");

        // every OTHER existing member merges the commit to advance the epoch
        match founder.decrypt(&commit).expect("founder processes the restore commit") {
            MlsIncoming::Commit => {}
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
            cara.restore_member("bob", &bob2.key_package().expect("kp")).expect("restore");
        let ct = cara.encrypt(b"raced ahead of the commit").expect("enc");

        // ❶ ahead of the commit: classified for retry, not dropped as an error
        match founder.decrypt(&ct) {
            Ok(MlsIncoming::FutureEpoch) => {}
            other => panic!("expected FutureEpoch, got {other:?}"),
        }
        // ❷ the commit lands …
        match founder.decrypt(&commit).expect("merge the commit") {
            MlsIncoming::Commit => {}
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
            cara.restore_member("bob", &bob2.key_package().expect("kp")).expect("restore");
        match founder.decrypt(&commit).expect("merge") {
            MlsIncoming::Commit => {}
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
            cara.restore_member("bob", &bob2.key_package().expect("kp")).expect("restore");
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
