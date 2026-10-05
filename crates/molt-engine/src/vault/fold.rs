// SPDX-License-Identifier: GPL-3.0-or-later

//! The folded vault base arriving off the actor (plan stage S5).

use molt_core::{MoltError, Reply};

impl crate::State {
    pub(crate) fn cmd_net_vault_base_fetched(&mut self, _bytes: Vec<u8>) -> Result<Reply, MoltError> {
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_net_vault_base_failed(&mut self) -> Result<Reply, MoltError> {
        Ok(Reply::Ack)
    }
}
