// SPDX-License-Identifier: GPL-3.0-or-later
//! The engine as the GUI holds it (`docs/web_gui.md` §6.1): the engine's
//! own handle in the native window, a remote one behind the web door.
//! Both answer the same two verbs, so nothing above this file knows which
//! it has.

use molt_core::{Command, Event, MoltError, Reply};
use molt_engine::WalletHandle;
use molt_ui_kit::client::{RemoteEvents, RemoteHandle};
use tokio::sync::broadcast::error::RecvError;

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

    /// The engine's events. A remote engine has ONE stream per connection.
    pub(crate) fn subscribe(&self) -> Events {
        match self {
            Engine::Native(h) => Events::Native(h.subscribe()),
            Engine::Remote(h) => Events::Remote(h.subscribe()),
        }
    }
}

/// The engine's event stream.
pub(crate) enum Events {
    Native(tokio::sync::broadcast::Receiver<Event>),
    Remote(RemoteEvents),
}

impl Events {
    /// The next event; `Lagged` means "re-read everything".
    pub(crate) async fn recv(&mut self) -> Result<Event, NoEvent> {
        match self {
            Events::Native(rx) => rx.recv().await.map_err(|e| match e {
                RecvError::Lagged(_) => NoEvent::Lagged,
                RecvError::Closed => NoEvent::Closed,
            }),
            Events::Remote(rx) => rx.recv().await,
        }
    }
}
