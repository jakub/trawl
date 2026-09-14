// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared display-name contract for saved queries.

/// Trim outer whitespace, preserving interior spacing, Unicode and punctuation.
/// Controls are rejected before trimming so tabs and newlines cannot disappear.
pub fn normalize(name: &str) -> Result<&str, &'static str> {
    if name.chars().any(char::is_control) {
        return Err("Name must not contain control characters.");
    }
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter a name.");
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    #[test]
    fn readable_names() {
        for name in [
            "Audit saved query",
            "a  b",
            "雪 / \"quoted\" \\ path",
            "../report",
        ] {
            assert_eq!(super::normalize(&format!("  {name}  ")), Ok(name));
        }
        for name in ["", "  ", "a\nb", "\tname", "name\0", "name\u{7f}"] {
            assert!(super::normalize(name).is_err(), "{name:?}");
        }
    }
}
