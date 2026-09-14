// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{env, path::PathBuf, process::Command};

pub fn prepare() {
    let target = env::var("TARGET").expect("Cargo sets TARGET");
    if !matches!(
        target.as_str(),
        "x86_64-unknown-linux-gnu"
            | "aarch64-unknown-linux-gnu"
            | "x86_64-apple-darwin"
            | "aarch64-apple-darwin"
    ) {
        // The parser is also used by the SPA and on non-product targets.
        // Those consumers must not acquire a native query-engine dependency.
        return;
    }

    println!("cargo:rerun-if-env-changed=DUCKDB_DOWNLOAD_LIB");
    assert!(
        !env::var("DUCKDB_DOWNLOAD_LIB").is_ok_and(|value| {
            !matches!(value.to_ascii_lowercase().as_str(), "" | "0" | "false")
        }),
        "unchecked DuckDB downloads are disabled; unset DUCKDB_DOWNLOAD_LIB or set it to 0"
    );
    println!("cargo:rerun-if-env-changed=DUCKDB_NO_PKG_CONFIG");
    // libduckdb-sys probes pkg-config even with DUCKDB_LIB_DIR set. Its
    // PKG_CONFIG_PATH override still permits fallback to host .pc files, whose
    // link search path could precede ours. pkg-config disables that probe on
    // variable presence, including empty and non-UTF-8 values.
    assert!(
        env::var_os("DUCKDB_NO_PKG_CONFIG").is_some(),
        "host DuckDB pkg-config selection must be disabled; run Cargo from the checkout, \
         or use DUCKDB_NO_PKG_CONFIG=1 cargo ... when invoking --manifest-path or install --path \
         from outside it (also required with DUCKDB_LIB_DIR)"
    );
    println!("cargo:rerun-if-env-changed=DUCKDB_LIB_DIR");
    println!("cargo:rerun-if-env-changed=DUCKDB_STATIC");
    assert!(
        !env::var("DUCKDB_STATIC").is_ok_and(|value| value != "0"),
        "Trawl requires the verified shared DuckDB runtime; unset DUCKDB_STATIC"
    );
    let source = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    // OUT_DIR is <target-root>[/<triple>]/<profile>/build/<crate>/out.
    // Derive paths from it so --target-dir, custom profiles and cross builds
    // work even when CARGO_TARGET_DIR is absent from the environment.
    let profile = out.ancestors().nth(3).expect("Cargo OUT_DIR layout");
    let deps = profile.join("deps");
    let cache = profile
        .parent()
        .unwrap()
        .join("duckdb-runtime-cache")
        .join(&target);
    let explicit_runtime = env::var_os("DUCKDB_LIB_DIR");
    let runtime = explicit_runtime
        .as_ref()
        .map_or_else(|| out.join("duckdb"), PathBuf::from);
    let script = source.join("scripts/release/distribution.py");
    let mut command = Command::new("python3");
    command.arg(&script);
    if explicit_runtime.is_some() {
        // An operator-supplied dependency directory is an input. Verify its
        // existing files without creating, repairing or downloading into it.
        command.args(["verify", "--runtime"]).arg(&runtime);
    } else {
        command
            .args(["prepare", "--output"])
            .arg(&runtime)
            .arg("--cache")
            .arg(&cache);
    }
    command
        .arg("--source")
        .arg(&source)
        .args(["--target", &target])
        .arg("--deps")
        .arg(&deps);
    assert!(
        command
            .status()
            .expect("native builds require Python 3.11+ and curl")
            .success(),
        "failed to prepare the checksum-verified DuckDB runtime"
    );

    // libduckdb-sys compiles its bindings without resolving -lduckdb. All
    // native engine consumers and core parity tests get this search path at
    // their final link. Cargo and bin/trawld-dev load the copy in profile/deps.
    println!("cargo:rustc-link-search=native={}", runtime.display());
    let library = if target.contains("apple") {
        "libduckdb.dylib"
    } else {
        "libduckdb.so"
    };
    for path in [
        source.join("Cargo.lock"),
        script,
        source.join("scripts/release/duckdb-runtime.json"),
        source.join("scripts/release/duckdb-LICENSE"),
        runtime.join(library),
        deps.join(library),
        if explicit_runtime.is_some() {
            runtime
        } else {
            cache
        },
    ] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
