// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Running `trawld` as root inside an unprivileged user namespace, with a
//! directory bind-mounted read-only, for the tests that need a root run or a
//! read-only mount without real privilege.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The name that makes an unavailable user namespace a test failure rather
/// than a skip. CI sets it, so a privileged test that did not run there is
/// never counted as passing evidence.
pub const REQUIRE_USERNS: &str = "TRAWL_TEST_REQUIRE_USERNS";

/// Running `trawld` as root inside an unprivileged user namespace, the way
/// the doctor's root-run and read-only-mount tests do: `unshare --user
/// --map-root-user --mount`, which maps this test's uid to 0, so `trawld`
/// sees euid 0 and the files the test made as owned by root.
#[derive(Debug, Clone)]
pub struct Userns {
    unshare: PathBuf,
    mount: PathBuf,
    shell: PathBuf,
}

/// Inside the namespace: bind the directory `$2` onto itself read-only when
/// it is not empty, then exec the rest of the arguments from the same
/// working directory. `$1` is `mount`.
/// Any failure ends the run before the doctor starts, with a status the
/// doctor never exits with.
const USERNS_SCRIPT: &str = r#"set -e
mount="$1"
ro="$2"
shift 2
if [ -n "$ro" ]; then
    "$mount" --bind "$ro" "$ro"
    "$mount" -o remount,bind,ro "$ro"
    # A working directory opened before the mount still names the
    # writable tree; look it up again, through the read-only one.
    cd "$PWD"
fi
exec "$@"
"#;

impl Userns {
    /// The namespace tools, once a probe proved this host lets this user
    /// make the namespace, be root in it, and mount a directory read-only
    /// there; otherwise why not.
    ///
    /// # Errors
    /// The reason the namespace is unavailable.
    pub fn probe() -> Result<Self, String> {
        let find = |name: &str| {
            let path = std::env::var_os("PATH").unwrap_or_default();
            std::env::split_paths(&path)
                .chain(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from))
                .map(|dir| dir.join(name))
                .find(|candidate| candidate.is_file())
                .ok_or_else(|| format!("no {name} on PATH"))
        };
        let tools = Self {
            unshare: find("unshare")?,
            mount: find("mount")?,
            shell: find("sh")?,
        };
        let dir = tempfile::tempdir().map_err(|e| format!("a probe directory: {e}"))?;
        let probe = dir.path().join("probe");
        std::fs::write(&probe, b"probe").map_err(|e| format!("a probe file: {e}"))?;
        // As root in the namespace, a write to the read-only bind mount must
        // fail; `id -u` must say 0.
        let output = tools
            .command(Some(dir.path()))
            .args([
                tools.shell.as_os_str(),
                OsStr::new("-c"),
                OsStr::new(
                    r#"[ "$(id -u)" = 0 ] || exit 90; if ( : > "$1/probe" ) 2>/dev/null; then exit 91; fi"#,
                ),
                OsStr::new("sh"),
                dir.path().as_os_str(),
            ])
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .output()
            .map_err(|e| format!("{} does not run: {e}", tools.unshare.display()))?;
        match output.status.code() {
            Some(0) => {}
            Some(90) => return Err("the namespace's uid is not 0".to_owned()),
            Some(91) => return Err("the read-only bind mount took a write".to_owned()),
            _ => {
                return Err(format!(
                    "unshare --user --map-root-user --mount with a read-only bind mount \
                     failed ({}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        if std::fs::read(&probe).ok().as_deref() != Some(b"probe".as_slice()) {
            return Err("the read-only bind mount's file changed".to_owned());
        }
        Ok(tools)
    }

    /// [`Userns::probe`] for the test `test`: the tools, or `None` after
    /// printing `SKIPPED: ...` when the namespace is unavailable.
    ///
    /// # Panics
    /// When the namespace is unavailable and [`REQUIRE_USERNS`] is `1`.
    pub fn for_test(test: &str) -> Option<Self> {
        match Self::probe() {
            Ok(tools) => Some(tools),
            Err(why) if std::env::var_os(REQUIRE_USERNS).is_some_and(|v| v == "1") => {
                panic!("{test}: {REQUIRE_USERNS}=1, and a user namespace is unavailable: {why}")
            }
            Err(why) => {
                eprintln!("SKIPPED: {test}: a user namespace is unavailable here ({why})");
                None
            }
        }
    }

    /// `unshare` with an empty environment, except the loader's library
    /// path, which the shared-DuckDB test build needs, set to run
    /// [`USERNS_SCRIPT`]'s arguments, with `read_only` bound read-only.
    pub fn command(&self, read_only: Option<&Path>) -> Command {
        let mut command = Command::new(&self.unshare);
        command.env_clear();
        for name in ["LD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .args(["--user", "--map-root-user", "--mount"])
            .arg(&self.shell)
            .args(["-c", USERNS_SCRIPT, "sh"])
            .arg(&self.mount)
            .arg(read_only.map_or_else(OsString::new, |dir| dir.as_os_str().to_owned()));
        command
    }
}
