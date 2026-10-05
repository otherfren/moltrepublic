// SPDX-License-Identifier: GPL-3.0-or-later

//! Vault payload files on the file plane, held by every seat (plan stage
//! S3a, spec §9.2).

use molt_core::{MoltError, Reply};

impl crate::State {
    pub(crate) fn cmd_net_vault_payload_fetched(
        &mut self,
        _hash: String,
        _bytes: Vec<u8>,
    ) -> Result<Reply, MoltError> {
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_net_vault_payload_failed(&mut self, _hash: String) -> Result<Reply, MoltError> {
        Ok(Reply::Ack)
    }
}
