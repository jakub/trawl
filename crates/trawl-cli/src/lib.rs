// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `trawl` CLI, as a library.
//!
//! The binary target (`src/main.rs`) is a one-line shim over [`main`]; the
//! command implementations live here so integration tests can drive them
//! directly against a real server instead of shelling out. [`cli`] and
//! [`schema`] are public for exactly that reason — `config` and `tui` stay
//! private.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Mutex;

use clap::{Parser, Subcommand};

pub mod cli;
mod config;
pub mod schema;
mod tui;

/// trawl — search your logs with a pipeline DSL.
///
/// Run with no subcommand to launch the interactive TUI.
#[derive(Parser)]
#[command(name = "trawl", version, long_version = trawl_core::version::long_version(), about)]
struct Cli {
    /// trawld HTTPS API URL (default: `https://localhost:5514`).
    #[arg(long, env = "TRAWL_URL", global = true)]
    url: Option<String>,

    /// API token (direct value).
    #[arg(long, env = "TRAWL_TOKEN", global = true)]
    token: Option<String>,

    /// Accept self-signed TLS certificates.
    #[arg(long, env = "TRAWL_INSECURE", global = true)]
    insecure: bool,

    /// Named profile from config file (overrides [server] settings).
    #[arg(long, short = 'p', env = "TRAWL_PROFILE", global = true)]
    profile: Option<String>,

    /// Config file path (default: ~/.config/trawl/config.toml).
    #[arg(long, short = 'c', global = true)]
    config: Option<String>,

    /// Enable driver mode: listen on a unix socket for programmatic control.
    /// Uses `~/.config/trawl/driver.sock` by default, or specify a custom path.
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    driver: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Execute a DSL query and print results.
    Query {
        /// The trawl DSL query.
        query: String,

        /// Parquet glob path for embedded mode (e.g. "/data/**/*.parquet").
        #[arg(long)]
        data: Option<String>,

        /// Output format (auto-detected if omitted: table for TTY, json for pipes).
        #[arg(long, short, value_enum)]
        format: Option<cli::OutputFormat>,

        /// Write output to file instead of stdout (required for parquet format).
        #[arg(long, short)]
        output: Option<PathBuf>,
    },

    /// Validate DSL query syntax without executing.
    Validate {
        /// The trawl DSL query to validate.
        query: String,
    },

    /// Inspect the field catalog (pinned types, observations, conflicts).
    Schema {
        #[command(subcommand)]
        cmd: SchemaSubcommand,
    },

    /// Control a running TUI via driver socket.
    Driver {
        /// Path to the driver unix socket.
        #[arg(long, default_value_t = tui::driver::default_socket_path().display().to_string())]
        socket: String,

        #[command(subcommand)]
        cmd: DriverSubcommand,
    },
}

#[derive(Subcommand)]
enum SchemaSubcommand {
    /// List pinned fields with types, observations, and conflict counts.
    Fields {
        /// Only fields observed for this service.
        #[arg(long)]
        service: Option<String>,

        /// Only fields observed within this window (e.g. "24h", "7d").
        #[arg(long)]
        last: Option<String>,

        /// Maximum fields to list (server clamps to the pin cap).
        #[arg(long)]
        limit: Option<usize>,

        /// Parquet glob for embedded mode (names + physical types only,
        /// no server needed).
        #[arg(long)]
        data: Option<String>,

        /// Output format (auto-detected if omitted).
        #[arg(long, short, value_enum)]
        format: Option<cli::OutputFormat>,
    },

    /// Show one field's pin, per-service observations, and conflicts.
    Field {
        /// Field name (folded to the catalog's ASCII-lowercase spelling).
        name: String,

        /// Maximum service observations per page (server clamps at 1000).
        #[arg(long)]
        limit: Option<usize>,

        /// Resume after a previous run's printed cursor (next page of
        /// service observations).
        #[arg(long)]
        after: Option<String>,

        /// Output format (auto-detected if omitted).
        #[arg(long, short, value_enum)]
        format: Option<cli::OutputFormat>,
    },

