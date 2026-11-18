// SPDX-License-Identifier: GPL-3.0-or-later
//! File content between the window and the engine (`docs/web_gui.md` §7).
//!
//! An engine in this process shares the window's disk, so a file crosses
//! as a PATH and the engine reads or writes it in place. An engine behind
//! the web door does not: there the content crosses as BYTES, through the
//! byte doors, and the platform hands it to the user or takes it from
//! them. Each function here is one flow with that one fork in it, so the
//! callers and everything after the fork are the same code on both.
//!
//! Pictures on display have no fork: both engines answer them through
//! `read_image_bytes`.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_core::wire::MAX_TRANSFER;
use molt_core::{ChannelRef, Command, MessageId, MoltError, Reply, TransferPhase};
use molt_ui_kit::platform::{Platform, Save};
use slint::ComponentHandle;

use crate::engine::Engine;
use crate::i18n::error_toast;
use crate::surfaces::ChatUiState;
use crate::{AppWindow, Strings};

/// The most a picture on display may weigh. Pictures ride a proposal, so
/// a real one is far below this; the cap only bounds the read.
const PICTURE_CAP: u64 = 8 * 1024 * 1024;

/// How often a running export is looked at again.
const EXPORT_POLL: Duration = Duration::from_millis(250);

/// Why a byte flow ended without its file.
#[derive(Debug)]
pub(crate) enum ByteError {
    /// The picked file did not read; carries its name.
    Unreadable(String),
    /// Over the ceiling one transfer through the door may carry.
    TooLarge,
    /// The engine refused.
    Engine(MoltError),
    /// The platform could not hand the file to the user.
    Save(String),
}

impl From<MoltError> for ByteError {
    fn from(e: MoltError) -> Self {
        // the door clamps every read to the ceiling and says so
        match &e {
            MoltError::BadPayload(t) if t.contains(" over the ") => ByteError::TooLarge,
            _ => ByteError::Engine(e),
        }
    }
}

/// How a download or an export to the user went.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Handed {
    /// The user has the file; where the platform says it went.
    Saved(String),
    /// The user closed the dialog.
    Cancelled,
    /// The engine is fetching the share; [`transfer_ended`] finishes it.
    Started,
    /// The engine made no file; its own report says why.
    NotMade,
}

/// The file's own name in what a picker answered: the part behind the
/// last separator of either kind (a browser's answer has no directory).
pub(crate) fn file_name(file: &str) -> &str {
    file.rsplit(['/', '\\']).next().unwrap_or(file)
}

/// Whether an export target names no directory: the engine then writes
/// into its download folder, and a window whose engine is elsewhere
/// fetches the blob from there. The engine's own reading, so the two
/// never disagree about where a file went.
pub(crate) use molt_core::is_bare_name;

fn over_ceiling(len: usize) -> bool {
    u64::try_from(len).map_or(true, |n| n > MAX_TRANSFER)
}

/// The picked file's bytes, for an engine that cannot read it in place.
async fn picked_bytes(platform: &dyn Platform, file: &str) -> Result<Vec<u8>, ByteError> {
    let read = platform.read_file(file.to_string()).await;
    let bytes = read.map_err(|_| ByteError::Unreadable(file_name(file).to_string()))?;
    if over_ceiling(bytes.len()) {
        return Err(ByteError::TooLarge);
    }
    Ok(bytes)
}

/// Put `bytes` into the engine's upload store; the id names them after.
async fn upload(engine: &Engine, name: &str, bytes: &[u8]) -> Result<String, ByteError> {
    if over_ceiling(bytes.len()) {
        return Err(ByteError::TooLarge);
    }
    let name = match file_name(name) {
        "" => "file",
        name => name,
    };
    Ok(molt_ui_kit::client::upload(|cmd| engine.execute(cmd), name, bytes).await?)
}

/// Share the file a picker named into `channel`.
pub(crate) async fn share_picked(
    engine: &Engine,
    platform: &dyn Platform,
    file: String,
    channel: ChannelRef,
) -> Result<(), ByteError> {
    if !engine.is_remote() {
        // the engine hashes the file where it lies: no copy, no ceiling
        engine.execute(Command::ShareFile { path: file, channel }).await?;
        return Ok(());
    }
    let bytes = picked_bytes(platform, &file).await?;
    let upload = upload(engine, &file, &bytes).await?;
    engine.execute(Command::ShareUpload { upload, channel }).await?;
    Ok(())
}

/// Give a picture proposal its picture: inline for the engine in this
/// process, as an upload for one behind the door (a text frame there
/// holds 1 MiB). `name` is the picture's file name.
pub(crate) async fn attach_picture(
    engine: &Engine,
    payload: &mut serde_json::Value,
    name: &str,
    bytes: Vec<u8>,
) -> Result<(), ByteError> {
    if engine.is_remote() {
        payload["upload"] = upload(engine, name, &bytes).await?.into();
    } else {
        use base64::Engine as _;
        payload["bytes_b64"] = base64::engine::general_purpose::STANDARD.encode(bytes).into();
    }
    Ok(())
}

