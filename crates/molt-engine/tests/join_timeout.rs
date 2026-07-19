// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The "Contacting the inviter…" wait must have a deadline.** A spent
//! invite link used against a FINISHED (or cancelled) ritual is dropped
//! silently on the founder side (stale generation), and a founder that is
//! simply offline answers nothing at all — in both cases the joiner used
//! to hang forever in the first wait. Pinned here: when nobody answers
//! the JoinRequest, `run_ritual_member` fails within its accept deadline
//! with a reason that tells the human what to do (ask for a fresh link),
//! instead of waiting forever.
//!
//! Runs on a paused current-thread runtime, so the 90 s deadline elapses
//! instantly once the task has nothing left to poll.

use molt_net::wrap::WrapKey;
use molt_net::{LoopbackHub, Transport};

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_join_nobody_answers_times_out_with_a_clear_error() {
    let hub = LoopbackHub::calm();
    let transport = hub.transport();
    // the invite queue exists (the link parsed fine) — but NOBODY is
    // listening behind it: the founding is over / the founder is gone
    let invite_q = transport.create_queue().await.expect("invite queue");
    let material = molt_engine::InviteMaterial {
        seat: 0,
        transport,
        invite_snd: invite_q.snd,
        invite_wrap: WrapKey::fresh().expect("wrap"),
        ticket: "ab".repeat(32),
    };
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let res = molt_engine::run_ritual_member(
        material,
        "loner".to_string(),
        phrase,
        true,
        false,
        None,
        None,
    )
    .await;
    let err = match res {
        Err(e) => e,
        Ok(_) => panic!("nobody answered — the join cannot have succeeded"),
    };
    assert!(
        err.contains("did not answer"),
        "the error explains the silent inviter: {err}"
    );
    assert!(
        err.contains("fresh link"),
        "the error tells the human the way out: {err}"
    );
}
