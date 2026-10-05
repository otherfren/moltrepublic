// SPDX-License-Identifier: GPL-3.0-or-later

//! The reader side: combine answers, decrypt, never store (plan stage S4).

use molt_core::{MoltError, Reply};

impl crate::State {
    pub(crate) fn cmd_vault_read(&mut self, _secret_id: String) -> Result<Reply, MoltError> {
        Err(MoltError::FeatureDisabled("vault"))
    }
}
