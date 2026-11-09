// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generated secrets, and the one way a secret reaches a container.
//!
//! Every secret the CLI generates comes from the OS RNG into a buffer that
//! is zeroed on drop, and has a `Debug` that shows no byte of it. The trial
//! certificate's key is held the same way once it is encoded, but rcgen's
//! own copies are not zeroed ([`super::certificate`]). A secret leaves the
//! process only on the stdin of a sealed docker call: [`put_file`] writes
//! it to a 0400 file in a service's volume, owned by the user that reads
//! it. No secret is ever an argument, an environment entry, a label, or a
//! host file.
//!
//! [`read_file`] reads a public file back from a volume, as the same user,
//! and tells a missing file from a read that failed.

use std::fmt;
use std::time::Duration;

use rand::RngCore as _;
use zeroize::Zeroizing;

use super::compose::Service;
use super::docker::{Args, Docker, DockerError, Sensitivity};

/// Random bytes behind each generated password.
const SECRET_BYTES: usize = 32;

/// A `compose run` that reads or writes one file creates the network and
/// volumes on first use, so it gets longer than a probe.
const FILE_TIMEOUT: Duration = Duration::from_secs(120);

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
/// renames the file into place. A directory at the path fails the rename
/// (GNU `mv -T`; both images are Debian), and the temporary file is
/// removed. The file is owned by the user the container runs as, which is
/// the user that reads it.
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
mv -fT -- "$t" "$1" || { rm -f -- "$t"; exit 67; }"#;

