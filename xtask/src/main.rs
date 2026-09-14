// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Workspace task runner:
//! - `build-web` — the SPA-into-binary build in the right order:
//!   1. `trunk build [--release]` inside `crates/trawl-web-ui/`
//!   2. release builds precompress + enforce the wire-size budget
//!   3. `cargo build [--release] -p trawl-web`
//! - `compress-web` — precompress an existing release SPA and enforce
//!   the gzip/Brotli wire-size budgets (used by cross-build CI).
//! - `design-cards` — emit static fleet-ui preview cards for
//!   claude.ai/design (see `design_cards` module).
//! - `ingest-fuzz` — emit deterministic, Vector-compatible NDJSON corpora
//!   for the ingest canonicalizer and field-catalog pin/conform boundary,
//!   per producer profile (`--profile http|syslog|trawld`).
//! - `pg-admission-guard` — prove every pg-touching test runs inside the
//!   `postgres` nextest group, so the workspace cannot outgrow postgres'
//!   `max_connections` (ADR-0021 ruling 4, see `pg_admission`).
//! - `e2e` — the real-browser Playwright suite (issue #118,
//!   `crates/trawl-web-ui/e2e/`): trunk-build the SPA, `npm ci` the
//!   suite's own devDependency, install the chromium browser, then run
//!   the suite against the zero-npm-dep stub server in `e2e/harness/`.
//!
//! Aliased as `cargo xtask` via `.cargo/config.toml`.

mod design_cards;
mod ingest_fuzz;
mod pg_admission;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use brotli::CompressorWriter;
use clap::{Parser, Subcommand};
use flate2::{Compression, GzBuilder};
use ingest_fuzz::{Phase, Profile};

const GZIP_BUDGET: u64 = 4 * 1024 * 1024;
const BROTLI_BUDGET: u64 = 5 * 1024 * 1024 / 2;
const BROTLI_QUALITY: u32 = 9;
const BROTLI_WINDOW: u32 = 22;
// TrueType is on the list because the fleet fonts ship as raw `.ttf`
// (the pinned upstream bytes, never re-cut to WOFF2); gzip roughly halves
// them. WOFF2 would not belong here: it is already Brotli inside.
const COMPRESSIBLE_EXTENSIONS: &[&str] = &[
    "css", "html", "js", "json", "map", "svg", "ttf", "txt", "wasm", "xml",
];

