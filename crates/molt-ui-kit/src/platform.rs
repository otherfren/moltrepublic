// SPDX-License-Identifier: GPL-3.0-or-later
//! The platform seam (`docs/web_gui.md` §6.3): everything the window takes
//! from the machine it runs on, behind one trait. The native window's
//! implementation sits in `molt-ui` (`native.rs`); a browser build installs
//! its own before the window starts.
//!
//! Every future is `Send`, so the window's shared code stays free of
//! per-target bounds. A browser implementation whose own futures are not
//! `Send` runs them on its one thread and answers through a channel.

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;

/// What a platform call hands back when the answer takes a while.
pub type Task<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// One file-open dialog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilePick {
    /// The one offered file type: its label and the extensions it covers.
    /// `None` offers every file.
    pub filter: Option<(String, Vec<String>)>,
}

/// Bytes to hand the user as a file: a save dialog here, a download in a
/// browser.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Save {
    /// The offered file name.
    pub name: String,
    /// The file's content.
    pub bytes: Vec<u8>,
    /// Refuse a destination that is a link instead of writing through it.
    pub refuse_link: bool,
}

/// The marker inside [`too_large`]'s error.
#[derive(Debug)]
struct TooLarge;

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("too large")
    }
}

impl std::error::Error for TooLarge {}

/// What [`Platform::read_file`] answers for a file it refuses unread
/// because it is over what one transfer may carry (a browser knows the
/// size before it reads).
#[must_use]
pub fn too_large() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, TooLarge)
}

/// Whether `error` is [`too_large`]'s.
#[must_use]
pub fn is_too_large(error: &std::io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<TooLarge>())
}

/// What the window takes from the machine it runs on.
pub trait Platform: Send + Sync + 'static {
    /// Put `text` on the system clipboard; `false` when it did not get
    /// there (a browser may refuse). The write starts with the call, on
    /// the UI thread: a caller with no use for the answer drops it.
    fn copy(&self, text: String) -> Task<bool>;

    /// The clipboard's text; `None` when it holds none or may not be read.
    /// Called on the UI thread.
    fn paste(&self) -> Task<Option<String>>;

    /// Play the alert `kind`, detached. `wav` renders its sound as a WAV
    /// file's bytes, for a platform that plays samples.
    fn play_alert(&self, kind: &str, wav: fn(&str) -> Vec<u8>);

    /// The wall clock: unix seconds.
    fn unix_now(&self) -> u64;

    /// The monotonic clock: time since a fixed, arbitrary origin.
    fn monotonic(&self) -> Duration;

    /// Resolve after `time`, holding no thread.
    fn sleep(&self, time: Duration) -> Task<()>;

    /// A fresh random secret (an API key, a file nonce); `None` when the
    /// platform's random source failed.
    fn random_token(&self) -> Option<String>;

    /// Ask for a folder, starting at `start` when that is one. `None`:
    /// cancelled, or a platform without folders (a browser).
    fn pick_folder(&self, start: String) -> Task<Option<String>>;

    /// Ask for a file to open. The answer NAMES the file for
    /// [`Platform::read_file`]: its path where there are paths. Whatever
    /// follows its last `/` or `\` is the file's own name.
    fn pick_file(&self, pick: FilePick) -> Task<Option<String>>;

    /// Ask where an engine on THIS machine should write a file offered as
    /// `name`. `None`: cancelled, or a platform with no such paths.
    fn pick_save(&self, name: String) -> Task<Option<String>>;

    /// The bytes of a file [`Platform::pick_file`] named. A file refused
    /// unread for its size answers [`too_large`].
    fn read_file(&self, file: String) -> Task<std::io::Result<Vec<u8>>>;

    /// Hand `save` to the user. `Ok(Some(where))` names where it went,
    /// `Ok(None)` is a cancelled dialog.
    fn save_file(&self, save: Save) -> Task<std::io::Result<Option<String>>>;
}

static INSTALLED: OnceLock<Box<dyn Platform>> = OnceLock::new();

/// Install the process's platform. `false`: one is installed already and
/// stays.
pub fn install(platform: Box<dyn Platform>) -> bool {
    INSTALLED.set(platform).is_ok()
}

/// The installed platform, if there is one.
#[must_use]
pub fn installed() -> Option<&'static dyn Platform> {
    INSTALLED.get().map(AsRef::as_ref)
}

/// The installed platform; the first call without one installs `default`.
pub fn get_or_install(default: impl FnOnce() -> Box<dyn Platform>) -> &'static dyn Platform {
    INSTALLED.get_or_init(default).as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(u64);

    impl Platform for Fixed {
        fn copy(&self, _text: String) -> Task<bool> {
            Box::pin(async { true })
        }
        fn paste(&self) -> Task<Option<String>> {
            Box::pin(async { None })
        }
        fn play_alert(&self, _kind: &str, _wav: fn(&str) -> Vec<u8>) {}
        fn unix_now(&self) -> u64 {
            self.0
        }
        fn monotonic(&self) -> Duration {
            Duration::ZERO
        }
        fn sleep(&self, _time: Duration) -> Task<()> {
            Box::pin(async {})
        }
        fn random_token(&self) -> Option<String> {
            None
        }
        fn pick_folder(&self, _start: String) -> Task<Option<String>> {
            Box::pin(async { None })
        }
        fn pick_file(&self, _pick: FilePick) -> Task<Option<String>> {
            Box::pin(async { None })
        }
        fn pick_save(&self, _name: String) -> Task<Option<String>> {
            Box::pin(async { None })
        }
        fn read_file(&self, _file: String) -> Task<std::io::Result<Vec<u8>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn save_file(&self, _save: Save) -> Task<std::io::Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }
    }

    #[test]
    fn a_size_refusal_is_told_from_any_other_read_error() {
        assert!(is_too_large(&too_large()));
        assert!(!is_too_large(&std::io::Error::other("no such file")));
        assert!(!is_too_large(&std::io::Error::from(std::io::ErrorKind::InvalidInput)));
    }

    /// The first platform stays: a later install is refused and a default
    /// is never built over it. (One test: the slot is the process's.)
    #[test]
    fn the_first_installed_platform_stays() {
        assert!(install(Box::new(Fixed(7))));
        assert!(!install(Box::new(Fixed(8))), "a second install is refused");
        let got = get_or_install(|| Box::new(Fixed(9)));
        assert_eq!(got.unix_now(), 7);
        assert_eq!(installed().map(Platform::unix_now), Some(7));
    }
}
