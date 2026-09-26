// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generated secrets, and the one way a secret reaches a container.
//!
//! Every secret the CLI generates comes from the OS RNG into a buffer that
//! is zeroed on drop, and has a `Debug` that shows no byte of it. It leaves
//! the process only on the stdin of a sealed docker call: [`put_file`]
//! writes it to a 0400 file in a service's volume, owned by the user that
//! reads it. No secret is ever an argument, an environment entry, a label,
//! or a host file.

use std::fmt;
use std::time::Duration;

use rand::RngCore as _;
use zeroize::Zeroizing;

use super::compose::Service;
use super::docker::{Args, Docker, DockerError, Sensitivity};

/// Random bytes behind each generated password.
const SECRET_BYTES: usize = 32;

/// A `compose run` that writes one file creates the network and volumes
/// on first use, so it gets longer than a probe.
const PUT_FILE_TIMEOUT: Duration = Duration::from_secs(120);

/// `n` random bytes from the OS RNG, as lowercase hex.
pub fn random_hex(n: usize) -> Zeroizing<String> {
    let mut bytes = Zeroizing::new(vec![0u8; n]);
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex(&bytes)
}

fn hex(bytes: &[u8]) -> Zeroizing<String> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = Zeroizing::new(String::with_capacity(bytes.len() * 2));
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// A generated password: 64 lowercase hex characters, so it needs no
/// quoting in SQL, a pgpass line, or anywhere else.
pub struct HexSecret(Zeroizing<String>);

impl HexSecret {
    pub fn generate() -> Self {
        Self(random_hex(SECRET_BYTES))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for HexSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HexSecret(<redacted>)")
    }
}

/// The two owner roles' passwords.
#[derive(Debug)]
pub struct DbPasswords {
    pub fleet: HexSecret,
    pub trawl: HexSecret,
}

impl DbPasswords {
    pub fn generate() -> Self {
        Self {
            fleet: HexSecret::generate(),
            trawl: HexSecret::generate(),
        }
    }
}

/// trawl-web's cookie AEAD key: 32 raw bytes, the exact contents
/// `[web] cookie_secret_path` reads (not base64).
pub struct CookieKey(Zeroizing<[u8; 32]>);

impl CookieKey {
    pub fn generate() -> Self {
        let mut key = Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(key.as_mut());
        Self(key)
    }

    pub fn expose(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl fmt::Debug for CookieKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CookieKey(<redacted>)")
    }
}

/// A volume that [`put_file`] writes into, and the user that owns and
/// reads the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The `postgres` volume, as the `postgres` user.
    Postgres,
    /// The `trawld` volume, as the image's `trawl` user.
    Trawld,
    /// The `web` volume, as the image's `trawl` user.
    Web,
}

impl Target {
    fn service(self) -> Service {
        match self {
            Self::Postgres => Service::Postgres,
            Self::Trawld => Service::Trawld,
            Self::Web => Service::Web,
        }
    }

    /// `--user`, for an image whose default user is not the reader.
    fn user(self) -> Option<&'static str> {
        match self {
            Self::Postgres => Some("postgres"),
            Self::Trawld | Self::Web => None,
        }
    }
}

