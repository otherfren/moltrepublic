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
