// SPDX-License-Identifier: GPL-3.0-or-later
//! The engine as the GUI holds it (`docs/web_gui.md` §6.1): the engine's
//! own handle in the native window, a remote one behind the web door.
//! Both answer the same two verbs, so nothing above this file knows which
//! it has.

use molt_core::{Command, Event, MoltError, Reply};
use crate::native::EngineEvents;
use molt_engine::WalletHandle;
use molt_ui_kit::client::{RemoteEvents, RemoteHandle};

pub(crate) use molt_ui_kit::client::NoEvent;

/// The GUI's engine handle.
#[derive(Clone)]
pub enum Engine {
    /// The engine in this process.
    Native(WalletHandle),
    /// An engine behind the web door.
    Remote(RemoteHandle),
}

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
            Engine::Native(h) => h.execute(cmd).await,
            Engine::Remote(h) => h.execute(cmd).await,
        }
    }

    /// What the process serving a REMOTE engine said about itself in its
    /// welcome (`docs/web_gui.md` §6.6). `None` for the engine in this
    /// process (`run_app` is handed those facts) and before the welcome.
    pub(crate) fn remote_caps(&self) -> Option<molt_core::wire::Caps> {
        match self {
            Engine::Native(_) => None,
            Engine::Remote(h) => h.welcome().map(|w| w.caps),
        }
    }

    /// The engine's events. A remote engine has ONE stream per connection.
    pub(crate) fn subscribe(&self) -> Events {
        match self {
            Engine::Native(h) => Events::Native(EngineEvents(h.subscribe())),
            Engine::Remote(h) => Events::Remote(h.subscribe()),
        }
    }
}

/// The engine's event stream.
pub(crate) enum Events {
    Native(EngineEvents),
    Remote(RemoteEvents),
}

impl Events {
    /// The next event; `Lagged` means "re-read everything".
    pub(crate) async fn recv(&mut self) -> Result<Event, NoEvent> {
        match self {
            Events::Native(rx) => rx.recv().await,
            Events::Remote(rx) => rx.recv().await,
        }
    }
}