    /// List recent type conflicts (schema-health dashboard).
    Conflicts {
        /// Only conflicts for this field.
        #[arg(long)]
        field: Option<String>,

        /// Only conflicts from this service.
        #[arg(long)]
        service: Option<String>,

        /// Only conflicts recorded within this window (e.g. "7d").
        #[arg(long)]
        last: Option<String>,

        /// Maximum rows to list (server clamps at 1000).
        #[arg(long)]
        limit: Option<usize>,

        /// Output format (auto-detected if omitted).
        #[arg(long, short, value_enum)]
        format: Option<cli::OutputFormat>,
    },

    /// Repin a field to a new type: shadow-rewrite the corpus with
    /// resurrection of conflict-nulled values from _raw (ADR-0011).
    Repin(RepinArgs),

    /// Show the running (or most recent) repin job.
    RepinStatus {
        /// Output format (auto-detected if omitted).
        #[arg(long, short, value_enum)]
        format: Option<cli::OutputFormat>,
    },

    /// Ask the running repin to stop. It stops at its next file boundary
    /// and the live corpus is left untouched; a repin already swapping the
    /// corpus is past the point where anything can be unwound and
    /// completes.
    RepinCancel {
        /// Output format (auto-detected if omitted).
        #[arg(long, short, value_enum)]
        format: Option<cli::OutputFormat>,
    },

    /// Reclaim pin slots held by fields nothing writes any more: no
    /// observation inside the window and no standing parquet declares the
    /// column.
    #[command(name = "gc-pins")]
    GcPins {
        /// Scan and report only, no deletion.
        #[arg(long)]
        dry_run: bool,

        /// How long a field must have gone unobserved to count as dead
        /// (e.g. "30d", "12w"). Defaults server-side to 30 days, and the
        /// server raises it to the retention window when that is longer.
        #[arg(long)]
        older_than: Option<String>,

        /// Output format (auto-detected if omitted).
        #[arg(long, short, value_enum)]
        format: Option<cli::OutputFormat>,
    },
    /// Acknowledge a field's degraded badge, or withdraw the
    /// acknowledgement. An ack covers the evidence that exists now: the
    /// next conflict episode raises the badge again.
    Ack(AckArgs),
}

/// `trawl schema ack` arguments.
#[derive(clap::Args)]
struct AckArgs {
    /// Field name (folded to the catalog's ASCII-lowercase spelling).
    field: String,

    /// Why the pin is being accepted as it stands (max 1024 bytes).
    #[arg(long, conflicts_with = "clear")]
    note: Option<String>,

    /// Withdraw the acknowledgement instead of writing one.
    #[arg(long)]
    clear: bool,

    /// Output format (auto-detected if omitted).
    #[arg(long, short, value_enum)]
    format: Option<cli::OutputFormat>,
}

/// `trawl schema repin` arguments.
///
/// Its own struct rather than an inline variant body: the flag list is long
/// enough that the dispatch arm was doing more unpacking than dispatching,
/// and [`RepinArgs::flags`] is the one place the command line turns into the
/// bundle [`schema::run_repin`] takes.
#[derive(clap::Args)]
#[allow(clippy::struct_excessive_bools)] // four independent CLI switches
struct RepinArgs {
    /// Field name (folded to the catalog's ASCII-lowercase spelling).
    field: String,

    /// Target type: BIGINT, DOUBLE, TIMESTAMP, BOOLEAN, VARCHAR, or
    /// SEVERITY.
    #[arg(long)]
    to: String,

    /// For --to severity only: which dialect the corpus's NUMERALS are
    /// read in (otel counts up 1-24, syslog counts down 0-7). The
    /// ladders overlap over 1-7 with opposite meanings, so only the
    /// operator can say which one the sender meant.
    #[arg(long, value_enum)]
    dialect: Option<cli::SeverityDialect>,

    /// Scan and report only — no mutation.
    #[arg(long)]
    dry_run: bool,

    /// Accept a lossy projection (values the new type cannot read are
    /// nulled; originals stay findable in _raw), or run a
    /// resurrection-only pass when --to equals the current pin.
    #[arg(long)]
    force: bool,

    /// Skip the interactive confirmation (required off a TTY).
    #[arg(long)]
    yes: bool,

    /// Poll the job to completion instead of returning immediately.
    #[arg(long)]
    wait: bool,

