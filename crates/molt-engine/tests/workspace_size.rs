// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The workspace list's `size_kib` is the real on-disk footprint, not a
//! constant: filled when the create finish materializes the directory,
//! refreshed when the workspace closes (the flushed log + closing
//! snapshot land on disk), always through the one
//! `molt_storage::workspace_size_kib` helper the boot scan uses too.

use std::time::Duration;

use molt_core::{ChannelRef, Command, Reply, SessionSettings, SessionView};
use molt_engine::WalletHandle;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn await_founding(w: &WalletHandle) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let s = read_session(w).await;
        match s.create.run.outcome {
            1 => return,
            2 => panic!("founding failed: {:?}", s.create.run.log),
            _ => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "founding did not seal in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn entry_size(s: &SessionView, id: &str) -> u32 {
    s.workspaces
        .iter()
        .find(|ws| ws.id == id)
        .expect("workspace entry")
        .size_kib
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_entry_reports_real_on_disk_size() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let session = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w = molt_engine::__spawn_sim_founding(molt_core::GroupConfig::demo(), session, true);

    w.execute(Command::CreateStart {
        name: "Sized Club".to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 3,
    })
    .await
    .expect("create start");
    await_founding(&w).await;
    w.execute(Command::CreateFinish).await.expect("enter");

    // the freshly founded entry carries the directory's real size — a
    // straggling async prefs write may shift it by a rounding step, so the
    // pin is "within 1 KiB of the disk", which the old constant 16 cannot be
    let s = read_session(&w).await;
    let id = s.active_workspace.clone();
    let dir = molt_storage::find_workspace_dir(&root, &id).expect("workspace dir");
    let real = u32::try_from(molt_storage::workspace_size_kib(&dir)).expect("fits u32");
    let at_finish = entry_size(&s, &id);
    assert!(at_finish > 0, "a materialized workspace has a footprint");
    assert!(
        at_finish.abs_diff(real) <= 1,
        "entry says {at_finish} KiB but the directory holds {real} KiB — \
         the entry must carry the real size, not a constant"
    );

    // grow the log, then close cleanly: the flushed messages + closing
    // snapshot are on disk and the list entry must follow, exactly
    let line = "x".repeat(256);
    for _ in 0..200 {
        w.execute(Command::Chat {
            body: line.clone(),
            quote: None,
            channel: ChannelRef::default(),
        })
        .await
        .expect("chat");
    }
    w.execute(Command::CloseWorkspace).await.expect("close");
    let s = read_session(&w).await;
    let at_close = entry_size(&s, &id);
    assert!(
        at_close > at_finish,
        "the entry follows on-disk growth ({at_finish} KiB -> {at_close} KiB)"
    );
    let real = u32::try_from(molt_storage::workspace_size_kib(&dir)).expect("fits u32");
    assert_eq!(
        at_close, real,
        "after a clean close the entry matches the flushed directory exactly"
    );
}
