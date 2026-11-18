// SPDX-License-Identifier: GPL-3.0-or-later
//! The spawner seam (`docs/web_gui.md` §6.2): what a callback hands to
//! [`Ctx::spawn`] runs to its end, on whatever the target runs futures on.

use super::*;
use crate::app::Ctx;

fn ctx(rt: &tokio::runtime::Runtime, root: &std::path::Path, ui: &AppWindow) -> Ctx {
    let (w, _) = node_with_chat(root);
    Ctx {
        spawner: rt.handle().clone().into(),
        wallet: w.into(),
        weak: ui.as_weak(),
        last_settings: Arc::new(Mutex::new(None)),
        chat_ui: Arc::new(Mutex::new(ChatUiState::default())),
    }
}

/// A spawned future is driven past a suspension point to completion.
#[test]
fn ctx_spawn_runs_a_future_to_completion() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let ui = AppWindow::new().expect("headless window");
    let cx = ctx(&rt, tmp.path(), &ui);

    let (go, gate) = futures_channel::oneshot::channel::<u32>();
    let (done, finished) = futures_channel::oneshot::channel::<u32>();
    cx.spawn(async move {
        let seed = gate.await.expect("the gate opens");
        let _ = done.send(seed + 1);
    });
    go.send(41).expect("the task waits on the gate");
    let got = rt.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(10), finished).await });
    assert_eq!(got.expect("in time").expect("the task ended"), 42);
}

/// A spawned future reaches the engine through the context it was given.
#[test]
fn a_spawned_future_reaches_the_engine() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let ui = AppWindow::new().expect("headless window");
    let cx = ctx(&rt, tmp.path(), &ui);

    let (done, finished) = futures_channel::oneshot::channel();
    let w = cx.wallet.clone();
    cx.spawn(async move {
        let _ = done.send(matches!(w.execute(Command::ReadSession).await, Ok(Reply::Session(_))));
    });
    let got = rt.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(10), finished).await });
    assert_eq!(got.expect("in time"), Ok(true));
}

/// Work handed off the UI thread comes back with its result.
#[test]
fn off_thread_work_returns_its_result() {
    let rt = rt();
    let got = rt.block_on(crate::native::off_thread(|| 6 * 7));
    assert_eq!(got, Some(42));
    let lost = rt.block_on(crate::native::off_thread(|| -> u32 { panic!("the worker dies") }));
    assert_eq!(lost, None, "a dead worker is an absent result, not a hang");
}

/// Every shared source line (comments cut) that `hit` matches, as
/// `(file relative to src, line)`; `native.rs` and the tests are exempt.
fn shared_lines(hit: fn(&str) -> bool) -> Vec<(String, usize)> {
    fn scan(src: &std::path::Path, dir: &std::path::Path, hit: fn(&str) -> bool, hits: &mut Vec<(String, usize)>) {
        for entry in std::fs::read_dir(dir).expect("src is readable") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n != "tests") {
                    scan(src, &path, hit, hits);
                }
            } else if path.extension().is_some_and(|e| e == "rs") && path.file_name().is_some_and(|n| n != "native.rs") {
                let text = std::fs::read_to_string(&path).expect("a source file");
                let name = path.strip_prefix(src).expect("under src").to_string_lossy().replace('\\', "/");
                for (no, line) in text.lines().enumerate() {
                    if hit(line.split("//").next().unwrap_or_default()) {
                        hits.push((name.clone(), no + 1));
                    }
                }
            }
        }
    }
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    scan(&src, &src, hit, &mut hits);
    hits
}

/// Shared code names no runtime: `tokio` appears in `native.rs` and the
/// tests only, so a browser build swaps one module.
#[test]
fn only_the_native_module_names_tokio() {
    let hits = shared_lines(|code| code.contains("tokio::") || code.contains("spawn_blocking"));
    assert!(hits.is_empty(), "tokio outside native.rs: {hits:?}");
}

/// Shared code reaches the disk, threads, child processes, the clocks,
/// the clipboard and the file dialogs through the platform seam only
/// (`docs/web_gui.md` §6.3): most of these compile on wasm32 and fail at
/// run time. Nothing is left.
#[test]
fn the_os_sites_outside_native_are_the_listed_ones() {
    const OS: [&str; 10] = [
        "std::fs",
        "std::thread",
        "std::process",
        "std::env",
        "Instant::now",
        "SystemTime",
        "Utc::now",
        "Local::now",
        "arboard::",
        "rfd::",
    ];
    let mut per_file = std::collections::BTreeMap::<String, usize>::new();
    for (file, _) in shared_lines(|code| OS.iter().any(|os| code.contains(os))) {
        *per_file.entry(file).or_default() += 1;
    }
    assert!(per_file.is_empty(), "an OS call in shared code: route it through the platform seam: {per_file:?}");
}

