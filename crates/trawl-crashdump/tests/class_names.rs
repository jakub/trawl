// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CI proof greps for readiness classes this crate can actually emit.
//!
//! `ci/crashdump-image.sh` decides whether the image is correct by grepping
//! trawld's own log line for `readiness="<class>"`. That coupling is invisible
//! to the compiler: rename a class here and the script keeps passing, because
//! every one of its assertions is a substring search that now searches for a
//! string nothing prints. The failure mode is the bad one, a green job that
//! checks nothing.
//!
//! So the class names are pinned twice. This test reads the script, collects
//! every class it names, and requires each to be a member of [`Status::ALL`].
//! It also pins `Status::ALL` itself, so a rename has to touch this file and
//! cannot slip through by renaming both sides in lockstep with nobody looking
//! at the CI script.

use std::path::{Path, PathBuf};

use trawl_crashdump::Status;

/// The CI script, relative to this crate's manifest directory.
const SCRIPT: &str = "../../ci/crashdump-image.sh";

/// What the script writes just before the class name.
const MARKER: &str = "readiness=\"";

fn script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(SCRIPT)
}

/// Every `readiness="<word>"` the script spells out, with its line number.
///
/// A `<word>` is a run of ASCII letters closed by the quote. An occurrence that
/// continues with anything else, a shell variable or a `sed` capture group, is
/// not a literal the script can be grepping for, so it is not a class name and
/// is skipped. The script spells all four out on purpose, which is what keeps
/// that leniency from hiding anything.
fn readiness_tokens(text: &str) -> Vec<(String, usize)> {
    let mut found = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let mut rest = line;
        while let Some(at) = rest.find(MARKER) {
            let tail = &rest[at + MARKER.len()..];
            let word: String = tail.chars().take_while(char::is_ascii_alphabetic).collect();
            if !word.is_empty() && tail[word.len()..].starts_with('"') {
                found.push((word, number + 1));
            }
            rest = tail;
        }
    }
    found
}

#[test]
fn ci_script_names_only_real_readiness_classes() {
    let path = script_path();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "cannot read {}: {err}\n\
             That script is the CI half of this contract. If it moved or was \
             deleted, this test moves or goes with it. It does not get to pass \
             because the file it guards is gone.",
            path.display()
        )
    });

    let tokens = readiness_tokens(&text);
    assert!(
        !tokens.is_empty(),
        "{} names no readiness class at all. Either the log field was renamed \
         (in which case this test's MARKER is stale) or the script stopped \
         asserting the verdict, which is most of what it exists to do.",
        path.display()
    );

    for (word, line) in tokens {
        assert!(
            Status::ALL.contains(&word.as_str()),
            "{}:{line} greps for readiness=\"{word}\", which is not a class \
             trawld can emit. The classes are {:?}.",
            path.display(),
            Status::ALL
        );
    }
}

#[test]
fn readiness_class_names_are_pinned() {
    assert_eq!(
        Status::ALL,
        ["ready", "denied", "indeterminate", "failed"],
        "A readiness class was renamed, added or removed. Every one of them is \
         a grep target in ci/crashdump-image.sh and a documented operator \
         action in docs/src/content/docs/reference/crash-dumps.md, so the \
         rename lands in all three places or not at all."
    );
}
