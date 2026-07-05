// SPDX-License-Identifier: GPL-3.0-or-later

//! Probe for the native file picker plumbing: mimics exactly how the GUI
//! invokes it (a `tokio::runtime::Handle::spawn` from a non-runtime thread,
//! like the Slint callback on the main thread) so runtime-context panics in
//! zbus/ashpd reproduce headlessly:
//!
//! ```sh
//! cargo run -p molt-ui --example pickerprobe
//! ```
//!
//! The dialog (if it opens) is cancelled by the 6 s timeout.

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    // main thread has NO tokio context — same as the Slint event loop
    let handle = rt.handle().clone();
    let task = handle.spawn(async {
        let picked = tokio::time::timeout(
            std::time::Duration::from_secs(6),
            rfd::AsyncFileDialog::new().pick_file(),
        )
        .await;
        match picked {
            Err(_) => println!("PROBE: dialog opened, timed out cleanly (no panic)"),
            Ok(None) => println!("PROBE: dialog cancelled (no panic)"),
            Ok(Some(f)) => println!("PROBE: picked {:?} (no panic)", f.file_name()),
        }
    });
    rt.block_on(task).expect("probe task must not panic");
    println!("PROBE OK");
}
