// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared display-name contract for saved queries.

/// Trim outer whitespace, preserving interior spacing, Unicode and punctuation.
/// Unsafe display characters are rejected before trimming. This shares the
/// field-name policy, including its refusal of invisible format characters and joiners.
pub fn normalize(name: &str) -> Result<&str, &'static str> {
    if name.chars().any(crate::sanitize::is_unsafe_display_char) {
        return Err("Name must not contain control or invisible formatting characters.");
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
            "e\u{301} café 日本語",
            "🙂 👍🏽 🇯🇵 ✈\u{fe0f}",
        ] {
            assert_eq!(super::normalize(&format!("  {name}  ")), Ok(name));
        }
        for name in [
            "",
            "  ",
            "a\nb",
            "\tname",
            "name\0",
            "name\u{7f}",
            "\u{200b}",
            "a\u{202e}b",
            "a\u{200c}b",
            "👩\u{200d}💻",
            "a\u{feff}b",
            "a\u{00ad}b",
        ] {
            assert!(super::normalize(name).is_err(), "{name:?}");
        }
    }
}
