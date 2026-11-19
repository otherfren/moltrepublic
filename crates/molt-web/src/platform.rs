// SPDX-License-Identifier: GPL-3.0-or-later
//! The browser's half of the platform seam (`docs/web_gui.md` §6.3).
//!
//! The seam's futures are `Send`; a browser's are not. Each call here
//! runs its JS future on the page's one thread and answers through a
//! channel ([`on_page`]). State (the audio context, the picked files)
//! lives in thread-locals for the same reason: the platform object itself
//! must be `Send + Sync`, and there is one thread.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::time::Duration;

use futures_channel::oneshot;
use molt_core::wire::MAX_TRANSFER;
use molt_ui_kit::platform::{too_large, FilePick, Platform, Save, Task};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};

use crate::rules;

/// The browser behind the platform seam.
pub(crate) struct WebPlatform;

/// Run `work` on the page's thread and answer through a channel, so the
/// seam's future is `Send`. `lost` is the answer if the work never ends
/// with one.
fn on_page<T: Send + 'static>(work: impl Future<Output = T> + 'static, lost: T) -> Task<T> {
    let (done, answer) = oneshot::channel();
    spawn_local(async move {
        drop(done.send(work.await));
    });
    Box::pin(async move { answer.await.unwrap_or(lost) })
}

/// A browser error as an I/O error (its message, where it has one).
fn js_error(value: &JsValue) -> io::Error {
    let text = value.dyn_ref::<js_sys::Error>().map(|e| String::from(e.message()));
    io::Error::other(text.or_else(|| value.as_string()).unwrap_or_else(|| "browser error".into()))
}

fn clipboard() -> Option<web_sys::Clipboard> {
    Some(web_sys::window()?.navigator().clipboard())
}

/// How many picked files wait for their read at most; the oldest goes.
const PICKS_HELD: usize = 8;

/// The files the user picked and the window has not read yet.
#[derive(Default)]
struct Picks {
    next: u64,
    held: VecDeque<(u64, web_sys::File)>,
}

impl Picks {
    fn keep(&mut self, file: web_sys::File) -> String {
        self.next = self.next.wrapping_add(1);
        if self.held.len() == PICKS_HELD {
            self.held.pop_front();
        }
        let named = rules::pick_name(self.next, &file.name());
        self.held.push_back((self.next, file));
        named
    }

    fn take(&mut self, pick: u64) -> Option<web_sys::File> {
        let at = self.held.iter().position(|(id, _)| *id == pick)?;
        self.held.remove(at).map(|(_, file)| file)
    }
}

thread_local! {
    static PICKS: RefCell<Picks> = RefCell::new(Picks::default());
    /// One audio context per page; a browser caps how many may exist.
    static AUDIO: RefCell<Option<web_sys::AudioContext>> = const { RefCell::new(None) };
}

fn audio() -> Result<web_sys::AudioContext, JsValue> {
    AUDIO.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(context) = slot.as_ref() {
            return Ok(context.clone());
        }
        let context = web_sys::AudioContext::new()?;
        *slot = Some(context.clone());
        Ok(context)
    })
}

/// Decode the WAV and play it once.
async fn play(wav: Vec<u8>) -> Result<(), JsValue> {
    let context = audio()?;
    // a context made before the user's first gesture starts suspended
    drop(context.resume());
    let encoded = js_sys::Uint8Array::from(wav.as_slice()).buffer();
    let decoded: web_sys::AudioBuffer = JsFuture::from(context.decode_audio_data(&encoded)?).await?.dyn_into()?;
    let source = context.create_buffer_source()?;
    source.set_buffer(Some(&decoded));
    source.connect_with_audio_node(&context.destination())?;
    source.start()
}

impl Platform for WebPlatform {
    fn copy(&self, text: String) -> Task<bool> {
        let written = clipboard().map(|c| JsFuture::from(c.write_text(&text)));
        // a refusal (no permission, no gesture) leaves the clipboard as it was
        on_page(
            async move {
                match written {
                    Some(written) => written.await.is_ok(),
                    None => false,
                }
            },
            false,
        )
    }

    fn paste(&self) -> Task<Option<String>> {
        let read = clipboard().map(|c| JsFuture::from(c.read_text()));
        on_page(
            async move {
                let text = read?.await.ok()?.as_string()?;
                (!text.is_empty()).then_some(text)
            },
            None,
        )
    }

    /// Samples through WebAudio: decoded from memory, so no URL the
    /// page's content policy would have to allow.
    fn play_alert(&self, kind: &str, wav: fn(&str) -> Vec<u8>) {
        let wav = wav(kind);
        // no sound (no device, the tab never clicked) is not a fault to show
        spawn_local(async move { drop(play(wav).await) });
    }

    fn unix_now(&self) -> u64 {
        web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }

    fn monotonic(&self) -> Duration {
        thread_local! {
            static ORIGIN: web_time::Instant = web_time::Instant::now();
        }
        ORIGIN.with(web_time::Instant::elapsed)
    }

    fn sleep(&self, time: Duration) -> Task<()> {
        // setTimeout takes 32-bit milliseconds: a longer sleep ends early
        let millis = u32::try_from(time.as_millis()).unwrap_or(u32::MAX);
        on_page(gloo_timers::future::TimeoutFuture::new(millis), ())
    }

    fn random_token(&self) -> Option<String> {
        let mut bytes = [0u8; rules::TOKEN_BYTES];
        getrandom::getrandom(&mut bytes).ok()?;
        Some(rules::token_hex(&bytes))
    }

    /// A browser has no folders: server directories are typed (§7).
    fn pick_folder(&self, _start: String) -> Task<Option<String>> {
        Box::pin(async { None })
    }

    fn pick_file(&self, pick: FilePick) -> Task<Option<String>> {
        on_page(
            async move {
                let mut dialog = rfd::AsyncFileDialog::new();
                if let Some((label, extensions)) = &pick.filter {
                    dialog = dialog.add_filter(label.as_str(), extensions);
                }
                let file = dialog.pick_file().await?.inner().clone();
                Some(PICKS.with(|picks| picks.borrow_mut().keep(file)))
            },
            None,
        )
    }

    /// No path here is one an engine could write to.
    fn pick_save(&self, _name: String) -> Task<Option<String>> {
        Box::pin(async { None })
    }

    fn read_file(&self, file: String) -> Task<io::Result<Vec<u8>>> {
        let picked = rules::pick_id(&file).and_then(|pick| PICKS.with(|picks| picks.borrow_mut().take(pick)));
        on_page(
            async move {
                let picked = picked.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
                // measured, not read: 64 MiB is the most one transfer carries
                if rules::over_ceiling(picked.size(), MAX_TRANSFER) {
                    return Err(too_large());
                }
                let buffer = JsFuture::from(picked.array_buffer()).await.map_err(|e| js_error(&e))?;
                Ok(js_sys::Uint8Array::new(&buffer).to_vec())
            },
            Err(io::Error::other("the read never answered")),
        )
    }

    /// A download: the dialog offers the file as a link the user clicks,
    /// so it also works when the bytes arrive long after the click that
    /// asked for them (phase two of a download, an export).
    fn save_file(&self, save: Save) -> Task<io::Result<Option<String>>> {
        on_page(
            async move {
                let dialog = rfd::AsyncFileDialog::new().set_file_name(save.name.as_str());
                let Some(link) = dialog.save_file().await else {
                    return Ok(None);
                };
                link.write(&save.bytes).await?;
                Ok(Some(save.name))
            },
            Err(io::Error::other("the download never answered")),
        )
    }
}
