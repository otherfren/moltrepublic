// SPDX-License-Identifier: GPL-3.0-or-later

//! The relay file plane: fetch tasks end at the workspace boundary.


/// FP3: a relay-plane fetch task holds a PRIVATE subscription (its own
/// relay runtime, which no net teardown reaches) — the close/switch
/// boundary must end the task instead of letting it live out its fetch
/// budget against a closed workspace.
#[test]
fn a_workspace_reset_aborts_the_running_file_fetches() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut st = crate::tests::plain_state();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _hold = tx;
        std::future::pending::<()>().await;
    });
    st.files.fetches.push(task.abort_handle());
    st.reset_workspace_state();
    rt.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("the fetch task must be aborted at the workspace boundary")
            .expect_err("the sender drops with the aborted future, unused");
    });
    assert!(st.files.fetches.is_empty(), "the handle list is cleared");
}

/// An online lower-named seat that claims every share whole and stays
/// silent must not starve a want: the sharer answers the repeat, once.
#[test]
fn a_silent_elected_holder_does_not_starve_a_repeated_piece_want() {
    use super::test_support::{presence_fixture, T};
    let mut st = presence_fixture();
    if let Some(r) = st.replica.as_mut() {
        r.member = "cid".to_string();
    }
    for w in &mut st.session.workspaces {
        for m in &mut w.members {
            if m.name == "ada" {
                m.last_seen = T;
            }
        }
    }
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("a");
    std::fs::write(&path, b"minutes").expect("write the shared file");
    let id = molt_core::MessageId([5u8; 16]);
    let mut msg = molt_core::ChatMessage::text(id, "cid", "a share", crate::now_secs());
    msg.file = Some(molt_core::FileMeta {
        name: "a".to_string(),
        size: 7,
        kind: "PDF".to_string(),
        modified: 100,
        available: true,
        checksum: "ab".repeat(32),
        key_b64: "AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM=".to_string(),
        pieces: 1,
        root: String::new(),
    });
    let env = st.make_env("cid".to_string(), molt_core::WorkspaceEvent::Chat(msg));
    st.apply(&env);
    st.files.share_paths.insert(id, path.clone());
    st.files
        .mirror
        .status
        .insert("ada".to_string(), vec![molt_core::MirrorHold { id, held: 1, of: 1 }]);
    let (ident, _) = st.share_identity(&id).expect("the share");
    assert_eq!(st.mirror_holders().get(&id).cloned(), Some(vec!["ada".to_string(), "cid".to_string()]));

    assert!(st.piece_source_if_elected(&id, &ident).is_none(), "ada is elected first");
    if let Some(w) = st.files.piece_wants.get_mut(&id) {
        w.0 -= 20 * 60;
    }
    assert_eq!(
        st.piece_source_if_elected(&id, &ident),
        Some((path, false)),
        "a repeated want reaches the sharer"
    );
    assert!(st.piece_source_if_elected(&id, &ident).is_none(), "one fallback answer per window");
}
