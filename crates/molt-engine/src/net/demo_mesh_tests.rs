// SPDX-License-Identifier: GPL-3.0-or-later

//! The demo-mesh seam.


/// The demo mesh is a **default-off test seam**: a freshly built state
/// (what every production spawner creates) wants no fake peers in the
/// session-only boot context; only the seam flag re-enables them.
#[test]
fn the_demo_mesh_seam_is_default_off() {
    let mut st = crate::tests::plain_state();
    assert!(
        !st.demo_mesh,
        "production default: the demo-mesh seam starts OFF"
    );
    assert!(
        !st.wants_demo_mesh(),
        "the boot context must not want fake peers without the seam"
    );
    st.demo_mesh = true;
    assert!(
        st.wants_demo_mesh(),
        "the test seam re-enables the session-only demo mesh"
    );
}