/// The in-container writer. Its only argument is the path; the bytes
/// arrive on stdin behind one line holding their length.
///
/// It creates the parent directory owner-only, clears temporary files a
/// killed run left, writes the bytes to a temporary file created 0400
/// (umask 377), refuses a short write (the CLI was killed mid-pipe), and
/// renames the file into place. The file is owned by the user the
/// container runs as, which is the user that reads it.
const WRITE: &str = r#"set -eu
case "$1" in /*) ;; *) exit 64 ;; esac
umask 077
mkdir -p -- "${1%/*}"
rm -f -- "$1".tmp.*
IFS= read -r n
case "$n" in ''|*[!0-9]*) exit 65 ;; esac
umask 377
t="$1.tmp.$$"
head -c "$n" > "$t"
if [ "$(wc -c < "$t")" -ne "$n" ]; then rm -f -- "$t"; exit 66; fi
mv -f -- "$t" "$1""#;

/// The `compose run` arguments that write `path` in `target`'s volume.
fn put_file_args(target: Target, path: &str) -> Args {
    let mut args = Args::new().args(["run", "--rm", "--no-deps", "-T"]);
    if let Some(user) = target.user() {
        args = args.args(["--user", user]);
    }
    args.args(["--entrypoint", "/bin/sh"])
        .arg(target.service().name())
        .args(["-c", WRITE, "sh", path])
}

/// Whether `path` is one the writer may take: absolute, and nothing but
/// plain segments of `[A-Za-z0-9._-]`.
fn is_plain_absolute(path: &str) -> bool {
    path.strip_prefix('/').is_some_and(|rest| {
        !rest.is_empty()
            && rest.split('/').all(|segment| {
                !segment.is_empty()
                    && segment != "."
                    && segment != ".."
                    && segment
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            })
    })
}

/// Write `bytes` to `path` inside `target`'s volume, mode 0400, owned by
/// the reading user, replacing any earlier file atomically.
///
/// The call is sealed: its output never reaches an error, and the bytes
/// travel only on stdin.
///
/// # Panics
/// When `path` is not a plain absolute path. Every caller passes one of
/// the constants in [`super::compose`].
pub async fn put_file(
    docker: &Docker,
    target: Target,
    path: &'static str,
    bytes: &[u8],
) -> Result<(), DockerError> {
    assert!(is_plain_absolute(path), "not a plain absolute path: {path}");
    let mut stdin = Zeroizing::new(Vec::with_capacity(bytes.len() + 24));
    stdin.extend_from_slice(format!("{}\n", bytes.len()).as_bytes());
    stdin.extend_from_slice(bytes);
    let args = docker.compose(put_file_args(target, path));
    docker
        .capture(&args, Some(&stdin), Sensitivity::Sealed, PUT_FILE_TIMEOUT)
        .await
        .map(drop)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    use super::*;
    use crate::trial::compose;
    use crate::trial::docker::tests::stub;

    #[test]
    fn hex_is_lowercase_and_two_digits_per_byte() {
        assert_eq!(&*hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        let secret = HexSecret::generate();
        assert_eq!(secret.expose().len(), 64);
        assert!(
            secret
                .expose()
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        );
        assert_ne!(secret.expose(), HexSecret::generate().expose());
        assert_eq!(random_hex(16).len(), 32);
    }

    #[test]
    fn debug_shows_no_secret() {
        let passwords = DbPasswords::generate();
        let key = CookieKey::generate();
        let debug = format!("{passwords:?} {key:?}");
        assert!(!debug.contains(passwords.fleet.expose()), "{debug}");
        assert!(!debug.contains(passwords.trawl.expose()), "{debug}");
        assert!(debug.contains("<redacted>"));
        assert_eq!(key.expose().len(), 32);
    }

    #[test]
    fn every_secret_path_is_plain_and_absolute() {
        for path in [
            compose::PG_SUPERUSER_PASSWORD,
            compose::TRAWLD_TOML,
            compose::PGPASS,
            compose::WEB_TOML,
            compose::WEB_COOKIE,
            compose::WEB_CA,
        ] {
            assert!(is_plain_absolute(path), "{path}");
        }
        for bad in [
            "",
            "/",
            "relative/file",
            "/a/../b",
            "/a/./b",
            "/a//b",
            "/a/b/",
            "/a b",
            "/a/$x",
            "/a/`x`",
        ] {
            assert!(!is_plain_absolute(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_writer_runs_as_the_reader_with_the_path_as_its_only_argument() {
        let args = put_file_args(Target::Postgres, compose::PG_SUPERUSER_PASSWORD);
        let shown = args.display();
        assert!(
            shown.starts_with(
                "docker run --rm --no-deps -T --user postgres --entrypoint /bin/sh postgres -c "
            ),
            "{shown}"
        );
        assert!(
            shown.ends_with(" sh /var/lib/postgresql/trial/superuser.password"),
            "{shown}"
        );

        let args = put_file_args(Target::Web, compose::WEB_COOKIE);
        let words = args.argv();
        assert!(!words.iter().any(|a| a == "--user"));
        let service = words.iter().position(|a| a == "trawl-web").unwrap();
        assert_eq!(words[service + 1..].len(), 4, "-c WRITE sh <path>");
        assert_eq!(words.last().unwrap(), compose::WEB_COOKIE);
    }

    /// A stub docker that runs the real writer script locally, with the
    /// path moved under the test's temp dir.
    fn local_writer(root: &std::path::Path) -> (tempfile::TempDir, Docker) {
        stub(&format!(
            r#"while [ "$1" != "-c" ]; do shift; done
script="$2"; path="$4"
exec /bin/sh -c "$script" sh "{root}$path""#,
            root = root.display()
        ))
    }

    #[tokio::test]
    async fn put_file_writes_0400_atomically_and_replaces() {
        let root = tempfile::tempdir().unwrap();
        let (_stub, docker) = local_writer(root.path());
        let key = CookieKey::generate();
        put_file(&docker, Target::Web, compose::WEB_COOKIE, key.expose())
            .await
            .unwrap();

        let file = root
            .path()
            .join(compose::WEB_COOKIE.trim_start_matches('/'));
        assert_eq!(std::fs::read(&file).unwrap(), key.expose());
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o400);
        assert_eq!(meta.uid(), nix::unistd::geteuid().as_raw());
        let parent = std::fs::metadata(file.parent().unwrap()).unwrap();
        assert_eq!(parent.permissions().mode() & 0o777, 0o700);

        // Raw bytes survive, including newlines and NULs.
        let raw = b"\n\0line\nno-trailing".to_vec();
        put_file(&docker, Target::Web, compose::WEB_COOKIE, &raw)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), raw);
        let leftovers: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, [std::ffi::OsString::from("web.cookie")]);
    }

    /// A pipe cut short (the CLI killed mid-write) never replaces the
    /// file: the length line catches the short write.
    #[tokio::test]
    async fn a_short_write_leaves_the_old_file() {
        let root = tempfile::tempdir().unwrap();
        let (_stub, docker) = local_writer(root.path());
        put_file(&docker, Target::Trawld, compose::PGPASS, b"old\n")
            .await
            .unwrap();

        // Claim 10 bytes, send 3.
        let args = docker.compose(put_file_args(Target::Trawld, compose::PGPASS));
        let err = docker
            .capture(
                &args,
                Some(b"10\nnew"),
                Sensitivity::Sealed,
                PUT_FILE_TIMEOUT,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exit status 66"), "{err}");
        let file = root.path().join(compose::PGPASS.trim_start_matches('/'));
        assert_eq!(std::fs::read(&file).unwrap(), b"old\n");
        assert_eq!(
            std::fs::read_dir(file.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[tokio::test]
    async fn a_failed_write_reports_no_secret() {
        let (_stub, docker) = stub("cat >/dev/null; echo \"leaked: $*\" >&2; exit 1");
        let secret = HexSecret::generate();
        let err = put_file(
            &docker,
            Target::Postgres,
            compose::PG_SUPERUSER_PASSWORD,
            secret.expose().as_bytes(),
        )
        .await
        .unwrap_err();
        let text = format!("{err} {err:?}");
        assert!(!text.contains(secret.expose()), "{text}");
        assert!(!text.contains("leaked"), "{text}");
    }
}
