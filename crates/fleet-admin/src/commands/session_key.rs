// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `fleet-admin generate-session-key` — emit a fresh
//! `XChaCha20`-Poly1305 key in base64url form.

use std::io::Write;

use fleet_auth::SessionKey;

use crate::error::AdminError;

/// Generate a new session key and write it to stdout as base64url-no-pad.
///
/// Output is exactly one line: 43 characters of base64url, no terminator
/// other than the trailing newline `println!` adds. The plaintext is held
/// in a [`zeroize::Zeroizing`] string for the duration of the call.
//
// Returns `Result` for shape-parity with the other subcommand entry points
// (`migrate::run`, `keys::*`), which the top-level dispatch unifies into a
// single `Result<(), AdminError>` chain.
pub fn run() -> Result<(), AdminError> {
    let key = SessionKey::generate();
    let encoded = key.to_base64url();
    let mut out = std::io::stdout().lock();
    out.write_all(encoded.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
