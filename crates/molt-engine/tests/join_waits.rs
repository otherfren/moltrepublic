// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The wait for the inviter has no deadline.** A founder may be
//! unreachable for hours; the joiner keeps waiting (and re-announcing)
//! until it is answered or cancelled - it never fails on a timer.
//!
//! Runs on a paused current-thread runtime, so hours elapse instantly.

use molt_net::wrap::WrapKey;
use molt_net::{LoopbackHub, Transport};

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_join_nobody_answers_keeps_waiting() {
    let hub = LoopbackHub::calm();
    let transport = hub.transport();
    // the invite queue exists, but nobody is listening behind it yet
    let invite_q = transport.create_queue().await.expect("invite queue");
    let material = molt_engine::InviteMaterial {
        seat: 0,
        transport,
        invite_snd: invite_q.snd,
        invite_wrap: WrapKey::fresh().expect("wrap"),
        ticket: "ab".repeat(32),
    };
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let join = tokio::spawn(molt_engine::run_ritual_member(
        material,
        "loner".to_string(),
        phrase,
        true,
        false,
        None,
        None,
    ));
    tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
    assert!(!join.is_finished(), "six silent hours must not end the join");
    join.abort();
}
