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
//! §6). The **write-ahead** ordering (persist the advanced ratchet before a
//! ciphertext leaves / before an inbound plaintext reaches the engine) is the
//! caller's contract: `encrypt`/`decrypt` mutate, then the caller persists
//! `snapshot()` before releasing the result.

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
}
