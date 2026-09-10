// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Source guard for the removed Settings placeholder, ADR-0025.

use std::path::Path;

fn check_source(dir: &Path) {
    for entry in std::fs::read_dir(dir).expect("read UI source directory") {
        let path = entry.expect("read UI source entry").path();
        if path.is_dir() {
            check_source(&path);
        } else {
            let text = std::fs::read_to_string(&path).expect("read UI source file");
            for forbidden in ["coming soon", "SettingsPlaceholder", "mod placeholder;"] {
                assert!(
                    !text.contains(forbidden),
                    "{} still contains {forbidden:?}",
                    path.display(),
                );
            }
        }
    }
}

#[test]
fn settings_placeholder_is_removed_from_ui_source() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    assert!(!src.join("pages/placeholder.rs").exists());
    check_source(&src);
}
