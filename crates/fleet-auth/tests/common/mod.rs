// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared mechanics for fleet-auth integration tests.

use std::sync::Arc;

use axum::response::Response;
use fleet_auth::{KeyStore, SessionConfig, SessionKey, SessionState};

pub async fn body_string(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

pub fn session_state(store: KeyStore, config: SessionConfig) -> (SessionState, Arc<SessionKey>) {
    let session_key = Arc::new(SessionKey::generate());
    let state = SessionState::new(store, Arc::clone(&session_key), Arc::new(config)).unwrap();
    (state, session_key)
}