    /// With --force: the most rows the rewrite may null before the
    /// cutover is refused. Omitted, the server derives one from its own
    /// scan (10% headroom over a floor of 10 rows) and the CLI prints
    /// the number it binds.
    #[arg(long)]
    max_nulled_rows: Option<u64>,

    /// With --force, --to severity: the most dialect-ambiguous numerals
    /// (1-7) the rewrite may carry before the cutover is refused.
    #[arg(long)]
    max_ambiguous_rows: Option<u64>,

    /// Output format (auto-detected if omitted).
    #[arg(long, short, value_enum)]
    format: Option<cli::OutputFormat>,
}

impl RepinArgs {
    fn flags(&self) -> schema::RepinFlags {
        schema::RepinFlags {
            dialect: self.dialect,
            dry_run: self.dry_run,
            force: self.force,
            yes: self.yes,
            wait: self.wait,
            max_nulled_rows: self.max_nulled_rows,
            max_ambiguous_rows: self.max_ambiguous_rows,
        }
    }
}

#[derive(Subcommand)]
enum DriverSubcommand {
    /// Show TUI state (JSON).
    Status,

    /// Set editor content and execute query, print results.
    Query {
        /// The trawl DSL query.
        query: String,

        /// Output format (auto-detected if omitted: table for TTY, json for pipes).
        #[arg(long, short, value_enum)]
        format: Option<cli::OutputFormat>,

        /// Execute timeout in milliseconds.
        #[arg(long, default_value = "300000")]
        timeout: u64,
    },

    /// Set editor content without executing.
    SetQuery {
        /// The trawl DSL query.
        query: String,
    },

    /// Render TUI to text.
    Capture {
        /// Terminal width for capture.
        #[arg(long, default_value = "120")]
        width: u16,

        /// Terminal height for capture.
        #[arg(long, default_value = "40")]
        height: u16,
    },

    /// Inject a single keystroke (e.g. "ctrl+enter", "F5", "a").
    Key {
        /// Key string to inject.
        key: String,
    },

    /// Inject multiple keystrokes sequentially.
    Keys {
        /// Key strings to inject.
        keys: Vec<String>,
    },

    /// Get structured result data from a tab.
    GetResults {
        /// Tab index (0-based, defaults to active tab).
        #[arg(long)]
        tab: Option<usize>,

        /// Output format (auto-detected if omitted: table for TTY, json for pipes).
        #[arg(long, short, value_enum)]
        format: Option<cli::OutputFormat>,
    },

    /// Request clean TUI exit.
    Quit,
}

/// Every way a `trawl` invocation can fail.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Engine(#[from] trawl_engine::error::EngineError),
    #[error("{0}")]
    Client(#[from] trawl_client::ClientError),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Config(#[from] config::ConfigError),
    #[error("{0}")]
    Usage(String),
}

/// The binary's entry point: parse argv, run, and map errors to an exit code.
#[tokio::main]
pub async fn main() {
    let args = Cli::parse();

    if let Err(e) = run(args).await {
        // Broken pipe is expected (e.g. `trawl query ... | head`), exit quietly.
        if let CliError::Io(ref io_err) = e
            && io_err.kind() == io::ErrorKind::BrokenPipe
        {
            process::exit(1);
        }
        eprintln!("trawl: {e}");
        process::exit(1);
    }
}

