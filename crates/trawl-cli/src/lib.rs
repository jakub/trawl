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
    /// Server URL (default: `https://localhost:5514`).
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
/// not `validate`: only `fields --data` has a serverless fallback, so for
/// everything else a token-resolution failure IS the answer and must
/// surface as itself — swallowing it into `Option` used to re-report a
/// broken profile as "schema field requires a server".
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
        opts.create(true).write(true).truncate(true);
    })?;
    // A log we cannot tighten is a log we do not write: unlike the
    // server's opt-in query log, this one is created fresh per run under
    // a user-chosen path, so a failing chmod means something is wrong
    // with that path rather than with an inherited file.
    if let Some(err) = chmod_error {
        return Err(err);
    }
    Ok(file)
}

#[cfg(all(test, unix))]
mod tui_log_tests {
    use super::open_tui_log;

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