/// The lint that holds the same rule by resolved path (`docs/web_gui.md`
/// §8.3) reads one list in both crates, and the names the scan above
/// looks for are on it.
#[test]
fn both_window_crates_carry_the_same_disallowed_list() {
    let ui = include_str!("../../../clippy.toml");
    let kit = include_str!("../../../../molt-ui-kit/clippy.toml");
    assert_eq!(ui, kit, "molt-ui and molt-ui-kit disallow the same calls");
    for path in [
        "std::time::Instant::now",
        "std::time::SystemTime::now",
        "std::thread::spawn",
        "std::process::Command::new",
        "std::env::current_dir",
        "std::fs::read",
        "std::fs::write",
        "tokio::spawn",
        "tokio::task::spawn_blocking",
        "tokio::time::sleep",
        "arboard::Clipboard::new",
        "rfd::AsyncFileDialog::new",
    ] {
        assert!(ui.contains(&format!("path = \"{path}\"")), "{path} is disallowed");
    }
}

/// The native entry shows its config file as a full path: a relative one
/// is read against the working directory, a full one is left alone.
#[test]
fn a_relative_config_path_is_read_against_the_working_directory() {
    let cwd = std::env::current_dir().expect("a working directory");
    assert_eq!(crate::native::absolute(std::path::Path::new("config.toml")), cwd.join("config.toml"));
    let full = cwd.join("elsewhere").join("config.toml");
    assert_eq!(crate::native::absolute(&full), full);
}

/// The crates a browser build cannot have, as shared code still names
/// them, per file (`docs/web_gui.md` §6.7). Pure helpers come from
/// `molt-core`, platform calls go through the seam (§6.3) and the invite
/// parsers went behind an engine read (§6.4); what is left is the native
/// engine handle. The list only shrinks.
#[test]
fn the_higher_crate_uses_outside_native_are_the_listed_ones() {
    let mut per_file = std::collections::BTreeMap::<String, usize>::new();
    for (file, _) in shared_lines(|code| {
        ["molt_engine::", "molt_storage::", "molt_mcp::", "molt_config::"].iter().any(|krate| code.contains(krate))
    }) {
        *per_file.entry(file).or_default() += 1;
    }
    let listed = [("engine.rs", 1)]; // WalletHandle
    let listed: std::collections::BTreeMap<String, usize> = listed.iter().map(|(f, n)| (f.to_string(), *n)).collect();
    assert_eq!(per_file, listed, "a higher crate in shared code: pure helpers live in molt-core");
}

/// The native platform's clocks: the wall clock is past this code's
/// writing, the monotonic one never runs backwards.
#[test]
fn the_native_clocks_tell_time() {
    let platform = crate::platform::platform();
    assert!(platform.unix_now() > 1_790_000_000, "{}", platform.unix_now());
    let utc = u64::try_from(crate::platform::utc_now().timestamp()).expect("after 1970");
    assert!(utc.abs_diff(platform.unix_now()) <= 2, "one clock behind both");
    let first = platform.monotonic();
    let rt = rt();
    rt.block_on(platform.sleep(std::time::Duration::from_millis(20)));
    assert!(platform.monotonic() >= first + std::time::Duration::from_millis(20));
}

/// A minted token is a secret of real length and never the same twice.
#[test]
fn the_native_token_is_fresh_each_time() {
    let platform = crate::platform::platform();
    let (one, two) = (platform.random_token().expect("a token"), platform.random_token().expect("a token"));
    assert!(one.len() >= 32, "{}", one.len());
    assert_ne!(one, two);
}

/// A picked file comes back as its bytes, a missing one as an error; a
/// file the engine wrote here is read in place with its version stamp.
#[test]
fn the_native_platform_reads_files_as_bytes() {
    let platform = crate::platform::platform();
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("a.bin");
    std::fs::write(&path, b"one").expect("write");
    let name = path.display().to_string();
    let rt = rt();
    assert_eq!(rt.block_on(platform.read_file(name.clone())).expect("read"), b"one");
    assert!(rt.block_on(platform.read_file(format!("{name}.missing"))).is_err());
    assert_eq!(platform.local_bytes(&name).as_deref(), Some(&b"one"[..]));
    assert_eq!(platform.local_stamp(&name).map(|(len, _)| len), Some(3));
    assert_eq!(platform.local_bytes(&format!("{name}.missing")), None);
    assert_eq!(platform.local_stamp(&format!("{name}.missing")), None);
}

/// A burst of alerts plays once: the next one is due 400 ms later.
#[test]
fn an_alert_is_due_again_after_the_debounce() {
    use crate::alerts::alert_due;
    use std::time::Duration;
    assert!(alert_due(None, Duration::ZERO), "the first alert plays");
    assert!(!alert_due(Some(Duration::from_secs(5)), Duration::from_millis(5399)));
    assert!(alert_due(Some(Duration::from_secs(5)), Duration::from_millis(5400)));
}

/// The alert sample is a complete 44.1 kHz mono 16-bit WAV.
#[test]
fn the_alert_sample_is_a_wav_file() {
    for kind in ["bell", "chime", "pop"] {
        let wav = crate::alerts::alert_wav(kind);
        assert_eq!(&wav[..4], b"RIFF", "{kind}");
        assert_eq!(&wav[8..16], b"WAVEfmt ", "{kind}");
        assert_eq!(u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]), 44_100, "{kind}");
        let data = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]);
        assert_eq!(usize::try_from(data).ok(), Some(wav.len() - 44), "{kind}");
        assert!(data > 0, "{kind}");
    }
}