/// A picture on display, by the `image` value its view carries.
pub(crate) async fn picture_bytes(engine: &Engine, key: String) -> Result<Vec<u8>, MoltError> {
    match engine.execute(Command::ReadImageBytes { key, cap: PICTURE_CAP }).await? {
        Reply::UploadBytes { bytes } => Ok(bytes),
        _ => Err(MoltError::Engine("unexpected reply".into())),
    }
}

/// What tells one state of the republic's pictures from the next: a
/// replaced picture keeps its file name, so the name alone would keep
/// showing the old one. Folds every applied picture change of the
/// Organization log.
pub(crate) fn pictures_version(applied: &[serde_json::Value]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for v in applied {
        let Some(op) = v.get("op").and_then(serde_json::Value::as_str).filter(|op| op.contains("image")) else {
            continue;
        };
        op.hash(&mut h);
        for key in ["member", "value", "bytes_b64"] {
            v.get(key).and_then(serde_json::Value::as_str).hash(&mut h);
        }
    }
    h.finish()
}

/// The pictures a window holds, by [`picture_key`]: each is read from the
/// engine once and again only when the pictures changed. `None`: the
/// engine refused that picture.
#[derive(Default)]
pub(crate) struct Pictures {
    held: HashMap<String, Option<Arc<Vec<u8>>>>,
}

/// The cache key of the picture `image` names at `version`
/// ([`pictures_version`]); "" for no picture.
pub(crate) fn picture_key(image: &str, version: u64) -> String {
    if image.is_empty() {
        String::new()
    } else {
        format!("{image}|{version:x}")
    }
}

/// The bytes behind every key in `wanted` (`(key, image)` pairs), read
/// through the engine where the window does not hold them yet. A picture
/// that did not read is absent; one the engine refused is not asked for
/// again under the same key.
pub(crate) async fn hold_pictures(
    engine: &Engine,
    chat_ui: &Arc<Mutex<ChatUiState>>,
    wanted: &[(String, String)],
) -> HashMap<String, Arc<Vec<u8>>> {
    hold_with(chat_ui, wanted, |image| picture_bytes(engine, image)).await
}

/// [`hold_pictures`] over any read.
pub(crate) async fn hold_with<F, Fut>(
    chat_ui: &Arc<Mutex<ChatUiState>>,
    wanted: &[(String, String)],
    read: F,
) -> HashMap<String, Arc<Vec<u8>>>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, MoltError>>,
{
    let mut held: HashMap<String, Option<Arc<Vec<u8>>>> = chat_ui
        .lock()
        .map(|st| {
            wanted.iter().filter_map(|(key, _)| Some((key.clone(), st.pictures.held.get(key)?.clone()))).collect()
        })
        .unwrap_or_default();
    for (key, image) in wanted {
        if key.is_empty() || held.contains_key(key) {
            continue;
        }
        match read(image.clone()).await {
            Ok(bytes) => held.insert(key.clone(), Some(Arc::new(bytes))),
            // the engine's own no (a republic that is not stored has no
            // picture to open): the same answer until the pictures change
            Err(MoltError::BadPayload(_)) => held.insert(key.clone(), None),
            // anything else may be a dropped connection: asked again
            Err(_) => None,
        };
    }
    let out = held.iter().filter_map(|(key, bytes)| Some((key.clone(), bytes.clone()?))).collect();
    if let Ok(mut st) = chat_ui.lock() {
        // what no row names any more goes: a replaced picture, a left seat
        st.pictures.held = held;
    }
    out
}

/// Read a share that is on the engine's disk and hand it to the user.
async fn hand_share(engine: &Engine, platform: &dyn Platform, id: MessageId, name: String) -> Result<Handed, ByteError> {
    let Reply::UploadBytes { bytes } = engine.execute(Command::ReadFileBytes { id, cap: MAX_TRANSFER }).await? else {
        return Err(ByteError::Engine(MoltError::Engine("unexpected reply".into())));
    };
    hand(platform, name, bytes).await
}

async fn hand(platform: &dyn Platform, name: String, bytes: Vec<u8>) -> Result<Handed, ByteError> {
    match platform.save_file(Save { name, bytes, refuse_link: false }).await {
        Ok(Some(dest)) => Ok(Handed::Saved(dest)),
        Ok(None) => Ok(Handed::Cancelled),
        Err(e) => Err(ByteError::Save(e.to_string())),
    }
}