async fn run(args: Cli) -> Result<(), CliError> {
    // Load config file and apply overrides.
    let mut cfg = config::Config::load(args.config.as_deref())?;
    if let Some(ref profile) = args.profile {
        cfg.apply_profile(profile)?;
    }
    cfg.apply_overrides(args.url, args.insecure);

    match args.command {
        None => {
            // TUI mode — tracing goes to a log file, not stderr (which corrupts the UI).
            let log_dir = shellexpand::tilde("~/.config/trawl");
            std::fs::create_dir_all(log_dir.as_ref())?;
            let log_file = open_tui_log(std::path::Path::new(&format!("{log_dir}/tui.log")))?;

            tracing_subscriber::fmt()
                .with_writer(Mutex::new(log_file))
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
                )
                .with_ansi(false)
                .init();

            // Resolve driver socket path.
            let driver_path: Option<PathBuf> = args.driver.map(|v| {
                if v.is_empty() {
                    tui::driver::default_socket_path()
                } else {
                    PathBuf::from(v)
                }
            });

            tui::run(&cfg, args.token.as_deref(), driver_path.as_deref()).await?;
        }
        Some(Command::Query {
            query,
            data,
            format,
            output,
        }) => {
            // For embedded mode (--data), no server connection needed.
            let conn = if data.is_some() {
                None
            } else {
                let token = cfg.load_token(args.token.as_deref())?;
                Some(cli::ConnectionParams {
                    url: cfg.server.url.clone(),
                    token,
                    insecure: cfg.server.insecure,
                })
            };

            cli::run_query(
                &query,
                data.as_deref(),
                format,
                output.as_deref(),
                conn,
                &cfg.ui.timezone,
            )
            .await?;
        }
        Some(Command::Validate { query }) => {
            // Validate supports both daemon and local-only mode.
            let conn = if let Ok(token) = cfg.load_token(args.token.as_deref()) {
                Some(cli::ConnectionParams {
                    url: cfg.server.url.clone(),
                    token,
                    insecure: cfg.server.insecure,
                })
            } else {
                None
            };

            cli::run_validate(&query, conn).await?;
        }

        Some(Command::Schema { cmd }) => {
            run_schema(cmd, &cfg, args.token.as_deref()).await?;
        }

        Some(Command::Driver { socket, cmd }) => {
            run_driver(&socket, cmd).await?;
        }
    }

    Ok(())
}

