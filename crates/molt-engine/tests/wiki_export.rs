// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The wiki export** (`docs/memory/wiki_export_plan.md`): the Shared-Memory
//! tree written to a user-picked directory as plain files, optionally with the
//! proof bundle that lets an OUTSIDER verify it — no moltd, no workspace key,
//! no trust in the exporter.
//!
//! What these pin: the real write (tree + `proof/bundle.json` +
//! `proof/README.md`), the honest refusals (empty wiki, no target, no chain
//! behind a `proof: true`, a second export while one runs), and the round trip
//! — `verify_wiki_export` over exactly what was written is green, and goes red
//! on a single flipped byte in an exported file.

use std::time::Duration;

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView, Surface};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

const ADD_A: &str = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,2 @@\n+hello\n+world\n";

// ---- helpers ---------------------------------------------------------------

/// A single-member group: threshold 1, no self-cosign — one `Approve`
/// applies. It has NO chain (the legacy counted path), which is exactly the
/// workspace shape a `proof: true` export must refuse.
fn spawn_solo() -> WalletHandle {
    molt_engine::spawn(
        GroupConfig {
            member: "me".to_string(),
            members: vec!["me".to_string()],
            threshold: 1,
            self_cosign: false,
        },
        SessionView::default(),
    )
}

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_for(
    w: &WalletHandle,
    what: &str,
    pred: impl Fn(&SessionView) -> bool,
) -> Box<SessionView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let s = read_session(w).await;
        if pred(&s) {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}\nsession: notice={:?} create.log={:?} join.log={:?}",
            s.notice,
            s.create.run.log,
            s.join.run.log
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll until the in-flight wiki export settles.
async fn await_wiki_export(w: &WalletHandle) -> molt_core::ExportState {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let s = read_session(w).await;
        if !s.wiki_export.running && !s.wiki_export.result.is_empty() {
            return s.wiki_export.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the wiki export never settled: {:?}",
            s.wiki_export
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Propose one wiki patch and return its id.
async fn propose_wiki(w: &WalletHandle, patch: &str) -> molt_core::ProposalId {
    match w
        .execute(Command::Propose {
            surface: Surface::Memory,
            payload: serde_json::json!({
                "op": "wiki_patch",
                "value": patch,
                "summary": "a.md",
            }),
        })
        .await
        .expect("propose the patch")
    {
        Reply::Proposed { id } => id,
        other => panic!("unexpected: {other:?}"),
    }
}

/// The wiki base as the engine serves it (co-equality: the same projection
/// the GUI renders).
async fn wiki_docs(w: &WalletHandle) -> Vec<molt_core::WikiDoc> {
    match w
        .execute(Command::ReadState {
            surface: Surface::Memory,
            channel: None,
            view: None,
        })
        .await
        .expect("read memory")
    {
        Reply::State(s) => s.wiki_tree,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Apply one wiki patch on the single-operator path (propose + the one
/// approval the threshold needs).
async fn apply_wiki_patch(w: &WalletHandle, patch: &str) {
    let id = propose_wiki(w, patch).await;
    w.execute(Command::Approve { proposal: id })
        .await
        .expect("approve");
    assert!(!wiki_docs(w).await.is_empty(), "the patch applied");
}

fn engine(root: &std::path::Path) -> WalletHandle {
    let session = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    molt_engine::spawn_with_storage(GroupConfig::demo(), session)
}

async fn adopt_relay(w: &WalletHandle, url: &str) {
    w.execute(Command::RelayAdd { url: url.to_string() })
        .await
        .expect("relay add");
    w.execute(Command::RelayConfirm {
        url: url.to_string(),
        accept_clearnet: true,
    })
    .await
    .expect("relay confirm");
    wait_for(w, "the relay probe to confirm the relay", |s| {
        s.settings
            .relays
            .iter()
            .any(|r| r.url.trim_end_matches('/') == url.trim_end_matches('/') && r.confirmed)
    })
    .await;
    w.execute(Command::RelayClearnetSession { unlock: true })
        .await
        .expect("session unlock");
}

/// Found a real 2-of-2 republic over the relay; both engines end up entered
/// (the `org_effective.rs` pattern — a chain-governed pair is the only place
/// a real m-of-n wiki patch can be committed).
async fn found_pair(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Wiki Republic".to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let s = wait_for(&a, "the seat link to become joinable", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();

    let b = engine(&root.join("member"));
    adopt_relay(&b, url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "walter".to_string(),
    })
    .await
    .expect("join starts");
    wait_for(&a, "the founder to accept the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Wiki Republic".to_string(),
        agenda: "write things down".to_string(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("charter proposed");
    {
        let seed = read_session(&a).await.create.seed.clone();
        a.execute(Command::ConfirmSeedBackup { phrase: seed })
            .await
            .expect("founder backup confirm");
    }
    wait_for(&b, "walter to see the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    {
        let seed = read_session(&b).await.join.seed.clone();
        b.execute(Command::ConfirmSeedBackup { phrase: seed })
            .await
            .expect("joiner backup confirm");
    }
    wait_for(&a, "the founding to seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    wait_for(&b, "the join to seal", |s| {
        s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()
    })
    .await;
    b.execute(Command::JoinFinish).await.expect("join finish");
    wait_for(&b, "the joiner to enter", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    (a, b)
}

/// The second voice: wait until `w` sees the open proposal with this `op`,
/// then approve it through the public command surface.
async fn approve_op(w: &WalletHandle, op: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Reply::Proposals { proposals } =
            w.execute(Command::ListProposals).await.expect("list proposals")
        {
            if let Some(p) = proposals.iter().find(|p| {
                p.state == molt_core::ProposalState::Proposed
                    && p.payload.get("op").and_then(|v| v.as_str()) == Some(op)
            }) {
                w.execute(Command::Approve { proposal: p.id })
                    .await
                    .expect("approve");
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the {op} proposal never reached the second voice"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---- the refusals ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_wiki_and_an_empty_target_are_refused() {
    let tmp = tempfile::tempdir().expect("tmp");
    let dest = tmp.path().join("out");
    let w = spawn_solo();
    let err = w
        .execute(Command::WikiExport {
            dest: dest.display().to_string(),
            proof: false,
        })
        .await
        .expect_err("an empty wiki has nothing to export");
    assert_eq!(err.to_string(), "wiki export: the wiki is empty");
    assert!(!dest.exists(), "a refused export writes nothing");

    apply_wiki_patch(&w, ADD_A).await;
    let err = w
        .execute(Command::WikiExport {
            dest: "   ".to_string(),
            proof: false,
        })
        .await
        .expect_err("no target directory");
    assert_eq!(err.to_string(), "wiki export: a target directory is required");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proof_without_chain_governance_is_refused_and_files_still_export() {
    let tmp = tempfile::tempdir().expect("tmp");
    let dest = tmp.path().join("out");
    let w = spawn_solo();
    apply_wiki_patch(&w, ADD_A).await;

    let err = w
        .execute(Command::WikiExport {
            dest: dest.display().to_string(),
            proof: true,
        })
        .await
        .expect_err("there are no threshold signatures to prove anything with");
    assert_eq!(err.to_string(), "wiki export: proof needs chain governance");
    assert!(!dest.exists(), "a refused export writes nothing");

    // files-only stays available — no fake proof, but the tree is real
    w.execute(Command::WikiExport {
        dest: dest.display().to_string(),
        proof: false,
    })
    .await
    .expect("the files-only export starts");
    let outcome = await_wiki_export(&w).await;
    assert_eq!(outcome.result, "ok", "{outcome:?}");
    assert_eq!(outcome.files, 1);
    assert_eq!(
        std::fs::read_to_string(dest.join("wiki").join("a.md")).expect("the exported file"),
        "hello\nworld\n"
    );
    assert!(!dest.join("proof").exists(), "no proof directory without proof");
}

/// A FIFO at the first file's path blocks the writer, so the second export
/// provably arrives while the first is still in flight.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_export_while_one_runs_is_refused() {
    let tmp = tempfile::tempdir().expect("tmp");
    let dest = tmp.path().join("out");
    std::fs::create_dir_all(dest.join("wiki")).expect("dest");
    let fifo = dest.join("wiki").join("a.md");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo runs");
    assert!(made.success(), "mkfifo created the blocking target");

    let w = spawn_solo();
    apply_wiki_patch(&w, ADD_A).await;

    // the rescue reader runs no matter how the assertions below end, so a
    // failure cannot leave the writer blocked forever
    let fifo_path = fifo.clone();
    let rescue = tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(300));
        let _ = std::fs::read(&fifo_path);
    });

    w.execute(Command::WikiExport {
        dest: dest.display().to_string(),
        proof: false,
    })
    .await
    .expect("the first export starts");
    let err = w
        .execute(Command::WikiExport {
            dest: dest.display().to_string(),
            proof: false,
        })
        .await
        .expect_err("one export at a time");
    assert_eq!(err.to_string(), "wiki export: an export is already running");

    rescue.await.expect("rescue reader");
    let outcome = await_wiki_export(&w).await;
    assert!(!outcome.running, "the slot is free again: {outcome:?}");
}

// ---- the real export, and the round trip -----------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_governed_export_verifies_against_what_it_wrote() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b) = found_pair(&tmp.path().join("workspaces"), &url).await;

    // a REAL m-of-n wiki patch: petra proposes, walter is the second voice
    propose_wiki(&a, ADD_A).await;
    approve_op(&b, "wiki_patch").await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while wiki_docs(&a).await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the patch never committed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let dest = tmp.path().join("export");
    a.execute(Command::WikiExport {
        dest: dest.display().to_string(),
        proof: true,
    })
    .await
    .expect("the export starts");
    let outcome = await_wiki_export(&a).await;
    assert_eq!(outcome.result, "ok", "{outcome:?}");
    assert_eq!(outcome.files, 1);
    assert!(outcome.bytes > 0);

    assert_eq!(
        std::fs::read_to_string(dest.join("wiki").join("a.md")).expect("the exported doc"),
        "hello\nworld\n"
    );
    let readme = std::fs::read_to_string(dest.join("proof").join("README.md")).expect("readme");
    assert!(
        readme.contains("molt-chain-change-v2")
            && readme.to_lowercase().contains("completeness"),
        "the reviewer's README carries the layout and the honest limitation"
    );

    // the round trip: the shipped verifier over exactly what was written
    let (bundle, tree) = molt_engine::read_wiki_export(&dest).expect("the export reads back");
    let report = molt_engine::verify_wiki_export(&bundle, &tree).expect("the export verifies");
    assert_eq!(report.patches, 1);
    assert_eq!(report.files, 1);
    assert_eq!(report.rule_m, 2);
    assert_eq!(
        report.members,
        vec!["petra".to_string(), "walter".to_string()]
    );

    // one flipped byte in an exported file and the proof no longer holds
    std::fs::write(dest.join("wiki").join("a.md"), "hello\nWorld\n").expect("tamper");
    let (bundle, tree) = molt_engine::read_wiki_export(&dest).expect("the export reads back");
    let err = molt_engine::verify_wiki_export(&bundle, &tree).expect_err("tampered");
    assert!(err.contains("a.md"), "the fault names the file: {err}");
}

/// A symlink planted in the target path is an ESCAPE from `<dest>`: writing
/// through it puts wiki content into a file the user never picked. Refuse it,
/// and leave the linked file untouched.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlink_in_the_target_path_is_refused() {
    let tmp = tempfile::tempdir().expect("tmp");
    let outside = tmp.path().join("outside.txt");
    std::fs::write(&outside, "not the export's business").expect("outside file");
    let dest = tmp.path().join("out");
    std::fs::create_dir_all(dest.join("wiki")).expect("dest");
    std::os::unix::fs::symlink(&outside, dest.join("wiki").join("a.md")).expect("symlink");

    let w = spawn_solo();
    apply_wiki_patch(&w, ADD_A).await;
    w.execute(Command::WikiExport {
        dest: dest.display().to_string(),
        proof: false,
    })
    .await
    .expect("the export starts");
    let outcome = await_wiki_export(&w).await;
    assert!(
        outcome.result.starts_with("error: ") && outcome.result.contains("a.md"),
        "the symlink must fail the export honestly: {outcome:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&outside).expect("the linked file"),
        "not the export's business",
        "nothing is written through the link"
    );
}
