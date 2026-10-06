// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The vault across a cut, a recovery and a total restore** (vault build
//! plan S5, spec §9.3-§9.5, §14.5): the cut folds the vault into a base the
//! file plane carries, a recovered seat fetches it and reads a grant, and a
//! republic whose every seat comes back from its own backup keeps its
//! vault.

use std::time::Duration;

use molt_core::vault::{SecretText, VaultDepositState, VaultGrantState, VaultMyCheck, VaultView};
use molt_core::{Command, ProposalId, Reply, Surface};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

mod vault_support;
use vault_support::{engine, found_n_at, read_session, wait_for, NAMES};

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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        let v = vault_view(w).await;
        if pred(&v) {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}\n{v:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn proposed(r: Reply) -> ProposalId {
    match r {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    }
}

/// `a` deposits `text` under `n`, `b` approves; every seat holds it.
async fn deposited(all: &[WalletHandle], text: &str) -> String {
    let cmd = Command::VaultSeal { name: "n".into(), kind: "text".into(), text: SecretText(text.into()) };
    let id = proposed(all[0].execute(cmd).await.expect("sealed"));
    wait_vault(&all[1], "the payload to arrive", |v| {
        v.deposits.iter().any(|d| d.proposal == Some(id.0) && d.held)
    })
    .await;
    all[1].execute(Command::Approve { proposal: id, note: None }).await.expect("approved");
    let mut sid = String::new();
    for w in all {
        let v = wait_vault(w, "the deposit to commit and arrive", |v| {
            v.deposits.iter().any(|d| d.name == "n" && d.state != VaultDepositState::Pending && d.held)
        })
        .await;
        sid = v.deposits.iter().find(|d| d.name == "n").expect("card").secret_id.clone();
    }
    sid
}

async fn has_cut(w: &WalletHandle) -> bool {
    match w.execute(Command::ReadChain).await.expect("read chain") {
        Reply::Chain { blocks, .. } => blocks.iter().any(|v| v.kind == "checkpoint"),
        other => panic!("unexpected: {other:?}"),
    }
}

/// `all[0]` proposes the cut; every seat co-signs (n-of-n) and applies it.
async fn cut(all: &[WalletHandle]) {
    all[0].execute(Command::ProposeCheckpoint).await.expect("cut proposed");
    for (i, w) in all.iter().enumerate() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !has_cut(w).await {
            assert!(tokio::time::Instant::now() < deadline, "{} never applied the cut", NAMES[i]);
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let v = vault_view(w).await;
        assert!(v.base_pending.is_none(), "{}: the cut seat holds its base", NAMES[i]);
        assert_eq!(v.deposits.len(), 1, "{}: the deposit reads from the base", NAMES[i]);
    }
}

/// `proposer` names `reader`; `approver` makes it m. Returns the grant id.
async fn grant(proposer: &WalletHandle, approver: &WalletHandle, sid: &str, reader: &str) -> String {
    let id = proposed(
        proposer
            .execute(Command::VaultGrant { secret_id: sid.to_string(), reader: reader.to_string() })
            .await
            .expect("grant proposed"),
    );
    let v = wait_vault(approver, "the grant card", |v| v.grants.iter().any(|g| g.proposal == Some(id.0))).await;
    let gid = v.grants.iter().find(|g| g.proposal == Some(id.0)).expect("card").grant_id.clone();
    approver.execute(Command::Approve { proposal: id, note: None }).await.expect("grant approved");
    gid
}

async fn wait_grant(w: &WalletHandle, gid: &str) {
    wait_vault(w, "the grant to commit", |v| {
        v.grants.iter().any(|g| g.grant_id == gid && g.state == VaultGrantState::Committed)
    })
    .await;
}

async fn read_text(w: &WalletHandle, sid: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        match w.execute(Command::VaultRead { secret_id: sid.to_string() }).await {
            Ok(Reply::VaultText { text, .. }) => return text.0,
            Ok(Reply::VaultPending { have, need, .. }) => {
                assert!(tokio::time::Instant::now() < deadline, "still {have} of {need}");
            }
            // the base or the payload may still be arriving
            Err(e) => assert!(tokio::time::Instant::now() < deadline, "read: {e}"),
            Ok(other) => panic!("unexpected: {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn reveal_seed(w: &WalletHandle) -> String {
    let id = read_session(w).await.active_workspace.clone();
    match w.execute(Command::RevealSeed { id }).await.expect("reveal") {
        Reply::Seed { seed } => seed,
        other => panic!("unexpected: {other:?}"),
    }
}

/// KEYSTONE §14.5: deposit, cut, `d` loses its device and recovers on a
/// fresh one; the pruned chain makes it base-pending until the base and
/// the payload arrive over the relay; a grant to it then reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deposit_cut_recover_grant_to_the_recovered_seat_reads() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let mut all = found_n_at(tmp.path(), &url, 4, 2, &["vault"]).await;
    let text = "quiet-lantern-17";
    let sid = deposited(&all, text).await;
    cut(&all).await;

    let phrase = reveal_seed(&all[3]).await;
    let d = all.pop().expect("d");
    d.execute(Command::CloseWorkspace).await.expect("d goes away");
    drop(d);

    all[0].execute(Command::RecoverInviteStart { member: "d".to_string() }).await.expect("mint");
    let s = wait_for(&all[0], "the recovery link", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link = s
        .notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("the mint must succeed: {:?}", s.notice))
        .to_string();
    let d = engine(&tmp.path().join("d-new"));
    vault_support::adopt_relay(&d, &url).await;
    d.execute(Command::RecoverStart { link, phrase }).await.expect("recover start");
    let s = wait_for(&d, "the recovery to open", |s| {
        (s.screen == molt_core::Screen::Main && s.notice.starts_with("recovered:"))
            || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "recovery: {:?}", s.notice);

    // the base and the payload come off the plane; the seed came from the
    // phrase, checked against the founding vault_pk
    let v = wait_vault(&d, "the base and the payload", |v| {
        v.base_pending.is_none() && v.deposits.iter().any(|x| x.secret_id == sid && x.held)
    })
    .await;
    let card = v.deposits.iter().find(|x| x.secret_id == sid).expect("card");
    assert_eq!(card.my_check, VaultMyCheck::Ok, "the recovered seat's share opens");

    let gid = grant(&all[1], &all[2], &sid, "d").await;
    wait_grant(&d, &gid).await;
    assert_eq!(read_text(&d, &sid).await, text);
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for e in std::fs::read_dir(from).expect("read dir") {
        let e = e.expect("entry");
        let dest = to.join(e.file_name());
        if e.file_type().expect("type").is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).expect("copy");
        }
    }
}

/// KEYSTONE §14.5, the twin of `a_folded_wiki_survives_every_seat_restoring_from_backup`:
/// after a cut every seat backs up. Every seat's export restores on a
/// fresh device with no holder online and finds its vault whole (base,
/// payload, seed); every seat's own device then comes back from its
/// backup copy alone, and a grant still reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vault_survives_every_seat_restoring_from_backup_after_a_cut() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["vault"]).await;
    let text = "copper-orchard-42";
    let sid = deposited(&all, text).await;
    cut(&all).await;

    // every seat backs up after the cut: an export blob and a copy of its dir
    let pass = "correct horse battery";
    let mut seats = Vec::new();
    for (i, w) in all.iter().enumerate() {
        let ws = read_session(w).await.active_workspace.clone();
        let blob = tmp.path().join(format!("{}.molt.enc", NAMES[i]));
        w.execute(Command::ExportWorkspace {
            id: ws.clone(),
            dest: blob.display().to_string(),
            passphrase: pass.to_string(),
        })
        .await
        .expect("export kickoff");
        let s = wait_for(w, "the export", |s| !s.export.running && !s.export.result.is_empty()).await;
        assert_eq!(s.export.result, "ok", "{}: the export succeeds", NAMES[i]);
        w.execute(Command::CloseWorkspace).await.expect("close");
        let dir = molt_storage::find_workspace_dir(&tmp.path().join(NAMES[i]), &ws).expect("dir");
        let copy = tmp.path().join(format!("{}-copy", NAMES[i]));
        copy_dir(&dir, &copy);
        seats.push((ws, dir, copy, blob));
    }

    // every blob restores on a fresh device, nobody online: the vault is
    // whole from the backup alone
    for (i, (_, _, _, blob)) in seats.iter().enumerate() {
        let fresh = engine(&tmp.path().join(format!("{}-fresh", NAMES[i])));
        fresh
            .execute(Command::RestoreStart {
                way: "file".to_string(),
                target: blob.display().to_string(),
                secret: pass.to_string(),
                replace: false,
            })
            .await
            .expect("restore start");
        let s = wait_for(&fresh, "the restore", |s| s.restore.run.outcome != 0).await;
        assert_eq!(s.restore.run.outcome, 1, "{}: {:?}", NAMES[i], s.restore.run.log);
        fresh.execute(Command::RestoreFinish).await.expect("restore finish");
        let v = wait_vault(&fresh, "the restored vault", |v| {
            v.base_pending.is_none() && v.deposits.iter().any(|x| x.secret_id == sid && x.held)
        })
        .await;
        let card = v.deposits.iter().find(|x| x.secret_id == sid).expect("card");
        let want = if i == 0 { VaultMyCheck::None } else { VaultMyCheck::Ok };
        assert_eq!(card.my_check, want, "{}: the seed re-derives from the phrase", NAMES[i]);
        fresh.execute(Command::CloseWorkspace).await.expect("close");
    }

    // every seat's device loses its disk and comes back from its copy
    for (i, (ws, dir, copy, _)) in seats.iter().enumerate() {
        std::fs::remove_dir_all(dir).expect("the disk is gone");
        copy_dir(copy, dir);
        all[i].execute(Command::OpenWorkspace { id: ws.clone() }).await.expect("reopen");
        wait_for(&all[i], "back in", |s| s.active_workspace == *ws).await;
    }
    for w in &all {
        wait_vault(w, "the vault from the copy", |v| {
            v.base_pending.is_none() && v.deposits.iter().any(|x| x.secret_id == sid && x.held)
        })
        .await;
    }
    let gid = grant(&all[0], &all[1], &sid, "c").await;
    wait_grant(&all[2], &gid).await;
    assert_eq!(read_text(&all[2], &sid).await, text);
}
