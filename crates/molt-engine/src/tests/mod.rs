// SPDX-License-Identifier: GPL-3.0-or-later

//! The engine's unit tests over the actor (`State`) and the public
//! `WalletHandle` surface, one file per concern (review E8); the shared
//! fixtures live in [`support`].

mod support;
pub(crate) use support::{plain_state, tiny_bmp_header};

mod chat_tests;
mod founding_tests;
mod governance_tests;
mod join_tests;
mod recovery_tests;
mod session_tests;
mod workspace_tests;
