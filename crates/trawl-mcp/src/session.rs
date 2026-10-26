// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What one MCP session has seen of each investigation (ADR-0059).
//!
//! The live board is pull, both ways: the MCP server holds a `seen_seq`
//! per investigation per session and sends it with every attached call
//! and every append, and trawld answers with the entries other keys
//! changed after it. One stdio session is one process, so the map lives in
//! [`crate::TrawlServer`]. Nothing here is stored at trawld: two sessions
//! on one key each see every entry once.
//!
//! The rules, which [`crate::tools::investigation`] and
//! [`crate::tools::search`] apply:
//!
//! - A call sends the session's `seen_seq` for the investigation, or none
//!   the first time the session touches it. trawld then answers no
//!   activity, and the session starts counting from the call's own write
//!   (for an attached search, the `step_seq` trawld answers): what came
//!   before is the board, which `read` prints.
//! - The session counts an entry as seen only once a reply printed it:
//!   `seen` moves to the `updated_seq` of the last entry the reply printed,
//!   never past an entry the response budget dropped, and only once the
//!   reply fitted its budget. A refused or failed call, and a reply that
//!   does not fit, move nothing.
//! - A read that starts at or before `seen` prints every entry from there,
//!   so it moves `seen` too; one that starts later leaves a gap and moves
//!   nothing. When the read printed every entry trawld sent and trawld
//!   holds none past them, `seen` moves to the read's `through_seq`, so a
//!   read of an empty board moves it as well; otherwise to the last entry
//!   printed.
//! - `seen` only grows.
//!
//! Calls that touch investigations, an attached search among them, run one
//! at a time per session: each holds [`Seen::lock`] from the `seen_seq` it
//! sends until it has recorded what its reply printed, so two calls in
//! flight never print the same entry. A search with no investigation
//! touches nothing here and takes no lock.

use std::collections::HashMap;

use tokio::sync::{Mutex, MutexGuard};
use trawl_api::investigations::InvestigationId;

/// The `seen_seq` of each investigation this session has touched.
#[derive(Debug, Default)]
pub struct Seen {
    map: Mutex<HashMap<InvestigationId, u64>>,
}

impl Seen {
    /// The map, held until the guard drops.
    pub async fn lock(&self) -> SeenGuard<'_> {
        SeenGuard(self.map.lock().await)
    }
}

/// [`Seen`], held by one call.
#[derive(Debug)]
pub struct SeenGuard<'a>(MutexGuard<'a, HashMap<InvestigationId, u64>>);

impl SeenGuard<'_> {
    /// The `seen_seq` to send for `id`; `None` before the session touched
    /// it.
    #[must_use]
    pub fn get(&self, id: &InvestigationId) -> Option<u64> {
        self.0.get(id).copied()
    }

    /// Count the entries of `id` through `seq` as seen. `seen` only grows.
    pub fn advance(&mut self, id: &InvestigationId, seq: u64) {
        let seen = self.0.entry(id.clone()).or_insert(seq);
        *seen = (*seen).max(seq);
    }

    /// Start counting `id` from `seq` if the session has not touched it;
    /// leave it as it is otherwise.
    pub fn touch(&mut self, id: &InvestigationId, seq: u64) {
        self.0.entry(id.clone()).or_insert(seq);
    }

    /// After a reply showed the entries of `id` changed after `since`,
    /// through `through`: move `seen` when the read left no gap, that is,
    /// when it started at or before `seen`, or when the session had not
    /// touched `id`.
    pub fn printed(&mut self, id: &InvestigationId, since: u64, through: Option<u64>) {
        let Some(through) = through else {
            return;
        };
        match self.get(id) {
            Some(seen) if since > seen => {}
            _ => self.advance(id, through),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> InvestigationId {
        InvestigationId::parse("0123456789abcdef0123456789abcdef").unwrap()
    }

    #[tokio::test]
    async fn seen_only_grows_and_a_read_with_a_gap_moves_nothing() {
        let seen = Seen::default();
        let mut guard = seen.lock().await;
        assert_eq!(guard.get(&id()), None);
        guard.touch(&id(), 4);
        guard.touch(&id(), 9);
        assert_eq!(guard.get(&id()), Some(4));
        guard.advance(&id(), 7);
        guard.advance(&id(), 6);
        assert_eq!(guard.get(&id()), Some(7));
        guard.printed(&id(), 8, Some(12));
        assert_eq!(guard.get(&id()), Some(7), "8 > 7 leaves a gap");
        guard.printed(&id(), 3, Some(12));
        assert_eq!(guard.get(&id()), Some(12));
        guard.printed(&id(), 0, None);
        assert_eq!(guard.get(&id()), Some(12));
    }
}