#[derive(Parser)]
#[command(name = "xtask", about = "trawl workspace task runner")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build the SPA, then compile trawl-web with the fresh dist/ baked in.
    BuildWeb {
        /// Pass --release to both trunk and cargo.
        #[arg(long)]
        release: bool,
    },
    /// Precompress an existing release SPA and enforce its wire-size budget.
    CompressWeb {
        /// SPA distribution directory, relative to the workspace root.
        #[arg(long, default_value = "crates/trawl-web-ui/dist")]
        dist: PathBuf,
    },
    /// Emit static fleet-ui HTML preview cards (derived from the
    /// component class contracts; regenerate, never hand-edit).
    DesignCards {
        /// Output directory for the generated cards.
        #[arg(long, default_value = "target/design-cards")]
        out: PathBuf,
    },
    /// Emit deterministic NDJSON for ingest and schema-pinning fuzz runs.
    IngestFuzz {
        /// Corpus phase. Run `pin`, compact, then run `conflicts` with the
        /// same seed and namespace to exercise already-pinned fields.
        #[arg(long, value_enum, default_value_t = Phase::Mutate)]
        phase: Phase,
        /// Producer profile the `mutate` corpus is shaped for: `http`
        /// wire events, or the payload map the `syslog`/`trawld` door
        /// hands the one canonicalizer. Only `mutate` has a profile.
        #[arg(long, value_enum, default_value_t = Profile::Http)]
        profile: Profile,
        /// Seed controlling field names, values, and mutation selection.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Lowercase label used to isolate one corpus's field and service names.
        #[arg(long, default_value = "local")]
        namespace: String,
        /// Allowlisted trawl environment written into accepted events.
        #[arg(long, default_value = "prod")]
        env: String,
        /// Number of events in the seeded `mutate` phase. Fixed contract
        /// phases use the exact row counts required by their boundaries.
        #[arg(long, default_value_t = 100)]
        events: usize,
    },
    /// Check that every pg-touching test binary runs inside the
    /// `postgres` nextest group. Arguments after `--` are forwarded to
    /// `cargo nextest list` (CI passes its feature selection).
    PgAdmissionGuard {
        /// Extra arguments for the `cargo nextest list` invocation.
        #[arg(last = true)]
        nextest_args: Vec<String>,
    },
    /// Run the real-browser Playwright suite (issue #118).
    E2e {
        /// Skip the `trunk build` step — reuse whatever's already in
        /// `crates/trawl-web-ui/dist/`. Fails loudly at server startup
        /// if that dist/ doesn't exist.
        #[arg(long)]
        skip_build: bool,
        /// Pass --release to the trunk build.
        #[arg(long)]
        release: bool,
        /// Run the browser headed (visible window) instead of headless.
        #[arg(long)]
        headed: bool,
        /// Only run specs whose title matches this pattern
        /// (`playwright test --grep`).
        #[arg(long)]
        grep: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::BuildWeb { release } => build_web(release),
        Cmd::CompressWeb { dist } => {
            let root = workspace_root();
            let dist = if dist.is_absolute() {
                dist
            } else {
                root.join(dist)
            };
            match compress_web(&dist) {
                Ok(sizes) => {
                    report_sizes(sizes);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("xtask: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Cmd::DesignCards { out } => {
            let root = workspace_root();
            let css = root.join("crates/fleet-ui/styles/fleet-ui.css");
            let fonts = root.join("crates/fleet-ui/fonts");
            let out = if out.is_absolute() {
                out
            } else {
                root.join(out)
            };
            design_cards::generate(&css, &fonts, &out)
        }
        Cmd::IngestFuzz {
            phase,
            profile,
            seed,
            namespace,
            env,
            events,
        } => ingest_fuzz::run(&ingest_fuzz::Config {
            phase,
            profile,
            seed,
            namespace,
            env,
            mutation_events: events,
        }),
        Cmd::PgAdmissionGuard { nextest_args } => {
            pg_admission::run(&workspace_root(), &nextest_args)
        }
        Cmd::E2e {
            skip_build,
            release,
            headed,
            grep,
        } => run_e2e(skip_build, release, headed, grep.as_deref()),
    }
}

/// Is `tool` on `PATH`? Used to fail loudly and by name rather than let
/// `Command::spawn` bubble up a bare "No such file or directory".
fn tool_on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file()))
}

