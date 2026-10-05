// SPDX-License-Identifier: GPL-3.0-or-later

//! Unit tests of the vault payload plane (plan S3a).

use super::*;
use crate::chain::test_support::{chain_peer, Builder};
use molt_core::vault::{SecretText, VaultDeposit};
use molt_core::ChainChange;

/// A deterministic rand_core 0.6 source for `build_deposit`.
struct TestRng(u64);

impl molt_vault::rand_core::RngCore for TestRng {
    fn next_u32(&mut self) -> u32 {
        u32::try_from(self.next_u64() >> 32).unwrap_or(0)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for b in dest {
            *b = self.next_u32().to_le_bytes()[0];
        }
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), molt_vault::rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl molt_vault::rand_core::CryptoRng for TestRng {}

/// A real deposit by `a` in the 2-of-4 vault founding: `(record, payload)`.
fn deposit(b: &Builder, name: &str, seed: u64) -> (VaultDeposit, Vec<u8>) {
    let ChainChange::Genesis { rule_m, identities, .. } = &b.blocks[0].change else {
        panic!("block 0 is not a genesis");
    };
    let ctx = crate::vault::ctx_from_founding(*rule_m, identities).expect("a vault founding");
    let (entropy, npk, _) = Builder::seat_keys(0);
    let vault_seed = molt_vault::derive_vault_seed(&entropy, &npk, "id");
    let text = SecretText(format!("{name} one two"));
    let input = molt_vault::DepositInput {
        republic_id: &b.republic_id,
        depositor: "a",
        name,
        kind: "text",
        text: &text,
        ctx: &ctx,
    };
    molt_vault::build_deposit(&input, &vault_seed, b.key("a"), &mut TestRng(seed)).expect("deposit")
}

fn op(dep: &VaultDeposit) -> Value {
    serde_json::to_value(VaultOp::Deposit(dep.clone())).expect("op")
}

/// Seat `me` of a vault republic whose chain committed `dep`.
fn seat_with_committed(me: &str) -> (crate::State, VaultDeposit, Vec<u8>) {
    let mut b = Builder::vault();
    let (dep, file) = deposit(&b, "one", 1);
    b.commit(ChainChange::Applied { proposal_id: 1, surface: Surface::Vault, payload: op(&dep) }, &["a", "b"]);
    let st = chain_peer(me, &b, b.blocks.clone());
    assert!(st.is_vault_republic());
    (st, dep, file)
}

fn sid(st: &crate::State, dep: &VaultDeposit) -> String {
    molt_core::vault::secret_id(&st.republic_id(), dep)
}

fn mark_held(st: &mut crate::State, secret_id: &str) {
    st.vault_sync_held();
    st.files.vault.held.insert(secret_id.to_string());
}

/// A pending (not yet committed) deposit proposal on `st`.
fn pending(st: &mut crate::State, id: u64, payload: Value) {
    st.proposals.insert(
        id,
        molt_core::ProposalRecord {
            surface: Surface::Vault,
            payload,
            approvals: 0,
            state: ProposalState::Proposed,
            applied_at: 0,
            declined_at: 0,
            declined_by: String::new(),
            decliners: Vec::new(),
            voted: Vec::new(),
            by: "a".to_string(),
            superseded: false,
            superseded_kind: None,
            withdrawn: false,
            wiki_rev: None,
        },
    );
}

/// Attach a real workspace directory whose `vault/` holds `files`.
fn attach_storage(st: &mut crate::State, files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tmp");
    let seed = molt_storage::seed_entropy(&molt_storage::generate_seed_phrase().expect("phrase"))
        .expect("entropy");
    let genesis = molt_core::EventEnvelope {
        prev_seq: 0,
        seq: 1,
        ts: 10,
        by: "a".to_string(),
        body: molt_core::WorkspaceEvent::Founded {
            name: "R".to_string(),
            rule_m: 2,
            rule_n: 4,
            member: "a".to_string(),
            roster: vec!["a".to_string(), "b".to_string(), "c".to_string(), "d".to_string()],
            identities: Vec::new(),
            attestations: Vec::new(),
            republic_id: String::new(),
            agenda: String::new(),
            relays: Vec::new(),
            features: None,
        },
    };
    let ws = molt_storage::create_workspace(tmp.path(), &seed, &genesis).expect("create");
    for (secret_id, bytes) in files {
        ws.write_vault_payload(secret_id, bytes).expect("payload");
    }
    let dir = ws.dir().to_path_buf();
    st.active = Some(crate::ActiveStorage {
        id: "w-vault".to_string(),
        dir,
        prefs: molt_core::WorkspacePrefs::default(),
        handle: molt_storage::start_writer(ws),
    });
    st.files.vault.held_scope = None;
    tmp
}

/// Plan S3a: every payload a deposit names and this seat lacks gets a
/// fetch - committed or pending - and a held one, or one outside a vault
/// republic, gets none.
#[test]
fn the_vault_payload_tick_fetches_a_missing_payload() {
    let (mut st, dep, _) = seat_with_committed("b");
    st.vault_payload_tick();
    let fetch = st.files.vault.fetches.get(&dep.payload.hash).expect("the committed payload is fetched");
    assert!(fetch.next_try > 0, "an attempt was made");

    // a pending deposit is fetched too (the approver needs it to vote)
    let b = Builder::vault();
    let (pending_dep, _) = deposit(&b, "two", 2);
    pending(&mut st, 7, op(&pending_dep));
    st.vault_payload_tick();
    assert!(st.files.vault.fetches.contains_key(&pending_dep.payload.hash), "the pending one too");

    // held: nothing to fetch, and the running attempt is abandoned
    let (mut held, dep, _) = seat_with_committed("b");
    let id = sid(&held, &dep);
    mark_held(&mut held, &id);
    held.vault_payload_tick();
    assert!(held.files.vault.fetches.is_empty(), "a held payload is not fetched");

    // a v5 republic with the mock vault names nothing
    let mut mock_b = Builder::new_with_features(&["a", "b", "c", "d"], 2, &["vault"], false);
    mock_b.commit(ChainChange::Applied { proposal_id: 1, surface: Surface::Vault, payload: op(&dep) }, &["a", "b"]);
    let mut mock = chain_peer("b", &mock_b, mock_b.blocks.clone());
    mock.vault_payload_tick();
    assert!(mock.files.vault.fetches.is_empty(), "no vault, no payload plane");
}

/// Plan S3a / 1.3.14: the tick only ever adds - a payload nothing names
/// any more (a replaced version waiting for its cut) stays on disk.
#[tokio::test]
async fn the_vault_payload_tick_never_deletes() {
    let (mut st, dep, file) = seat_with_committed("b");
    let named = sid(&st, &dep);
    let stale = "ee".repeat(32);
    let _tmp = attach_storage(&mut st, &[(&named, &file), (&stale, b"old version")]);
    for _ in 0..3 {
        st.vault_payload_tick();
    }
    let dir = st.active.as_ref().expect("active").dir.clone();
    assert_eq!(molt_storage::list_vault_payloads(&dir), {
        let mut both = vec![named.clone(), stale.clone()];
        both.sort();
        both
    });
    assert!(st.files.vault.fetches.is_empty(), "the named one is held, nothing to fetch");
    // a wrong fetch result deletes nothing either
    st.cmd_net_vault_payload_fetched(dep.payload.hash.clone(), b"forged".to_vec()).expect("ack");
    assert_eq!(molt_storage::list_vault_payloads(&dir).len(), 2);
}

/// Spec §9.2: a fetched payload is held only when it matches the hash its
/// deposit commits to.
#[tokio::test]
async fn a_fetched_payload_is_held_only_when_it_matches_its_hash() {
    let (mut st, dep, file) = seat_with_committed("c");
    let id = sid(&st, &dep);
    let _tmp = attach_storage(&mut st, &[]);
    let mut forged = file.clone();
    forged[0] ^= 1;
    st.cmd_net_vault_payload_fetched(dep.payload.hash.clone(), forged).expect("ack");
    assert!(!st.vault_payload_held(&id), "a forged copy is not held");
    st.cmd_net_vault_payload_fetched(dep.payload.hash.clone(), file.clone()).expect("ack");
    assert!(st.vault_payload_held(&id));
    let dir = st.active.as_ref().expect("active").dir.clone();
    assert_eq!(molt_storage::list_vault_payloads(&dir), vec![id]);
}

/// Spec §9.2: holding is mandatory - a seat with the mirror off and the
/// file cap at zero still answers a piece want for a payload it holds.
#[tokio::test]
async fn a_vault_piece_is_served_with_the_mirror_off_and_cap_zero() {
    let (mut st, dep, file) = seat_with_committed("b");
    let id = sid(&st, &dep);
    let _tmp = attach_storage(&mut st, &[(&id, &file)]);
    st.nostr = Some(crate::NostrTransport {
        sk: zeroize::Zeroizing::new(vec![1u8; 32]),
        relays: Vec::new(),
        rotation_seed: [7u8; 32],
    });
    st.files.mirror.on = false;
    st.session.settings.file_cap_bytes = Some(0);
    let series = MessageId(molt_net::file_plane::vault_payload_series(&dep.payload.hash));

    st.cmd_net_piece_wanted(&"c".to_string(), series, vec![(0, 0)]).expect("ack");

    let store = st.file_store().expect("store");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let jobs = store.load().await.file_jobs.publish;
        if let Some(job) = jobs.iter().find(|j| j.series == series.to_string()) {
            assert_eq!(job.vault.as_deref(), Some(dep.payload.hash.as_str()));
            assert_eq!(job.path, id, "the sink's file name");
            assert_eq!(job.ranges, vec![(0, 0)]);
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the vault piece was never queued");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
