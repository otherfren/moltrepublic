// SPDX-License-Identifier: GPL-3.0-or-later
//! The settings draft: MiB quotas, the two S3 targets, the folder browse.
//!
//! The two S3 targets (`docs_archive/storage/s3_buckets.md`): the byte quotas
//! are edited in MiB but stored in bytes, and the two targets share no
//! field on the way through the settings draft.

use super::*;

/// The workspace-folder browse dialog starts where the hand-editable
/// draft points ONLY when that (after the engine's own `~` expansion —
/// the config default is "~/…") is a real directory; anything else
/// (empty draft, typo, a file) must yield no start dir so the dialog
/// opens at its platform default instead of failing.
#[test]
fn ws_dir_browse_starts_at_the_draft_only_when_it_is_a_real_directory() {
    let dir = tempfile::tempdir().expect("create a temp directory");
    let dir_path = dir.path().display().to_string();
    assert_eq!(
        browse_start_dir(&dir_path),
        Some(dir.path().to_path_buf()),
        "an existing directory is a usable start dir"
    );
    // a "~" draft expands against $HOME exactly like the engine resolves
    // the setting — pinning the config default's "~/…" form to a REAL
    // start dir, not a literal "~" path that never exists
    let home = std::env::var_os("HOME").expect("HOME is set in the test env");
    assert_eq!(
        browse_start_dir("~"),
        Some(std::path::PathBuf::from(home)),
        "a tilde draft starts at the expanded home directory"
    );
    // a FILE is not a directory to start browsing in
    let file_path = dir.path().join("config.toml");
    std::fs::write(&file_path, b"x").expect("write a probe file");
    assert_eq!(browse_start_dir(&file_path.display().to_string()), None);
    assert_eq!(browse_start_dir(""), None, "empty draft → dialog default");
    assert_eq!(
        browse_start_dir(&format!("{dir_path}/definitely-missing")),
        None,
        "a stale/typoed draft → dialog default"
    );
}

/// A quota the operator wrote by hand in bytes must survive a settings
/// save that did not touch it - the MiB stepper is a VIEW of the value,
/// not a re-quantization of it.
#[test]
fn an_untouched_byte_quota_is_not_rounded_onto_the_mib_grid() {
    // rounded UP, so the displayed limit is never smaller than the real one
    assert_eq!(mib_label(500_000_000), "477");
    assert_eq!(
        mib_text_to_bytes("477", 500_000_000),
        500_000_000,
        "the field still shows 477 - keep the exact stored bytes"
    );
    // …but a real edit converts
    assert_eq!(mib_text_to_bytes("1000", 500_000_000), 1000 * 1024 * 1024);
    // 0 is "no limit" on both sides, and clearing one really clears it
    assert_eq!(mib_label(0), "0");
    assert_eq!(mib_text_to_bytes("0", 0), 0);
    assert_eq!(mib_text_to_bytes("0", 500_000_000), 0);
    // an emptied field means no limit; garbage keeps the stored value
    // rather than inventing one
    assert_eq!(mib_text_to_bytes("  ", 500_000_000), 0);
    assert_eq!(mib_text_to_bytes("-5", 500_000_000), 500_000_000);
    assert_eq!(mib_text_to_bytes("abc", 500_000_000), 500_000_000);
    // an absurd number saturates instead of wrapping
    assert_eq!(mib_text_to_bytes(&u64::MAX.to_string(), 0), u64::MAX);
}

/// Push the account and both buckets into a real headless window and read
/// the draft back: the two buckets stay distinct, and the quotas survive.
#[test]
fn both_buckets_round_trip_through_the_settings_draft() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let stored = SessionSettings {
        s3_endpoint: "https://backup.example.org".to_string(),
        s3_access_key: "BAK".to_string(),
        s3_secret_key: "bak-secret".to_string(),
        s3_bucket: "media-archive".to_string(),
        s3_max_bytes: 500_000_000,
        media_s3_bucket: "clips".to_string(),
        media_s3_max_bytes: 3 * 1024 * 1024 * 1024,
        ..SessionSettings::default()
    };
    apply_settings_fields(&ui, &stored);
    let draft = read_settings_draft(&ui, &stored);
    assert_eq!(draft.s3_endpoint, "https://backup.example.org");
    assert_eq!(draft.s3_bucket, "media-archive");
    assert_eq!(draft.media_s3_bucket, "clips");
    assert_eq!(
        draft.s3_access_key, "BAK",
        "one account: the credentials are not per bucket"
    );
    assert_eq!(
        draft.s3_max_bytes, 500_000_000,
        "the hand-written byte quota survives an untouched round trip"
    );
    assert_eq!(draft.media_s3_max_bytes, 3 * 1024 * 1024 * 1024);
    // and the form reports itself clean: an unedited draft must not make
    // the leave-guard fire
    assert!(
        !settings_draft_differs(&stored, &ui),
        "an untouched draft equals the stored settings"
    );
}
