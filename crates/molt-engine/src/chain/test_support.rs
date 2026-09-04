// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared fixtures of the chain tests: the [`Builder`] that seals real
//! threshold-signed chains from seeded identities, and the peer / signer /
//! wire helpers every sibling test file uses.

use super::*;
use molt_core::{ChainChange, MembershipOp, Surface};
use molt_storage::{derive_identity_key, identity_sign, SigningKey};
use serde_json::json;

/// A minimal chain builder: derives each member's identity key from a seed,
/// seals the genesis with everyone (n-of-n) and appends later blocks signed
/// by a chosen subset — exactly what the real founding + threshold path
/// will produce.
#[derive(Clone)]
pub(super) struct Builder {
    pub(super) republic_id: String,
    pub(super) keys: Vec<(String, SigningKey)>,
    pub(super) blocks: Vec<ChainBlock>,
    pub(super) head_hash: String,
}

impl Builder {
    pub(super) fn new(members: &[&str], rule_m: u8) -> Builder {
        Builder::new_on_relays(members, rule_m, Vec::new())
    }

    /// A founding whose genesis ratifies `relays` (R3b ledger tests).
    pub(super) fn new_on_relays(members: &[&str], rule_m: u8, relays: Vec<String>) -> Builder {
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
    pub(super) fn seal(&self, height: u64, change: ChainChange, signers: &[&str]) -> ChainBlock {
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

    pub(super) fn push(&mut self, block: ChainBlock) {
        self.head_hash = block_hash(&self.republic_id, &block);
        self.blocks.push(block);
    }

    /// Commit a gated Applied change signed by `signers` at the next height.
    pub(super) fn commit_applied(&mut self, proposal_id: u64, signers: &[&str]) {
        let height = u64::try_from(self.blocks.len()).expect("small chain");
        let change = ChainChange::Applied {
            proposal_id,
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "id": proposal_id }),
        };
        let block = self.seal(height, change, signers);
        self.push(block);
    }

    /// Commit a ratified wiki patch (a new document) — the Memory entries
    /// a folded cut collapses into one commitment (K6).
    pub(super) fn commit_wiki(&mut self, proposal_id: u64, path: &str, body: &str, signers: &[&str]) {
        let height = u64::try_from(self.blocks.len()).expect("small chain");
        let lines: Vec<&str> = body.split('\n').collect();
        let mut patch = format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n",
            lines.len()
        );
        for l in lines {
            patch.push('+');
            patch.push_str(l);
            patch.push('\n');
        }
        let change = ChainChange::Applied {
            proposal_id,
            surface: Surface::Memory,
            payload: json!({ "op": "wiki_patch", "value": patch }),
        };
        let block = self.seal(height, change, signers);
        self.push(block);
    }

    /// Commit an Organization edit — the surface whose ops occupy
    /// last-write-wins slots (§B.6a), so a checkpoint summarizes them.
    pub(super) fn commit_org(&mut self, proposal_id: u64, op: &str, value: &str, signers: &[&str]) {
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
    pub(super) fn commit_restored(&mut self, member: &str, nostr_pk: &str, signers: &[&str]) {
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
    pub(super) fn key(&self, member: &str) -> &SigningKey {
        &self
            .keys
            .iter()
            .find(|(m, _)| m == member)
            .expect("known member")
            .1
    }

    /// A member's anchored identity pk (from the genesis roster).
    pub(super) fn pk(&self, member: &str) -> String {
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

/// A 2-member chain-governed peer holding only the genesis `b` roots.
pub(super) fn chain_peer(member: &str, b: &Builder, chain: Vec<ChainBlock>) -> crate::State {
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

/// A 2-of-3 chain peer holding the FULL three-member roster (the shared
/// `chain_peer` pins the founding pair).
pub(super) fn chain_peer_3(member: &str, b: &Builder) -> crate::State {
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
pub(super) fn wire(peer: &mut crate::State, from: &str, seq: u64, body: WorkspaceEvent) {
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

/// Grow a builder chain to exactly `len` blocks (genesis included).
pub(super) fn grown_chain(len: usize) -> Builder {
    let mut b = Builder::new(&["petra", "walter"], 2);
    for id in 0..len.saturating_sub(1) {
        b.commit_applied(u64::try_from(id + 100).expect("small id"), &["petra", "walter"]);
    }
    assert_eq!(b.blocks.len(), len);
    b
}

/// A chain-governed member that can also SIGN (holds its identity key).
pub(super) fn chain_signer(member: &str, b: &Builder, chain: Vec<ChainBlock>) -> crate::State {
    let mut s = chain_peer(member, b, chain);
    s.identity_sk = Some(b.key(member).clone());
    s
}

/// The restored member's consent bytes, signed with its own roster key.
pub(super) fn consent_for(b: &Builder, member: &str, nostr_pk: &str) -> String {
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
