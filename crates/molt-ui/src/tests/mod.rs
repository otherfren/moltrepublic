// SPDX-License-Identifier: GPL-3.0-or-later
//! The crate's unit tests, one file per module under test, plus the
//! headless GUI tests (`gui/`). Every file starts with `use super::*;`:
//! this prelude re-exports the crate the way the tests saw it when they
//! lived in lib.rs.

mod channels;
mod chat_log;
mod gui;
mod i18n;
mod images;
mod labels;
mod mirror;
mod net_tor;
mod relays;
mod ritual;
mod settings;
mod surfaces;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use molt_core::relay::{RelayBlock, RelayKind, RelayStatus};
use molt_core::{
    ChannelInfo, ChannelRef, ChatMessage, Command, GroupConfig, MessageId, ProposalId,
    ProposalState, ProposalView, Reply, SessionScope, SessionSettings, SessionView, Surface,
};
use molt_engine::WalletHandle;
use slint::{Model, ModelRc, VecModel};

use crate::actions::relays::relay_add_check;
use crate::app::Ctx;
use crate::channels::*;
use crate::chat_log::*;
use crate::i18n::*;
use crate::images::*;
use crate::labels::*;
use crate::mirror::*;
use crate::net_tor::*;
use crate::settings::*;
use crate::surfaces::*;
use crate::wiki_bridge::*;
use crate::*;
