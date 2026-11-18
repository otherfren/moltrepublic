// SPDX-License-Identifier: GPL-3.0-or-later
//! The engine as the GUI holds it (`docs/web_gui.md` §6.1): the engine's
//! own handle in the native window, a remote one behind the web door.
//! Both answer the same two verbs, so nothing above this file knows which
//! it has.

use molt_core::{Command, Event, MoltError, Reply};
#[cfg(not(target_arch = "wasm32"))]
use crate::native::EngineEvents;
#[cfg(not(target_arch = "wasm32"))]
use molt_engine::WalletHandle;
use molt_ui_kit::client::{NotRotated, RemoteEvents, RemoteHandle};

pub(crate) use molt_ui_kit::client::NoEvent;

/// The GUI's engine handle.
#[derive(Clone)]
pub enum Engine {
    /// The engine in this process. A browser build has none.
    #[cfg(not(target_arch = "wasm32"))]
    Native(WalletHandle),
    /// An engine behind the web door.
    Remote(RemoteHandle),
}

#[cfg(not(target_arch = "wasm32"))]
impl From<WalletHandle> for Engine {
    fn from(handle: WalletHandle) -> Self {
        Engine::Native(handle)
    }
}

impl From<RemoteHandle> for Engine {
    fn from(handle: RemoteHandle) -> Self {
        Engine::Remote(handle)
    }
}

impl Engine {
    /// Run one command.
    pub async fn execute(&self, cmd: Command) -> Result<Reply, MoltError> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Engine::Native(h) => h.execute(cmd).await,
            Engine::Remote(h) => h.execute(cmd).await,
        }
    }

    /// Make `key` the seat key. Behind the web door the server then
    /// closes every session of the old key, this one included; the
    /// connection follows onto `key`, and the key the server welcomes
    /// next is the answer (`docs/web_gui.md` §6.5).
    pub(crate) async fn rotate_seat(&self, key: String) -> Result<(), NotRotated> {
        let write = Command::PatchSettings { patch: serde_json::json!({ "mcp_token": key }) };
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Engine::Native(h) => h.execute(write).await.map(drop).map_err(NotRotated::Refused),
            Engine::Remote(h) => h.rotate(write, key).await,
        }
    }

    /// What the process serving a REMOTE engine said about itself in its
    /// welcome (`docs/web_gui.md` §6.6). `None` for the engine in this
    /// process (`run_app` is handed those facts) and before the welcome.
    pub(crate) fn remote_caps(&self) -> Option<molt_core::wire::Caps> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Engine::Native(_) => None,
            Engine::Remote(h) => h.welcome().map(|w| w.caps),
        }
    }

    /// Whether the engine runs elsewhere: its paths are then not this
    /// machine's, and file content crosses as bytes (`docs/web_gui.md` §7).
    pub(crate) fn is_remote(&self) -> bool {
        matches!(self, Engine::Remote(_))
    }

    /// The engine's events. A remote engine has ONE stream per connection.
    pub(crate) fn subscribe(&self) -> Events {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Engine::Native(h) => Events::Native(EngineEvents(h.subscribe())),
            Engine::Remote(h) => Events::Remote(h.subscribe()),
        }
    }
}

/// The engine's event stream.
pub(crate) enum Events {
    #[cfg(not(target_arch = "wasm32"))]
    Native(EngineEvents),
    Remote(RemoteEvents),
}

impl Events {
    /// The next event; `Lagged` means "re-read everything".
    pub(crate) async fn recv(&mut self) -> Result<Event, NoEvent> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Events::Native(rx) => rx.recv().await,
            Events::Remote(rx) => rx.recv().await,
        }
    }
}