/// Dispatch `trawl schema <cmd>`. Connection resolution mirrors `query`,
/// not `validate`: only `fields --data` has a serverless fallback, so
/// everywhere else a token-resolution failure must surface as itself.
/// Swallowing it into an `Option` would re-report a broken profile as
/// "schema field requires a server".
// Long because it is one arm per subcommand: every arm unpacks its flags
// and calls its runner, and splitting the match would only move arms behind
// a second name.
#[allow(clippy::too_many_lines)]
async fn run_schema(
    cmd: SchemaSubcommand,
    cfg: &config::Config,
    token: Option<&str>,
) -> Result<(), CliError> {
    let conn = |token: Option<&str>| -> Result<cli::ConnectionParams, CliError> {
        let token = cfg.load_token(token)?;
        Ok(cli::ConnectionParams {
            url: cfg.server.url.clone(),
            token,
            insecure: cfg.server.insecure,
        })
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match cmd {
        SchemaSubcommand::Fields {
            service,
            last,
            limit,
            data,
            format,
        } => {
            let conn = if data.is_some() {
                None
            } else {
                Some(conn(token)?)
            };
            schema::run_fields(
                &mut out,
                conn,
                data.as_deref(),
                service.as_deref(),
                last.as_deref(),
                limit,
                format,
            )
            .await
        }
        SchemaSubcommand::Field {
            name,
            limit,
            after,
            format,
        } => {
            schema::run_field(
                &mut out,
                Some(conn(token)?),
                &name,
                limit,
                after.as_deref(),
                format,
            )
            .await
        }
        SchemaSubcommand::Conflicts {
            field,
            service,
            last,
            limit,
            format,
        } => {
            schema::run_conflicts(
                &mut out,
                Some(conn(token)?),
                field.as_deref(),
                service.as_deref(),
                last.as_deref(),
                limit,
                format,
            )
            .await
        }
        SchemaSubcommand::Repin(args) => {
            let flags = args.flags();
            schema::run_repin(
                &mut out,
                conn(token)?,
                &args.field,
                &args.to,
                flags,
                args.format,
            )
            .await
        }
        SchemaSubcommand::RepinStatus { format } => {
            schema::run_repin_status(&mut out, conn(token)?, format).await
        }
        SchemaSubcommand::RepinCancel { format } => {
            schema::run_repin_cancel(&mut out, conn(token)?, format).await
        }
        SchemaSubcommand::GcPins {
            dry_run,
            older_than,
            format,
        } => {
            schema::run_gc_pins(
                &mut out,
                conn(token)?,
                dry_run,
                older_than.as_deref(),
                format,
            )
            .await
        }
        SchemaSubcommand::Ack(args) => {
            let note = args.note.as_deref();
            schema::run_ack(
                &mut out,
                conn(token)?,
                &args.field,
                note,
                args.clear,
                args.format,
            )
            .await
        }
    }
}

async fn run_driver(socket: &str, cmd: DriverSubcommand) -> Result<(), CliError> {
    use tui::driver::DriverRequest;

    let socket_path = PathBuf::from(socket);

    match cmd {
        DriverSubcommand::Status => {
            let resp = driver_send(&socket_path, &DriverRequest::Status).await?;
            let json = serde_json::to_string_pretty(&resp.data)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            println!("{json}");
        }

        DriverSubcommand::Query {
            query,
            format,
            timeout,
        } => {
            driver_send(
                &socket_path,
                &DriverRequest::SetQuery {
                    query: query.clone(),
                },
            )
            .await?;
            let resp = driver_send(
                &socket_path,
                &DriverRequest::Execute {
                    timeout_ms: timeout,
                },
            )
            .await?;
            render_driver_data(&resp.data, format)?;
        }

        DriverSubcommand::SetQuery { query } => {
            driver_send(&socket_path, &DriverRequest::SetQuery { query }).await?;
        }

        DriverSubcommand::Capture { width, height } => {
            let resp = driver_send(
                &socket_path,
                &DriverRequest::Capture {
                    width: Some(width),
                    height: Some(height),
                },
            )
            .await?;
            if let Some(content) = resp.data.content {
                print!("{content}");
            }
        }

        DriverSubcommand::Key { key } => {
            driver_send(&socket_path, &DriverRequest::Key { key }).await?;
        }

        DriverSubcommand::Keys { keys } => {
            driver_send(&socket_path, &DriverRequest::Keys { keys }).await?;
        }

        DriverSubcommand::GetResults { tab, format } => {
            let resp = driver_send(&socket_path, &DriverRequest::GetResults { tab }).await?;
            render_driver_data(&resp.data, format)?;
        }

        DriverSubcommand::Quit => {
            driver_send(&socket_path, &DriverRequest::Quit).await?;
        }
    }

    Ok(())
}

/// Send a driver command and check for protocol-level errors.
async fn driver_send(
    socket_path: &Path,
    request: &tui::driver::DriverRequest,
) -> Result<tui::driver::DriverResponse, CliError> {
    let resp = tui::driver::send_command(socket_path, request).await?;
    if !resp.ok {
        return Err(CliError::Usage(
            resp.error.unwrap_or_else(|| "driver command failed".into()),
        ));
    }
    Ok(resp)
}

/// Open the TUI trace log truncated and owner-only on Unix (`0600`,
/// tightening a pre-existing looser file) — tracing output can carry
/// query text and server responses.
fn open_tui_log(path: &std::path::Path) -> io::Result<std::fs::File> {
    let (file, chmod_error) = trawl_config::fs::open_with_mode(path, 0o600, |opts| {
        opts.create(true).write(true);
    })?;
    finish_tui_log_open(file, chmod_error)
}

fn finish_tui_log_open(
    file: std::fs::File,
    chmod_error: Option<io::Error>,
) -> io::Result<std::fs::File> {
    // A log we cannot tighten is a log we do not write: unlike the
    // server's opt-in query log, this one is created fresh per run under
    // a user-chosen path, so a failing chmod means something is wrong
    // with that path rather than with an inherited file.
    if let Some(err) = chmod_error {
        return Err(err);
    }
    // Opening with `truncate(true)` would erase an existing file before
    // the chmod result above is known. Truncate only after the owner-only
    // policy has accepted the opened file.
    file.set_len(0)?;
    Ok(file)
}

#[cfg(all(test, unix))]
mod tui_log_tests {
    use std::io;

    use super::{finish_tui_log_open, open_tui_log};

    fn mode_of(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn tui_log_is_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tui.log");
        let _file = open_tui_log(&path).unwrap();
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn tui_log_tightens_existing_looser_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tui.log");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let _file = open_tui_log(&path).unwrap();
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }

    #[test]
    fn tui_log_chmod_failure_preserves_existing_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tui.log");
        std::fs::write(&path, "keep me").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();

        let err = finish_tui_log_open(
            file,
            Some(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected chmod failure",
            )),
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");
    }
}

