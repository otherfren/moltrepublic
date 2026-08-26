// SPDX-License-Identifier: GPL-3.0-or-later
//! Alert sounds: the sound-name/index map, the own-echo gate, and a
//! pure-Rust WAV synthesized once per process and handed to the system
//! player (no compiled audio stack - the pure-Rust posture stands).

use std::sync::{Arc, Mutex};

use molt_core::SessionSettings;

use crate::AppWindow;

/// Map an alert-sound name to its ComboBox index (none/bell/chime/pop).
pub(crate) fn sound_index(s: &str) -> i32 {
    match s {
        "bell" => 1,
        "chime" => 2,
        "pop" => 3,
        _ => 0,
    }
}

/// Map a ComboBox index back to an alert-sound name.
pub(crate) fn sound_name(i: i32) -> String {
    match i {
        1 => "bell",
        2 => "chime",
        3 => "pop",
        _ => "none",
    }
    .to_string()
}

/// The last time an alert actually played — a debounce so a reconnect
/// catch-up of hundreds of queued messages cannot spawn a player storm.
static LAST_ALERT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// The shared own-echo gate of the chat and vote alerts: play the configured
/// sound unless the acting member IS the local one. The comparison runs on
/// the Slint thread because `node_member` is a UI property; the sound name is
/// read from the last APPLIED settings, so an unsaved draft never changes
/// behavior.
pub(crate) fn alert_unless_own(
    last_settings: &Arc<Mutex<Option<SessionSettings>>>,
    pick: impl Fn(&SessionSettings) -> String,
    weak: &slint::Weak<AppWindow>,
    by: molt_core::MemberId,
) {
    let sound = last_settings
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(pick))
        .unwrap_or_default();
    let weak2 = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak2.upgrade() else { return };
        if ui.get_node_member() != by.as_str() {
            play_alert(&sound);
        }
    });
}

/// Play a short alert sound, fire-and-forget. The sample is synthesized in
/// pure Rust (a tiny WAV, cached in the temp dir) and handed to the system
/// player — pw-play/paplay/aplay, runtime-detected, silently a no-op when
/// none exists. Deliberately NO compiled audio stack: cpal/rodio would pull
/// ALSA's C bindings, and the pure-Rust posture stands (CLAUDE.md).
///
/// Total-review hardening: (1) at most one alert per 400 ms (a message
/// burst plays once, not hundreds of times); (2) ALL work — the first-play
/// WAV synthesis and the player spawn — runs on a detached thread, never
/// the caller's UI/runtime thread; (3) the spawned player is REAPED (its
/// `wait()` runs on that thread) so no zombies accumulate.
pub(crate) fn play_alert(kind: &str) {
    if kind == "none" || kind.is_empty() {
        return;
    }
    {
        let now = std::time::Instant::now();
        let mut last = match LAST_ALERT.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if last.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_millis(400)) {
            return;
        }
        *last = Some(now);
    }
    let kind = kind.to_string();
    std::thread::spawn(move || {
        // under the per-user runtime dir (0700) when there is one, else the
        // shared temp dir — and never a file this process did not write:
        // a pre-planted file (pids are enumerable) would feed the player
        // attacker bytes (review F8). One random tag per process start.
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .filter(|d| d.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("molt-alert-{}-{kind}.wav", alert_nonce()));
        if !path.exists() && write_alert_wav(&path, &kind).is_err() {
            return;
        }
        for player in ["pw-play", "paplay", "aplay"] {
            match std::process::Command::new(player)
                .arg(&path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    let _ = child.wait(); // reap — no zombie
                    return;
                }
                Err(_) => continue,
            }
        }
    });
}

/// Synthesize one alert as a 44.1 kHz mono 16-bit WAV: a few decaying
/// sine partials per kind — bell (bright fifth), chime (rising triad),
/// pop (short thump).
/// A per-process-start random tag for the alert files: unguessable, unlike
/// the pid.
fn alert_nonce() -> &'static str {
    static NONCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NONCE.get_or_init(|| molt_config::random_token().unwrap_or_else(|_| "0".to_string()))
}

/// Create `path` as a NEW file — never through one that already exists
/// (a planted file or symlink there is not ours to write).
fn write_new_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    std::io::Write::write_all(&mut f, bytes)
}

fn write_alert_wav(path: &std::path::Path, kind: &str) -> std::io::Result<()> {
    let (freqs, dur): (&[f32], f32) = match kind {
        "bell" => (&[880.0, 1318.5], 0.35),
        "chime" => (&[523.25, 659.25, 783.99], 0.5),
        _ => (&[220.0, 440.0], 0.12), // pop
    };
    let rate = 44_100u32;
    let n = (dur * rate as f32) as usize;
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let env = (-6.0 * t / dur).exp();
        let mut v = 0.0f32;
        for (k, f) in freqs.iter().enumerate() {
            // chime arpeggiates: each partial enters a beat later
            let start = if kind == "chime" { k as f32 * 0.12 } else { 0.0 };
            if t >= start {
                v += ((t - start) * f * std::f32::consts::TAU).sin() * env;
            }
        }
        let v = (v / freqs.len() as f32 * 0.4 * f32::from(i16::MAX)) as i16;
        samples.push(v);
    }
    let data_len = u32::try_from(samples.len() * 2).unwrap_or(0);
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    write_new_file(path, &wav)
}
