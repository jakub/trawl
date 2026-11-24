// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::Write;

use fleet_auth::SessionKey;

use crate::error::AdminError;

pub fn run() -> Result<(), AdminError> {
    let key = SessionKey::generate();
    let encoded = key.to_base64url();
    let mut out = std::io::stdout().lock();
    out.write_all(encoded.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use fleet_auth::SessionKey;

    #[test]
    fn generated_key_encodes_to_43_base64url_chars() {
        for _ in 0..16 {
            let encoded = SessionKey::generate().to_base64url();
            let s = encoded.as_str();
            assert_eq!(s.len(), 43, "expected 43 chars, got {} in {s:?}", s.len());
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "non-base64url char in {s:?}"
            );
        }
    }
}