/// Download the share `id`, offered to the user as `name`.
///
/// The engine in this process writes where the user points. One behind
/// the door fetches the share onto ITS disk first when it is not there
/// (phase one, the progress row), and the bytes then cross the door and
/// leave through the platform (phase two) - at once when the engine has
/// them, else when [`transfer_ended`] sees the fetch done.
pub(crate) async fn download(
    engine: &Engine,
    platform: &dyn Platform,
    chat_ui: &Arc<Mutex<ChatUiState>>,
    id: MessageId,
    name: String,
) -> Result<Handed, ByteError> {
    if !engine.is_remote() {
        let Some(dest) = platform.pick_save(name).await else {
            return Ok(Handed::Cancelled);
        };
        engine.execute(Command::DownloadFile { id, dest: Some(dest) }).await?;
        return Ok(Handed::Started);
    }
    let Reply::Uploads { uploads } = engine.execute(Command::ReadUploads).await? else {
        return Err(ByteError::Engine(MoltError::NoFile(id)));
    };
    let Some(share) = uploads.into_iter().find(|u| u.id == id) else {
        return Err(ByteError::Engine(MoltError::NoFile(id)));
    };
    if share.size > MAX_TRANSFER {
        return Err(ByteError::TooLarge);
    }
    if matches!(share.local.as_str(), "own" | "downloaded" | "mirrored") {
        return hand_share(engine, platform, id, name).await;
    }
    // noted BEFORE the fetch starts: its end may come at once
    let fresh = chat_ui.lock().is_ok_and(|mut st| st.to_user.insert(id, name).is_none());
    if let Err(e) = engine.execute(Command::DownloadFile { id, dest: None }).await {
        // a fetch an earlier click started keeps its note
        if fresh {
            if let Ok(mut st) = chat_ui.lock() {
                st.to_user.remove(&id);
            }
        }
        return Err(e.into());
    }
    Ok(Handed::Started)
}

/// A download's end as the engine reports it. `Some(name)`: this window
/// asked for the share and it is on the engine's disk now - phase two
/// ([`finish_download`]) is due. A failed or cancelled fetch is forgotten.
pub(crate) fn transfer_ended(chat_ui: &Arc<Mutex<ChatUiState>>, id: MessageId, phase: &TransferPhase) -> Option<String> {
    let mut st = chat_ui.lock().ok()?;
    match phase {
        TransferPhase::Done { .. } => st.to_user.remove(&id),
        TransferPhase::Failed { .. } | TransferPhase::Cancelled => {
            st.to_user.remove(&id);
            None
        }
        _ => None,
    }
}

/// Phase two of [`download`].
pub(crate) async fn finish_download(
    engine: &Engine,
    platform: &dyn Platform,
    id: MessageId,
    name: String,
) -> Result<Handed, ByteError> {
    hand_share(engine, platform, id, name).await
}

/// Export workspace `id` and hand the blob to the user as `name`: the
/// engine writes it into its download folder under that name, and the
/// window reads it back once the export reports done.
pub(crate) async fn export_to_user(
    engine: &Engine,
    platform: &dyn Platform,
    id: String,
    name: String,
    passphrase: String,
) -> Result<Handed, ByteError> {
    engine.execute(Command::ExportWorkspace { id: id.clone(), dest: name.clone(), passphrase }).await?;
    // the command is in: the first state that is not running is this
    // export's (or a later one's)
    let size = loop {
        let Reply::Session(sv) = engine.execute(Command::ReadSession).await? else {
            return Err(ByteError::Engine(MoltError::Engine("unexpected reply".into())));
        };
        let ex = &sv.export;
        if ex.running {
            platform.sleep(EXPORT_POLL).await;
            continue;
        }
        if ex.result != "ok" || ex.workspace != id {
            // the session's export note says why
            return Ok(Handed::NotMade);
        }
        break ex.bytes;
    };
    if size > MAX_TRANSFER {
        return Err(ByteError::TooLarge);
    }
    let Reply::UploadBytes { bytes } = engine.execute(Command::ReadExportBytes { cap: MAX_TRANSFER }).await? else {
        return Err(ByteError::Engine(MoltError::Engine("unexpected reply".into())));
    };
    hand(platform, name, bytes).await
}

/// The toast a failed byte flow shows.
pub(crate) fn byte_error_toast(ui: &AppWindow, e: &ByteError) -> slint::SharedString {
    let s = ui.global::<Strings>();
    match e {
        ByteError::Unreadable(name) => format!("\u{26a0} {} {name}", s.get_toast_file_unreadable()).into(),
        ByteError::TooLarge => format!("\u{26a0} {}", s.get_toast_too_large_web()).into(),
        ByteError::Engine(e) => error_toast(ui, e),
        ByteError::Save(why) => format!("\u{26a0} {why}").into(),
    }
}

/// Toast how handing a file to the user went; a start and a closed dialog
/// say nothing. Runs on the UI thread.
pub(crate) fn toast_handed(ui: &AppWindow, outcome: &Result<Handed, ByteError>) {
    match outcome {
        Ok(Handed::Saved(dest)) => {
            let done = ui.global::<Strings>().get_toast_dl_done();
            ui.invoke_show_toast(format!("{done} {dest}").into());
        }
        Ok(Handed::Cancelled | Handed::Started | Handed::NotMade) => {}
        Err(e) => ui.invoke_show_toast_error(byte_error_toast(ui, e)),
    }
}

/// [`toast_handed`] from a task.
pub(crate) fn toast_handed_later(weak: slint::Weak<AppWindow>, outcome: Result<Handed, ByteError>) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            toast_handed(&ui, &outcome);
        }
    });
}