/// `cargo xtask e2e` — build the SPA, install the suite's own npm
/// dependency + browser, then run Playwright against the zero-npm-dep
/// stub server in `e2e/harness/server.mjs`.
fn run_e2e(skip_build: bool, release: bool, headed: bool, grep: Option<&str>) -> ExitCode {
    let root = workspace_root();
    let web_ui = root.join("crates").join("trawl-web-ui");
    let e2e = web_ui.join("e2e");

    for (tool, hint) in [
        ("node", "install Node.js (node/npm/npx)"),
        ("npm", "install Node.js (node/npm/npx)"),
        ("npx", "install Node.js (node/npm/npx)"),
    ] {
        if !tool_on_path(tool) {
            eprintln!("xtask: `{tool}` not found on PATH — {hint}");
            return ExitCode::FAILURE;
        }
    }
    if !skip_build && !tool_on_path("trunk") {
        eprintln!(
            "xtask: `trunk` not found on PATH — install it (`cargo install trunk`) \
             or pass --skip-build to reuse an existing crates/trawl-web-ui/dist/"
        );
        return ExitCode::FAILURE;
    }

    if !skip_build {
        let mut trunk = Command::new("trunk");
        trunk.current_dir(&web_ui).arg("build");
        if release {
            trunk.arg("--release");
        }
        eprintln!(
            "xtask: trunk build{} (in {})",
            if release { " --release" } else { "" },
            web_ui.display()
        );
        if !run(trunk) {
            return ExitCode::FAILURE;
        }
    }

    let mut npm_ci = Command::new("npm");
    npm_ci
        .current_dir(&e2e)
        .args(["ci", "--no-audit", "--no-fund"]);
    eprintln!("xtask: npm ci (in {})", e2e.display());
    if !run(npm_ci) {
        return ExitCode::FAILURE;
    }

    let mut install_browser = Command::new("npx");
    install_browser
        .current_dir(&e2e)
        .args(["playwright", "install", "chromium"]);
    eprintln!("xtask: npx playwright install chromium");
    if !run(install_browser) {
        return ExitCode::FAILURE;
    }

    let mut test = Command::new("npm");
    test.current_dir(&e2e).args(["run", "test", "--"]);
    if headed {
        test.arg("--headed");
    }
    if let Some(pattern) = grep {
        test.args(["--grep", pattern]);
    }
    eprintln!(
        "xtask: npm run test{}{}",
        if headed { " --headed" } else { "" },
        grep.map_or_else(String::new, |g| format!(" --grep {g}")),
    );
    if !run(test) {
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn build_web(release: bool) -> ExitCode {
    let root = workspace_root();
    let web_ui = root.join("crates").join("trawl-web-ui");

    let mut trunk = Command::new("trunk");
    trunk.current_dir(&web_ui).arg("build");
    if release {
        trunk.arg("--release");
    }
    eprintln!(
        "xtask: trunk build{} (in {})",
        if release { " --release" } else { "" },
        web_ui.display()
    );
    if !run(trunk) {
        return ExitCode::FAILURE;
    }

    if release {
        match compress_web(&web_ui.join("dist")) {
            Ok(sizes) => report_sizes(sizes),
            Err(e) => {
                eprintln!("xtask: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let mut cargo = Command::new(env!("CARGO"));
    cargo.current_dir(&root).args(["build", "-p", "trawl-web"]);
    if release {
        cargo.arg("--release");
    }
    eprintln!(
        "xtask: cargo build -p trawl-web{}",
        if release { " --release" } else { "" }
    );
    if !run(cargo) {
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BundleSizes {
    raw: u64,
    gzip_wire: u64,
    brotli_wire: u64,
}

fn compress_web(dist: &Path) -> Result<BundleSizes, String> {
    if !dist.join("index.html").is_file() {
        return Err(format!(
            "{} has no index.html; run `trunk build --release` first",
            dist.display()
        ));
    }

    let mut files = Vec::new();
    collect_files(dist, &mut files).map_err(|e| format!("scan {}: {e}", dist.display()))?;
    files.sort();

    // A release build normally replaces dist atomically, but deleting old
    // sidecars here makes this command safe after manual/incremental builds too.
    for path in &files {
        if is_sidecar(path)
            && let Err(e) = fs::remove_file(path)
        {
            return Err(format!("remove stale {}: {e}", path.display()));
        }
    }
    files.retain(|path| !is_sidecar(path));

    let mut sizes = BundleSizes::default();
    for path in files {
        let raw = fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let raw_len =
            u64::try_from(raw.len()).map_err(|_| format!("{} is too large", path.display()))?;
        sizes.raw = sizes.raw.saturating_add(raw_len);

        if is_compressible(&path) {
            let gzip = gzip(&raw).map_err(|e| format!("gzip {}: {e}", path.display()))?;
            let brotli = brotli(&raw).map_err(|e| format!("brotli {}: {e}", path.display()))?;
            sizes.gzip_wire = sizes
                .gzip_wire
                .saturating_add(write_if_smaller(&path, "gz", &raw, &gzip)?);
            sizes.brotli_wire = sizes
                .brotli_wire
                .saturating_add(write_if_smaller(&path, "br", &raw, &brotli)?);
        } else {
            sizes.gzip_wire = sizes.gzip_wire.saturating_add(raw_len);
            sizes.brotli_wire = sizes.brotli_wire.saturating_add(raw_len);
        }
    }

    if sizes.gzip_wire > GZIP_BUDGET {
        return Err(format!(
            "gzip wire size {} exceeds {} budget",
            human_bytes(sizes.gzip_wire),
            human_bytes(GZIP_BUDGET)
        ));
    }
    if sizes.brotli_wire > BROTLI_BUDGET {
        return Err(format!(
            "Brotli wire size {} exceeds {} budget",
            human_bytes(sizes.brotli_wire),
            human_bytes(BROTLI_BUDGET)
        ));
    }

    Ok(sizes)
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn is_sidecar(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("br" | "gz")
    )
}

fn is_compressible(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| COMPRESSIBLE_EXTENSIONS.contains(&ext))
}

fn gzip(raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut writer = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::best());
    writer.write_all(raw)?;
    writer.finish()
}

fn brotli(raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut writer = CompressorWriter::new(Vec::new(), 64 * 1024, BROTLI_QUALITY, BROTLI_WINDOW);
    writer.write_all(raw)?;
    writer.flush()?;
    Ok(writer.into_inner())
}

fn write_if_smaller(
    source: &Path,
    extension: &str,
    raw: &[u8],
    compressed: &[u8],
) -> Result<u64, String> {
    if compressed.len() >= raw.len() {
        return u64::try_from(raw.len()).map_err(|_| format!("{} is too large", source.display()));
    }

    let sidecar = append_extension(source, extension);
    let temporary = append_extension(source, &format!("{extension}.tmp"));
    fs::write(&temporary, compressed).map_err(|e| format!("write {}: {e}", temporary.display()))?;
    fs::rename(&temporary, &sidecar).map_err(|e| {
        format!(
            "publish {} as {}: {e}",
            temporary.display(),
            sidecar.display()
        )
    })?;
    u64::try_from(compressed.len()).map_err(|_| format!("{} is too large", sidecar.display()))
}

fn append_extension(path: &Path, extension: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".");
    value.push(extension);
    PathBuf::from(value)
}

fn report_sizes(sizes: BundleSizes) {
    eprintln!(
        "xtask: web bundle raw={}, gzip={}, brotli={} (budgets: gzip {}, brotli {})",
        human_bytes(sizes.raw),
        human_bytes(sizes.gzip_wire),
        human_bytes(sizes.brotli_wire),
        human_bytes(GZIP_BUDGET),
        human_bytes(BROTLI_BUDGET)
    );
}

fn human_bytes(bytes: u64) -> String {
    let hundredths = u128::from(bytes) * 100 / (1024 * 1024);
    format!("{}.{:02} MiB", hundredths / 100, hundredths % 100)
}

fn run(mut cmd: Command) -> bool {
    match cmd.status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("xtask: command failed: {status}");
            false
        }
        Err(e) => {
            eprintln!("xtask: failed to spawn: {e}");
            false
        }
    }
}

/// The workspace root — parent of `xtask/`.
fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map_or_else(|| manifest.to_path_buf(), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_extension_keeps_the_original_extension() {
        assert_eq!(
            append_extension(Path::new("app.wasm"), "br"),
            PathBuf::from("app.wasm.br")
        );
    }

    #[test]
    fn compression_is_deterministic_and_smaller_for_wasm_like_input() {
        let raw = vec![0_u8; 128 * 1024];
        let gzip_a = gzip(&raw).unwrap();
        let gzip_b = gzip(&raw).unwrap();
        let brotli_a = brotli(&raw).unwrap();
        let brotli_b = brotli(&raw).unwrap();

        assert_eq!(gzip_a, gzip_b);
        assert_eq!(brotli_a, brotli_b);
        assert!(gzip_a.len() < raw.len());
        assert!(brotli_a.len() < raw.len());
    }
}
