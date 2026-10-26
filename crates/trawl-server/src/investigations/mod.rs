// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Investigations (ADR-0059): trawl's first installation-owned object, read
//! and written by grant, never by ownership.
//!
//! [`service`] is the capability, transport-free: list, create if absent,
//! read since a sequence, append a keep, a finding or a note, overwrite the
//! title, the question, the status or a finding's status, extend the
//! evidence retention, and delete. It checks the caller's permissions
//! before it looks anything up and validates every bound before the store
//! sees the request. `routes` adapts it to the HTTP routes under
//! `/api/v1/investigations`. The rows live in
//! [`crate::store::InvestigationStore`]. [`step`] is the step an attached
//! capture appends at admission and the completion that writes its outcome
//! when the work ends.

pub(crate) mod routes;
pub mod service;
pub mod step;
