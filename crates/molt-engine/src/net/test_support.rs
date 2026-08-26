// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared fixtures of the net tests: a stamped presence fixture, its pill
//! reader and a deterministic message id.

use molt_core::{MemberInfo, MessageId};

/// A base instant for the presence tests, far from the thresholds.
pub(super) const T: u64 = 1_750_000_000;

pub(super) fn id(n: usize) -> MessageId {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&(u64::try_from(n).expect("small")).to_le_bytes());
    MessageId(b)
}

/// A state with an active workspace entry and a real 2-of-3 roster —
/// ada is the local member; nobody has been seen yet.
pub(super) fn presence_fixture() -> crate::State {
    let mut st = crate::tests::plain_state();
    st.presence.clock_override = Some(T);
    let roster: Vec<String> =
        vec!["ada".to_string(), "bob".to_string(), "cid".to_string()];
    st.replica = Some(crate::ReplicaState {
        member: "ada".to_string(),
        roster: roster.clone(),
        rule_m: 2,
        ..Default::default()
    });
    let id = "w-presence".to_string();
    st.session.active_workspace = id.clone();
    st.session.workspaces.push(molt_core::WorkspaceInfo {
        id,
        name: "Presence".to_string(),
        detail: "2-of-3".to_string(),
        synced: true,
        state: 0,
        last_sync_min: 0,
        sync_queue: 0,
        s3: false,
        size_kib: 0,
        last_backup_min: molt_core::WorkspaceInfo::NEVER,
        backup_copies: 0,
        backup_error: String::new(),
        seed: String::new(),
        net: "none".to_string(),
        encrypted: false,
        restored: false,
        members: molt_core::roster_members(&roster, T, |_| MemberInfo::NEVER),
        agenda: String::new(),
    });
    st
}

pub(super) fn pill(st: &crate::State, name: &str) -> MemberInfo {
    st.session
        .workspaces
        .iter()
        .find(|w| w.id == st.session.active_workspace)
        .expect("active entry")
        .members
        .iter()
        .find(|m| m.name == name)
        .expect("member pill")
        .clone()
}
