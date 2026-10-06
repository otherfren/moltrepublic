// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Vault complaints** (vault build plan S3c, spec §7, §14.3 c): a
//! complaint is decided from the depositor's reveal and names the cheater,
//! a reveal lowers the threshold the card shows, and a re-seal is a vote
//! that deals a fresh secret.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use molt_core::vault::{
    SecretText, VaultComplaintStatus, VaultComplaintView, VaultDeposit, VaultDepositView, VaultOp,
    VaultRefusal, VaultView,
};
use molt_core::{ChainChange, Command, MoltError, ProposalId, Reply, Surface};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

mod vault_support;
use vault_support::{close_and_open, found_n_at, read_session, NAMES};

const TEXT: &str = "amber-lantern-31";

async fn vault_view(w: &WalletHandle) -> VaultView {
    match w
        .execute(Command::ReadState { surface: Surface::Vault, channel: None, view: None })
        .await
        .expect("read vault")
    {
        Reply::State(s) => s.vault.expect("the vault view"),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_vault(w: &WalletHandle, what: &str, pred: impl Fn(&VaultView) -> bool) -> VaultView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
    loop {
        let v = vault_view(w).await;
        if pred(&v) {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}\n{v:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The committed card of `n` (the current version).
fn current(v: &VaultView) -> Option<&VaultDepositView> {
    v.deposits.iter().find(|d| d.name == "n" && d.proposal.is_none())
}

fn lines(v: &VaultView) -> Vec<VaultComplaintView> {
    current(v).map(|d| d.complaints.clone()).unwrap_or_default()
}

fn line(holder: &str, status: VaultComplaintStatus) -> VaultComplaintView {
    VaultComplaintView { holder: holder.to_string(), status }
}

async fn approve_when_held(w: &WalletHandle, id: ProposalId) -> Result<Reply, MoltError> {
    wait_vault(w, "the payload to arrive", |v| v.deposits.iter().any(|d| d.proposal == Some(id.0) && d.held)).await;
    w.execute(Command::Approve { proposal: id, note: None }).await
}

/// The open deposit card of `n` at `w`.
async fn pending_id(w: &WalletHandle) -> ProposalId {
    let v = wait_vault(w, "the pending deposit", |v| v.deposits.iter().any(|d| d.name == "n" && d.proposal.is_some()))
        .await;
    ProposalId(v.deposits.iter().find_map(|d| (d.name == "n").then_some(d.proposal).flatten()).expect("a proposal"))
}

/// A 2-of-4 vault republic in which `a` deposited `n` and `approver`
/// approved it; `complainer` complains whatever it finds, `a` lies on its
/// reveals when `lie` is set.
async fn deposit_with(
    root: &std::path::Path,
    url: &str,
    complainer: Option<usize>,
    lie: bool,
    approver: usize,
) -> (Vec<WalletHandle>, String) {
    let all = found_n_at(root, url, 4, 2, &["vault"]).await;
    if let Some(i) = complainer {
        all[i].__vault_complain(true);
    }
    all[0].__vault_lie_on_reveal(lie);
    let cmd = Command::VaultSeal { name: "n".into(), kind: "text".into(), text: SecretText(TEXT.into()) };
    let id = match all[0].execute(cmd).await.expect("sealed") {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    };
    approve_when_held(&all[approver], id).await.expect("approved");
    let sid = committed_everywhere(&all).await;
    (all, sid)
}

async fn committed_everywhere(all: &[WalletHandle]) -> String {
    let mut sid = String::new();
    for w in all {
        let v = wait_vault(w, "the deposit to commit", |v| current(v).is_some()).await;
        sid = current(&v).expect("committed").secret_id.clone();
    }
    sid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_false_complaint_names_the_complainer() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (all, _) = deposit_with(tmp.path(), &url, Some(1), false, 2).await;
    for w in &all {
        wait_vault(w, "false complaint by b", |v| lines(v) == [line("b", VaultComplaintStatus::False)]).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_share_names_the_depositor() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["vault"]).await;
    all[0].__vault_seal_with_bad_share("n", "text", TEXT, "c");
    let id = pending_id(&all[0]).await;
    match approve_when_held(&all[2], id).await {
        Err(MoltError::Vault(VaultRefusal::NotVerified)) => {}
        other => panic!("c must refuse its bad share, got {other:?}"),
    }
    approve_when_held(&all[1], id).await.expect("b's share is fine");
    committed_everywhere(&all).await;
    for w in &all {
        let v = wait_vault(w, "bad share from a", |v| lines(v) == [line("c", VaultComplaintStatus::BadShare)]).await;
        let card = current(&v).expect("the card");
        assert_eq!(card.readable_by, 2, "a bad share publishes nothing usable");
        assert!(!card.reseal);
    }
}

/// KEYSTONE §14.3 c.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lying_reveal_names_the_depositor() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (all, _) = deposit_with(tmp.path(), &url, Some(1), true, 2).await;
    for w in &all[1..] {
        wait_vault(w, "a lie by a", |v| lines(v) == [line("b", VaultComplaintStatus::Lie)]).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reveal_lowers_readable_by_and_offers_reseal() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (all, _) = deposit_with(tmp.path(), &url, Some(1), false, 2).await;
    for (i, w) in all.iter().enumerate() {
        let v = wait_vault(w, "the reveal", |v| current(v).is_some_and(|d| d.readable_by == 1)).await;
        let card = current(&v).expect("the card");
        assert_eq!(card.reseal, i == 0, "only the depositor is offered a re-seal");
    }
}

/// Every formatted tracing line of this test binary, from the first call on.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut b) = self.0.lock() {
            b.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture() -> &'static Captured {
    static CAPTURED: OnceLock<Captured> = OnceLock::new();
    CAPTURED.get_or_init(|| {
        let captured = Captured::default();
        let sink = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter("molt_engine=trace,molt_net=trace,molt_vault=trace,molt_storage=trace,molt_core=trace")
            .with_writer(move || sink.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");
        captured
    })
}

/// Every file under `dir`, raw.
fn files_under(dir: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).expect("dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else if let Ok(bytes) = std::fs::read(&path) {
            out.push((path, bytes));
        }
    }
    out
}

/// Re-seal after a false complaint and wait until it commits: `(old, new)`.
async fn reseal_committed(all: &[WalletHandle], old: &str) -> String {
    wait_vault(&all[0], "the re-seal offer", |v| current(v).is_some_and(|d| d.reseal)).await;
    all[1].__vault_complain(false);
    let id = match all[0].execute(Command::VaultReseal { secret_id: old.to_string() }).await.expect("re-sealed") {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    };
    let v = wait_vault(&all[0], "the re-seal card", |v| v.deposits.iter().any(|d| d.proposal == Some(id.0))).await;
    let card = v.deposits.iter().find(|d| d.proposal == Some(id.0)).expect("the card");
    assert_eq!(card.replaces.as_deref(), Some(old));
    let shown = all[0].execute(Command::ReadProposal { id: id.0 }).await.expect("read proposal");
    assert!(!format!("{shown:?}").contains(TEXT), "the re-seal card carries the text");
    assert!(current(&v).is_some_and(|d| !d.reseal), "no second offer while the re-seal is open");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let v = vault_view(&all[0]).await;
    assert_eq!(current(&v).map(|d| d.secret_id.as_str()), Some(old), "a vote, not automatic");
    approve_when_held(&all[2], id).await.expect("approved");
    let mut new = String::new();
    for w in all {
        let v = wait_vault(w, "the re-seal to commit", |v| current(v).is_some_and(|d| d.secret_id != old)).await;
        new = current(&v).expect("committed").secret_id.clone();
    }
    new
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reseal_is_a_vote_and_restores_the_threshold() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let captured = capture();
    let (all, old) = deposit_with(tmp.path(), &url, Some(1), false, 2).await;
    reseal_committed(&all, &old).await;
    for w in &all {
        let v = wait_vault(w, "the fresh version hardened", |v| {
            current(v).is_some_and(|d| d.state == molt_core::vault::VaultDepositState::Hardened)
        })
        .await;
        let card = current(&v).expect("the card");
        assert_eq!(card.readable_by, 2, "the threshold is back");
        assert!(card.complaints.is_empty() && !card.reseal);
    }
    for (name, ws) in close_and_open(tmp.path(), &all).await {
        let log = serde_json::to_string(&ws.read_log_from(1).expect("log")).expect("json");
        let transport = serde_json::to_string(&ws.read_transport_state()).expect("json");
        let chain = serde_json::to_string(&ws.read_chain().expect("chain").1).expect("json");
        for (what, s) in [("log", &log), ("transport", &transport), ("chain", &chain)] {
            assert!(!s.contains(TEXT), "{name}: the text is in its {what}");
        }
    }
    for (path, bytes) in files_under(tmp.path()) {
        assert!(!bytes.windows(TEXT.len()).any(|w| w == TEXT.as_bytes()), "the text is in {}", path.display());
    }
    let logs = String::from_utf8_lossy(&captured.0.lock().expect("lock")).into_owned();
    assert!(logs.contains("vault: re-seal"), "the capture works");
    assert!(!logs.contains(TEXT), "a log line carries the text");
}

fn deposits_of(chain: &[molt_core::ChainBlock]) -> Vec<VaultDeposit> {
    chain
        .iter()
        .filter_map(|b| match &b.change {
            ChainChange::Applied { surface: Surface::Vault, payload, .. } => {
                match serde_json::from_value::<VaultOp>(payload.clone()) {
                    Ok(VaultOp::Deposit(d)) => Some(d),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reseal_deals_a_fresh_secret() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (all, old_sid) = deposit_with(tmp.path(), &url, Some(1), false, 2).await;
    let new_sid = reseal_committed(&all, &old_sid).await;

    let opened = close_and_open(tmp.path(), &all).await;
    let (_, chain) = opened[0].1.read_chain().expect("chain");
    let ChainChange::Genesis { republic_id, rule_m, identities, .. } = &chain[0].change else {
        panic!("block 0 is not a genesis");
    };
    let deps = deposits_of(&chain);
    assert_eq!(deps.len(), 2);
    let (old, new) = (&deps[0], &deps[1]);
    assert_eq!(molt_core::vault::secret_id(republic_id, new), new_sid);
    assert_ne!(old.nonce, new.nonce, "a fresh nonce");
    assert_ne!(old.commitments, new.commitments, "a fresh polynomial");

    // the old shares plus the reveal recombine the OLD secret only
    let ctx = molt_core::vault::VaultCtx {
        m: *rule_m,
        holders_in_genesis_order: identities
            .iter()
            .map(|i| (i.member.clone(), i.identity_pk.clone(), i.vault_pk.clone()))
            .collect(),
    };
    let seed = |i: usize| -> [u8; 32] {
        let ts = opened[i].1.read_transport_state();
        <[u8; 32]>::try_from(ts.vault_seed.expect("a vault seed").0.as_slice()).expect("32 bytes")
    };
    let x = |name: &str| u8::try_from(1 + identities.iter().position(|i| i.member == name).expect("a seat")).expect("x");
    let (revealed, _) = molt_vault::rederive_share(old, republic_id, &ctx, &seed(0), "b").expect("b's reveal");
    let (c_sk, _) = molt_vault::vault_keypair(&seed(2));
    let c_share = molt_vault::check_my_share(old, republic_id, &ctx, "c", &c_sk).expect("c's old share");
    let s_old = molt_vault::combine(*rule_m, &[(x("b"), revealed), (x("c"), c_share)]).expect("combined");
    let old_file = opened[3].1.read_vault_payload(&old_sid).expect("read").expect("held");
    let new_file = opened[3].1.read_vault_payload(&new_sid).expect("read").expect("held");
    let text = molt_vault::open_payload(old, republic_id, &s_old, &old_file).expect("the old version opens");
    assert_eq!(text.0, TEXT);
    assert!(
        molt_vault::open_payload(new, republic_id, &s_old, &new_file).is_err(),
        "the old shares open the new version"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_member_sees_the_reveal_verdict_again() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (all, _) = deposit_with(tmp.path(), &url, Some(1), false, 2).await;
    let (a, d) = (&all[0], &all[3]);
    wait_vault(d, "the verdict at d", |v| lines(v) == [line("b", VaultComplaintStatus::False)]).await;

    // d comes back from a backup: transport.state is not exported
    let id = read_session(d).await.active_workspace.clone();
    d.execute(Command::CloseWorkspace).await.expect("close");
    {
        let dir = molt_storage::find_workspace_dir(&tmp.path().join(NAMES[3]), &id).expect("dir");
        let (ws, _) = molt_storage::open_workspace(&dir).expect("open");
        let mut ts = ws.read_transport_state();
        assert!(!ts.vault_status.is_empty(), "the verdict was persisted");
        ts.vault_status = molt_core::vault::VaultStatusStore::default();
        ws.write_transport_state(&ts).expect("restored state");
    }
    d.execute(Command::OpenWorkspace { id }).await.expect("reopen");
    let v = wait_vault(d, "the card after the restore", |v| current(v).is_some()).await;
    assert!(lines(&v).is_empty(), "the restore lost the verdict");

    // the depositor re-sends its reveals when it starts
    let id = read_session(a).await.active_workspace.clone();
    a.execute(Command::CloseWorkspace).await.expect("close");
    a.execute(Command::OpenWorkspace { id }).await.expect("reopen");
    wait_vault(d, "the verdict again", |v| lines(v) == [line("b", VaultComplaintStatus::False)]).await;
}