/// The in-container reader. Its only argument is the path.
///
/// It prints one status line, then for a regular file the file's first
/// [`READ_LIMIT`] + 1 bytes, so a longer file reads as too long without
/// being read whole:
///
/// - `present`: a regular file, followed by its bytes.
/// - `not-a-file`: something else is at the path, such as a directory or a
///   link, dangling or not. It is not read.
/// - `absent`: nothing is at the path, and the nearest ancestor that exists
///   is a directory the reader can search, so nothing is hidden from it.
///
/// Anything else, such as an ancestor the reader cannot search, exits
/// non-zero. A failed read never reads as `absent`.
const READ: &str = r#"set -eu
case "$1" in /*) ;; *) exit 64 ;; esac
if [ -L "$1" ]; then echo not-a-file; exit 0; fi
if [ -f "$1" ]; then echo present; exec head -c 65537 -- "$1"; fi
if [ -e "$1" ]; then echo not-a-file; exit 0; fi
d="${1%/*}"
while [ -n "$d" ] && [ ! -e "$d" ] && [ ! -L "$d" ]; do d="${d%/*}"; done
d="${d:-/}"
if [ -L "$d" ] || [ ! -d "$d" ] || [ ! -x "$d" ]; then
  echo "cannot search $d" >&2
  exit 65
fi
echo absent"#;

/// The most bytes [`read_file`] returns of a file it reads whole.
pub const READ_LIMIT: u64 = 64 * 1024;

/// What [`read_file`] found at a path.
#[derive(Debug, PartialEq, Eq)]
pub enum VolumeFile {
    /// A regular file: its bytes, or its first [`READ_LIMIT`] + 1 bytes
    /// when it is longer than [`READ_LIMIT`].
    Present(Vec<u8>),
    /// Something other than a regular file, which was not read.
    NotAFile,
    /// Nothing is at the path.
    Absent,
}

/// The `compose run` arguments that run `script` with `path` as its only
/// argument in `target`'s volume, as the volume's reading user.
fn script_args(target: Target, script: &str, path: &str) -> Args {
    let mut args = Args::new().args(["run", "--rm", "--no-deps", "-T"]);
    if let Some(user) = target.user() {
        args = args.args(["--user", user]);
    }
    args.args(["--entrypoint", "/bin/sh"])
        .arg(target.service().name())
        .args(["-c", script, "sh", path])
}

/// The `compose run` arguments that write `path` in `target`'s volume.
fn put_file_args(target: Target, path: &str) -> Args {
    script_args(target, WRITE, path)
}

/// The `compose run` arguments that read `path` in `target`'s volume.
fn read_file_args(target: Target, path: &str) -> Args {
    script_args(target, READ, path)
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
        .capture(&args, Some(&stdin), Sensitivity::Sealed, FILE_TIMEOUT)
        .await
        .map(drop)
}

/// Read `path` inside `target`'s volume, as the user [`put_file`] writes
/// it as. For a public file only: the call quotes the reader's stderr in
/// its error.
///
/// [`VolumeFile::Absent`] comes only from the reader's own `absent` line,
/// never from empty output. A reader that fails, or prints no status line
/// this function knows, is an error.
///
/// # Panics
/// When `path` is not a plain absolute path. Every caller passes one of
/// the constants in [`super::compose`].
pub async fn read_file(
    docker: &Docker,
    target: Target,
    path: &'static str,
) -> Result<VolumeFile, DockerError> {
    assert!(is_plain_absolute(path), "not a plain absolute path: {path}");
    let args = docker.compose(read_file_args(target, path));
    let out = docker
        .capture(&args, None, Sensitivity::Diagnose, FILE_TIMEOUT)
        .await?;
    let (status, rest) = match out.stdout.iter().position(|&b| b == b'\n') {
        Some(end) => (&out.stdout[..end], &out.stdout[end + 1..]),
        None => (&out.stdout[..], &[][..]),
    };
    match (status, rest.is_empty()) {
        (b"present", _) => Ok(VolumeFile::Present(rest.to_vec())),
        (b"not-a-file", true) => Ok(VolumeFile::NotAFile),
        (b"absent", true) => Ok(VolumeFile::Absent),
        _ => Err(DockerError::Failed {
            command: args.display(),
            status: "exit status 0".to_owned(),
            detail: Some("its output does not start with a status line trawl knows".to_owned()),
        }),
    }
}

#[cfg(test)]
pub(crate) mod tests {
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
            compose::TLS_KEY,
            compose::TLS_CERT,
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

    /// A stub docker that runs the real writer or reader script locally,
    /// with the path moved under `root`.
    ///
    /// It logs every call, in order, to `calls` in the stub's own temp dir,
    /// outside `root`: `put <path>` for the writer, `read <path>` for the
    /// reader, and `other` for anything that runs neither, which then
    /// fails.
    pub(crate) fn local_writer(root: &std::path::Path) -> (tempfile::TempDir, Docker) {
        stub(&format!(
            r#"log="${{0%/*}}/calls"
run=
for a; do if [ "$a" = "-c" ]; then run=1; break; fi; done
if [ -z "$run" ]; then echo other >> "$log"; exit 1; fi
while [ "$1" != "-c" ]; do shift; done
script="$2"; path="$4"
case "$script" in *'mv -f'*) op=put ;; *) op=read ;; esac
echo "$op $path" >> "$log"
exec /bin/sh -c "$script" sh "{root}$path""#,
            root = root.display()
        ))
    }

    /// The calls [`local_writer`] logged, one per line.
    pub(crate) fn calls(stub: &tempfile::TempDir) -> String {
        std::fs::read_to_string(stub.path().join("calls")).unwrap_or_default()
    }

    #[test]
    fn the_reader_runs_as_the_writer_with_the_path_as_its_only_argument() {
        let read = read_file_args(Target::Trawld, compose::TLS_CERT)
            .argv()
            .to_vec();
        let put = put_file_args(Target::Trawld, compose::TLS_CERT)
            .argv()
            .to_vec();
        assert_eq!(read.len(), put.len());
        let script = read.iter().position(|a| a == "-c").unwrap() + 1;
        for (i, (r, p)) in read.iter().zip(&put).enumerate() {
            if i != script {
                assert_eq!(r, p, "argument {i}");
            }
        }
        assert_eq!(read[script], READ);
        assert!(!READ.contains("mv -f"), "the stub tells a read by this");
        // The cap the reader applies is READ_LIMIT + 1.
        assert!(READ.contains(&format!("head -c {} --", READ_LIMIT + 1)));
    }

    #[tokio::test]
    async fn read_file_tells_present_absent_and_not_a_file_apart() {
        let root = tempfile::tempdir().unwrap();
        let (stub, docker) = local_writer(root.path());
        let at = |path: &str| root.path().join(path.trim_start_matches('/'));

        // A fresh volume: no `tls/` either.
        let read = || read_file(&docker, Target::Trawld, compose::TLS_CERT);
        assert_eq!(read().await.unwrap(), VolumeFile::Absent);
        std::fs::create_dir_all(at(compose::TLS_CERT).parent().unwrap()).unwrap();
        assert_eq!(read().await.unwrap(), VolumeFile::Absent);

        // Bytes come back exactly, the empty file included.
        for bytes in [&b""[..], b"\n", b"-----BEGIN CERTIFICATE-----\nabc\n"] {
            std::fs::write(at(compose::TLS_CERT), bytes).unwrap();
            assert_eq!(read().await.unwrap(), VolumeFile::Present(bytes.to_vec()));
        }

        // A longer file comes back one byte past the limit.
        let limit = usize::try_from(READ_LIMIT).unwrap();
        std::fs::write(at(compose::TLS_CERT), vec![b'x'; limit * 2]).unwrap();
        assert_eq!(
            read().await.unwrap(),
            VolumeFile::Present(vec![b'x'; limit + 1])
        );

        // A directory, a link to a real file, and a dangling link are
        // none of them read.
        std::fs::remove_file(at(compose::TLS_CERT)).unwrap();
        std::fs::create_dir(at(compose::TLS_CERT)).unwrap();
        assert_eq!(read().await.unwrap(), VolumeFile::NotAFile);
        std::fs::remove_dir(at(compose::TLS_CERT)).unwrap();
        std::fs::write(at(compose::TLS_KEY), b"key").unwrap();
        std::os::unix::fs::symlink(at(compose::TLS_KEY), at(compose::TLS_CERT)).unwrap();
        assert_eq!(read().await.unwrap(), VolumeFile::NotAFile);
        std::fs::remove_file(at(compose::TLS_KEY)).unwrap();
        assert_eq!(read().await.unwrap(), VolumeFile::NotAFile);

        let logged = calls(&stub);
        assert!(
            logged
                .lines()
                .all(|line| line == format!("read {}", compose::TLS_CERT)),
            "{logged}"
        );
        assert_eq!(logged.lines().count(), 9, "{logged}");
    }

    /// Only the reader's own `absent` line reads as absent: a parent it
    /// cannot search, a failed run, an empty reply, and a reply without a
    /// status line are errors.
    #[tokio::test]
    async fn a_read_that_cannot_tell_is_an_error_never_absent() {
        // The ancestor walk, run as a user who cannot search `tls/`. Root
        // searches every directory, so it runs only for another user.
        if !nix::unistd::geteuid().is_root() {
            let root = tempfile::tempdir().unwrap();
            let (_stub, docker) = local_writer(root.path());
            let tls = root
                .path()
                .join(compose::TLS_CERT.trim_start_matches('/'))
                .parent()
                .unwrap()
                .to_owned();
            std::fs::create_dir_all(&tls).unwrap();
            std::fs::set_permissions(&tls, std::fs::Permissions::from_mode(0o000)).unwrap();
            let result = read_file(&docker, Target::Trawld, compose::TLS_CERT).await;
            std::fs::set_permissions(&tls, std::fs::Permissions::from_mode(0o700)).unwrap();
            let err = result.unwrap_err();
            assert!(err.to_string().contains("exit status 65"), "{err}");
            assert!(err.to_string().contains("cannot search"), "{err}");
        }

        for body in [
            "echo 'Error response from daemon: no such volume' >&2; exit 1",
            "exit 0",
            "printf ''",
            "echo absent-ish",
            "printf 'absent\\nmore'",
            "echo PRESENT",
        ] {
            let (_stub, docker) = stub(body);
            let result = read_file(&docker, Target::Trawld, compose::TLS_CERT).await;
            assert!(result.is_err(), "{body}: {result:?}");
        }
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
            .capture(&args, Some(b"10\nnew"), Sensitivity::Sealed, FILE_TIMEOUT)
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

    /// A directory at the path is never written into: the write fails,
    /// sealed, and leaves no temporary file beside or inside it.
    #[tokio::test]
    async fn put_file_to_a_directory_fails_and_leaves_nothing() {
        let root = tempfile::tempdir().unwrap();
        let (_stub, docker) = local_writer(root.path());
        let file = root.path().join(compose::TLS_KEY.trim_start_matches('/'));
        std::fs::create_dir_all(&file).unwrap();
        let secret = HexSecret::generate();
        let err = put_file(
            &docker,
            Target::Trawld,
            compose::TLS_KEY,
            secret.expose().as_bytes(),
        )
        .await
        .unwrap_err();
        let text = format!("{err} {err:?}");
        assert!(!text.contains(secret.expose()), "{text}");
        assert!(text.contains("exit status 67"), "{text}");
        assert!(!text.contains("cannot overwrite"), "sealed: {text}");
        assert!(file.is_dir());
        assert_eq!(std::fs::read_dir(&file).unwrap().count(), 0);
        let beside: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(beside, [file.file_name().unwrap().to_owned()]);
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
