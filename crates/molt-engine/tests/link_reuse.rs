// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **A founding invite link used twice** (founder mistake: the same link
//! sent to two people). The ticket is single-use; the second activation
//! must fail LOUDLY on both sides instead of leaving the second joiner in
//! an endless wait:
//!
//! 1. The second joiner's `run_ritual_member` returns an error naming the
//!    spent link (its engine surfaces it as a failed join → the wizard's
//!    red state + "Back to start", so they can retry with a fresh link).
//! 2. The founder's ritual log records the double activation prominently.
//! 3. The FIRST joiner's seat stays anchored and untouched — the ritual
//!    remains viable for the remaining links.

use std::time::Duration;

use molt_core::{Command, Reply, SessionSettings, SessionView};
use molt_engine::WalletHandle;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_activation_of_the_same_link_fails_loudly_on_both_sides() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root_a.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let (a, material_rx) =
        molt_engine::__spawn_manual_founding(molt_core::GroupConfig::demo(), session_a);
    // 2-of-3: two invites, so the ritual stays pending after the collision
    a.execute(Command::CreateStart {
        name: "Twice".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 3,
    })
    .await
    .expect("create start");
    let materials = tokio::task::spawn_blocking(move || {
        material_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("invite material")
    })
    .await
    .expect("join blocking");
    let mut it = materials.into_iter();
    let seat0 = it.next().expect("seat 0 material");
    let seat0_again = seat0.clone();

    // joiner 1 activates the link legitimately (they block later at the
    // charter step, which this test never reaches — the task is abandoned
    // at the end of the test)
    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let _b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat0, "member-b".to_string(), b_phrase, true, false, None, None)
            .await
    });

    // wait until the founder anchored member-b on seat 0
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let s = read_session(&a).await;
        if s.create
            .seats
            .first()
            .is_some_and(|v| v.member == "member-b" && v.state >= 1)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "member-b never activated; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // joiner 2 uses the SAME link — the ticket is spent, so this must come
    // back as a loud error instead of an endless wait
    let c_phrase = molt_storage::generate_seed_phrase().expect("c phrase");
    let c_res = tokio::time::timeout(
        Duration::from_secs(10),
        molt_engine::run_ritual_member(
            seat0_again,
            "member-c".to_string(),
            c_phrase,
            true,
            false,
            None,
            None,
        ),
    )
    .await
    .expect("the second activation must FAIL, not hang forever");
    let err = match c_res {
        Err(e) => e,
        Ok(_) => panic!("a spent link cannot join"),
    };
    assert!(
        err.contains("already"),
        "the error names the spent link: {err}"
    );

    // the founder saw it and said so in the ritual log
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let s = read_session(&a).await;
        if s.create
            .run
            .log
            .iter()
            .any(|l| l.starts_with('✗') && l.contains("member-c"))
        {
            // …and the first joiner's seat is untouched: still member-b,
            // still anchored — the ritual stays viable for the other links
            let seat = s.create.seats.first().expect("seat 0");
            assert_eq!(seat.member, "member-b", "seat 0 keeps its first member");
            assert!(seat.state >= 1, "seat 0 stays anchored");
            assert_eq!(
                s.create.run.outcome, 0,
                "one spent-link mistake must not kill the whole ritual"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the founder never logged the double activation; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
