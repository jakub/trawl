// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Build script: ensure the SPA `dist/` directory exists before
//! `rust-embed` scans it at compile time.
//!
//! rust-embed fails at macro expansion if the target folder is missing
//! (fresh clones don't have a pre-built SPA). We create an empty dir
//! so the crate builds; running `cargo xtask build-web` populates it
//! with a real SPA bundle, and subsequent builds pick those up.

use std::fs;
use std::path::Path;

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("trawl-web-ui")
        .join("dist");

    if !dist.exists()
        && let Err(e) = fs::create_dir_all(&dist)
    {
        // Non-fatal: a missing `dist/` just means rust-embed sees no
        // assets, which the runtime handles by returning 404s on /*
        // (and the user sees an empty page). Worst case is a confusing
        // first-run UX, not a broken build.
        println!("cargo:warning=could not create {}: {e}", dist.display());
    }

    println!("cargo:rerun-if-changed={}", dist.display());
}