/// Render driver response data containing columns + rows.
fn render_driver_data(
    data: &tui::driver::DriverData,
    format: Option<cli::OutputFormat>,
) -> Result<(), CliError> {
    let columns = data.columns.as_deref().unwrap_or_default();
    let rows = data.rows.as_deref().unwrap_or_default();

    let format = format.unwrap_or_else(|| {
        if io::stdout().is_terminal() {
            cli::OutputFormat::Table
        } else {
            cli::OutputFormat::Json
        }
    });

    let stdout = io::stdout();
    let mut out = stdout.lock();
    cli::render_driver_results(columns, rows, format, &mut out)?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod arg_tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    /// clap's own consistency check over the whole command tree: a
    /// conflicting-argument name that does not exist is a runtime panic
    /// otherwise, and `schema ack --clear` names `note`.
    #[test]
    fn the_command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    /// The repin ceilings parse as numbers and default to absent. Absent is
    /// "let the server's scan decide", which is not the same as zero.
    #[test]
    fn repin_ceiling_flags_parse() {
        let cli = Cli::try_parse_from([
            "trawl",
            "schema",
            "repin",
            "status",
            "--to",
            "VARCHAR",
            "--force",
            "--yes",
            "--max-nulled-rows",
            "250",
            "--max-ambiguous-rows",
            "0",
        ])
        .expect("both ceilings parse");
        let Some(Command::Schema {
            cmd: SchemaSubcommand::Repin(args),
        }) = cli.command
        else {
            panic!("expected a schema repin command");
        };
        assert!(args.force);
        assert_eq!(args.max_nulled_rows, Some(250));
        assert_eq!(args.max_ambiguous_rows, Some(0));
        // The bundle the command handler receives carries them unchanged.
        let flags = args.flags();
        assert_eq!(flags.max_nulled_rows, Some(250));
        assert_eq!(flags.max_ambiguous_rows, Some(0));

        let cli =
            Cli::try_parse_from(["trawl", "schema", "repin", "status", "--to", "VARCHAR"]).unwrap();
        let Some(Command::Schema {
            cmd: SchemaSubcommand::Repin(args),
        }) = cli.command
        else {
            panic!("expected a schema repin command");
        };
        assert_eq!(args.max_nulled_rows, None);
        assert_eq!(args.max_ambiguous_rows, None);

        assert!(
            Cli::try_parse_from([
                "trawl",
                "schema",
                "repin",
                "status",
                "--to",
                "VARCHAR",
                "--max-nulled-rows",
                "many",
            ])
            .is_err(),
            "a ceiling is a row count, not a word"
        );
    }

    /// `schema ack` takes a note or clears, never both: withdrawing an
    /// acknowledgement writes no note, so accepting one would silently drop
    /// what the operator typed.
    #[test]
    fn ack_note_and_clear_are_mutually_exclusive() {
        let cli = Cli::try_parse_from(["trawl", "schema", "ack", "duration", "--note", "fix due"])
            .unwrap();
        let Some(Command::Schema {
            cmd: SchemaSubcommand::Ack(args),
        }) = cli.command
        else {
            panic!("expected a schema ack command");
        };
        assert_eq!(args.field, "duration");
        assert_eq!(args.note.as_deref(), Some("fix due"));
        assert!(!args.clear);

        assert!(Cli::try_parse_from(["trawl", "schema", "ack", "duration", "--clear"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "trawl", "schema", "ack", "duration", "--clear", "--note", "fix due",
            ])
            .is_err(),
            "--clear and --note must not be accepted together"
        );
    }

    /// Embedded mode prints an engine refusal as its own sentence.
    ///
    /// `CliError` renders every engine failure through `{0}`, and the
    /// refusal is trawl-authored and quotes only the tokens the reader
    /// typed, so the one thing the exit path must not do is dress it up:
    /// "trawl: " and the sentence, with no "query failed" in front of it.
    #[test]
    fn an_engine_refusal_prints_as_its_own_sentence() {
        let message = "timechart on 'hostname' is not a timestamp: VARCHAR";
        let err = CliError::Engine(trawl_engine::error::EngineError::Refused {
            message: message.to_string(),
        });
        assert_eq!(err.to_string(), message);
    }
}
