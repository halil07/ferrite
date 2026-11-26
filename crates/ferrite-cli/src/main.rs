#![forbid(unsafe_code)]

mod mcp;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use ferrite_cdp::MockCdpTransport;
use ferrite_core::NATIVE_HOST_NAME;
use ferrite_daemon::{
    ConnectionInfo, E2eSession, HeadlessCdpTransport, HeadlessOptions, SuiteReport, WsServer,
    default_connection_file, default_snapshots_dir, generate_session_token, read_connection_file,
    run_suite, run_suite_over_ws_with_snapshots, send_ws_rpc, write_connection_file,
};
use ferrite_dsl::{compile_path, default_project_scaffold, load_project_config};
use ferrite_ipc::{NativeBridgeClient, StdioBridge};
use serde_json::json;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BrowserKind {
    Chrome,
    Edge,
    Brave,
    Chromium,
}

#[derive(Debug, Parser)]
#[command(
    name = "ferrite",
    version,
    about = "Ferrite E2E: .ferrite DSL + MV3 Chrome Extension + Native Messaging + Tokio Engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run as Chrome Native Messaging host (reads 4-byte LE frames from stdin, writes to stdout).
    NativeHost {
        #[arg(long, default_value_t = 0)]
        ws_port: u16,

        #[arg(long)]
        snapshots_dir: Option<PathBuf>,

        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        _chrome_args: Vec<String>,
    },

    /// Start a standalone local WebSocket server backed by MockCdpTransport for development/testing.
    DevServer {
        #[arg(long, default_value_t = 9444)]
        ws_port: u16,

        #[arg(long, default_value = ".ferrite-snapshots")]
        snapshots_dir: PathBuf,
    },

    /// Scaffold a new `.ferrite` E2E test project (ferrite.toml, helpers, fixtures, starter test).
    Init {
        #[arg(default_value = ".")]
        dir: PathBuf,

        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Compile `.ferrite` test files or a project directory into JSON TestSuite IR.
    Compile {
        #[arg(default_value = ".")]
        path: PathBuf,

        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Validate and type-check `.ferrite` test suites without launching a browser run.
    Check {
        #[arg(default_value = ".")]
        path: PathBuf,
    },

    /// Execute `.ferrite` test files, JSON suites, or a project directory over WebSocket.
    Run {
        #[arg(default_value = ".")]
        path: PathBuf,

        #[arg(long)]
        ws_url: Option<String>,

        #[arg(long)]
        token: Option<String>,

        #[arg(long, default_value_t = false)]
        update_baselines: bool,

        #[arg(long)]
        snapshots_dir: Option<PathBuf>,

        /// Output the full machine-readable JSON test report instead of the human summary.
        #[arg(long, default_value_t = false)]
        json: bool,

        /// Launch an isolated headless Chrome/Chromium/Edge browser directly (no extension or visible window required).
        #[arg(long, default_value_t = false)]
        headless: bool,

        /// Optional explicit path to Chrome/Chromium/Edge binary when running with --headless.
        #[arg(long)]
        chrome_path: Option<PathBuf>,
    },

    /// Record live user interactions in Chrome and generate a `.ferrite` test file.
    Record {
        #[arg(long)]
        url: Option<String>,

        #[arg(long, default_value = "recorded-suite")]
        suite: String,

        #[arg(long, default_value = "recorded-flow")]
        case_name: String,

        #[arg(long)]
        out: Option<PathBuf>,

        #[arg(long, default_value_t = false)]
        no_mask_passwords: bool,

        #[arg(long)]
        ws_url: Option<String>,

        #[arg(long)]
        token: Option<String>,
    },

    /// Register the Chrome Native Messaging host manifest for a Store or unpacked extension ID.
    Setup {
        #[arg(long)]
        extension_id: Option<String>,

        #[arg(long, value_enum, default_value_t = BrowserKind::Chrome)]
        browser: BrowserKind,

        #[arg(long)]
        binary_path: Option<PathBuf>,
    },

    /// Run a Model Context Protocol (MCP) JSON-RPC 2.0 server over stdio for AI agents.
    Mcp {
        #[arg(long)]
        ws_url: Option<String>,

        #[arg(long)]
        token: Option<String>,

        /// Optional default Chrome tabId to target when a tool call omits `tabId`.
        #[arg(long)]
        tab_id: Option<i64>,

        #[arg(long)]
        snapshots_dir: Option<PathBuf>,
    },

    /// Print a Chrome Native Messaging host manifest JSON (`com.ferrite.e2e`).
    PrintManifest {
        #[arg(long)]
        extension_id: String,

        #[arg(long)]
        binary_path: Option<PathBuf>,
    },
}

fn init_stderr_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Chrome launches native messaging hosts with `chrome-extension://<id>/` (and `--parent-window=...`
    // on Windows) directly as argv[1..], without a subcommand. Detect and route cleanly to native-host mode.
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args
        .get(1)
        .is_some_and(|arg| arg.starts_with("chrome-extension://"))
    {
        init_stderr_tracing();
        return run_native_host(0, default_snapshots_dir()).await;
    }

    let cli = Cli::parse();
    let Some(command) = cli.command else {
        anyhow::bail!("missing subcommand; run `ferrite --help` for usage");
    };

    match command {
        Command::NativeHost {
            ws_port,
            snapshots_dir,
            ..
        } => {
            init_stderr_tracing();
            run_native_host(ws_port, snapshots_dir.unwrap_or_else(default_snapshots_dir)).await
        }
        Command::DevServer {
            ws_port,
            snapshots_dir,
        } => {
            init_stderr_tracing();
            run_dev_server(ws_port, snapshots_dir).await
        }
        Command::Init { dir, force } => run_init(&dir, force),
        Command::Compile { path, out } => run_compile(&path, out.as_deref()),
        Command::Check { path } => run_check(&path),
        Command::Run {
            path,
            ws_url,
            token,
            update_baselines,
            snapshots_dir,
            json: json_output,
            headless,
            chrome_path,
        } => {
            init_stderr_tracing();
            run_test_suites(
                path,
                ws_url,
                token,
                update_baselines,
                snapshots_dir,
                json_output,
                headless,
                chrome_path,
            )
            .await
        }
        Command::Record {
            url,
            suite,
            case_name,
            out,
            no_mask_passwords,
            ws_url,
            token,
        } => {
            init_stderr_tracing();
            run_record(
                url,
                suite,
                case_name,
                out,
                !no_mask_passwords,
                ws_url,
                token,
            )
            .await
        }
        Command::Setup {
            extension_id,
            browser,
            binary_path,
        } => run_setup(extension_id, browser, binary_path),
        Command::Mcp {
            ws_url,
            token,
            tab_id,
            snapshots_dir,
        } => {
            init_stderr_tracing();
            let server = mcp::McpServer::new(mcp::McpConfig {
                ws_url,
                token,
                default_tab_id: tab_id,
                snapshots_dir,
            });
            server.run_stdio().await
        }
        Command::PrintManifest {
            extension_id,
            binary_path,
        } => print_manifest(&extension_id, binary_path),
    }
}

async fn run_native_host(ws_port: u16, snapshots_dir: PathBuf) -> anyhow::Result<()> {
    let stdio = StdioBridge::spawn()?;
    let client = NativeBridgeClient::connect(stdio.outbound_tx, stdio.inbound_rx);
    let client_handle = Arc::clone(&client);

    let session = E2eSession::new(client, snapshots_dir);
    let token = generate_session_token()?;
    let ws = WsServer::bind(ws_port, session, token.clone()).await?;

    let conn_path = default_connection_file();
    let my_pid = std::process::id();
    let info = ConnectionInfo {
        port: ws.addr().port(),
        token,
        pid: my_pid,
    };
    write_connection_file(&conn_path, &info)?;
    tracing::info!(
        ws_url = %ws.ws_url(),
        connection_file = %conn_path.display(),
        "Ferrite native-host daemon ready"
    );

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received Ctrl-C, shutting down native-host daemon");
        }
        _ = client_handle.wait_disconnected() => {
            tracing::info!("native messaging stdio pipe closed, exiting");
        }
    }

    if read_connection_file(&conn_path).is_ok_and(|existing| existing.pid == my_pid) {
        let _ = fs::remove_file(&conn_path);
    }
    ws.shutdown();
    Ok(())
}

async fn run_dev_server(ws_port: u16, snapshots_dir: PathBuf) -> anyhow::Result<()> {
    let mock: Arc<MockCdpTransport> = MockCdpTransport::new();
    let session = E2eSession::new(mock, snapshots_dir);
    let token = generate_session_token()?;
    let ws = WsServer::bind(ws_port, session, token.clone()).await?;

    let conn_path = default_connection_file();
    write_connection_file(
        &conn_path,
        &ConnectionInfo {
            port: ws.addr().port(),
            token: token.clone(),
            pid: std::process::id(),
        },
    )?;

    tracing::info!(
        ws_url = %ws.ws_url(),
        token = %token,
        connection_file = %conn_path.display(),
        "Ferrite dev-server listening"
    );

    tokio::signal::ctrl_c().await?;
    let _ = fs::remove_file(&conn_path);
    ws.shutdown();
    Ok(())
}

fn run_init(dir: &Path, force: bool) -> anyhow::Result<()> {
    let written = default_project_scaffold(dir, force)?;
    if written.is_empty() {
        println!(
            "All Ferrite project files already exist in {} (pass --force to overwrite).",
            dir.display()
        );
    } else {
        println!("Initialized Ferrite E2E project in {}:", dir.display());
        for p in written {
            println!("  created {}", p.display());
        }
    }
    Ok(())
}

fn run_compile(path: &Path, out: Option<&Path>) -> anyhow::Result<()> {
    let compiled = compile_path(path)?;
    if compiled.is_empty() {
        anyhow::bail!("no runnable test cases found under {}", path.display());
    }

    let rendered = if compiled.len() == 1 && path.is_file() {
        serde_json::to_string_pretty(&compiled[0].1)?
    } else {
        let suites: Vec<_> = compiled.into_iter().map(|(_, s)| s).collect();
        serde_json::to_string_pretty(&suites)?
    };

    if let Some(dest) = out {
        if let Some(parent) = dest.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(dest, format!("{rendered}\n"))?;
        println!("Compiled suite JSON written to {}", dest.display());
    } else {
        println!("{rendered}");
    }
    Ok(())
}

fn run_check(path: &Path) -> anyhow::Result<()> {
    let compiled = compile_path(path)?;
    if compiled.is_empty() {
        anyhow::bail!("no runnable `.ferrite` test suites found under {}", path.display());
    }

    let mut total_cases = 0usize;
    for (file, suite) in &compiled {
        total_cases += suite.cases.len();
        println!(
            "OK  {}  (suite {:?}, {} case(s))",
            file.display(),
            suite.name,
            suite.cases.len()
        );
    }
    println!(
        "Checked {} suite(s), {} total test case(s).",
        compiled.len(),
        total_cases
    );
    Ok(())
}

fn resolve_daemon_connection(
    ws_url: Option<String>,
    token: Option<String>,
) -> anyhow::Result<(String, String)> {
    match (ws_url, token) {
        (Some(u), Some(t)) => Ok((u, t)),
        (opt_u, opt_t) => {
            let conn_path = default_connection_file();
            let conn = read_connection_file(&conn_path).map_err(|err| {
                anyhow::anyhow!(
                    "could not read Ferrite daemon connection file at {} ({err}).\n\
                     Make sure:\n\
                     1. Chrome is running with the Ferrite E2E extension enabled, and\n\
                     2. You have run `ferrite setup --extension-id <32-char-id>` (or pass `--ws-url` and `--token`).",
                    conn_path.display()
                )
            })?;
            Ok((
                opt_u.unwrap_or_else(|| format!("ws://127.0.0.1:{}", conn.port)),
                opt_t.unwrap_or(conn.token),
            ))
        }
    }
}

fn print_human_report(reports: &[SuiteReport]) {
    let mut total_cases = 0usize;
    let mut passed_cases = 0usize;
    let mut failed_cases = 0usize;
    let mut total_ms = 0u64;

    for report in reports {
        total_cases += report.total_cases;
        passed_cases += report.passed_cases;
        failed_cases += report.failed_cases;
        total_ms += report.duration_ms;

        let suite_status = if report.passed { "PASS" } else { "FAIL" };
        println!(
            "[{suite_status}] suite {:?} ({}/{} passed, {}ms)",
            report.suite, report.passed_cases, report.total_cases, report.duration_ms
        );
        for case in &report.cases {
            let case_status = if case.passed { "PASS" } else { "FAIL" };
            println!(
                "  [{case_status}] {} ({} step(s), {}ms)",
                case.name,
                case.steps.len(),
                case.duration_ms
            );
            if !case.passed
                && let Some(failed_step) = case.steps.iter().find(|s| !s.passed)
            {
                println!(
                    "         step #{} `{}` failed: {}",
                    failed_step.index + 1,
                    failed_step.action,
                    failed_step.error.as_deref().unwrap_or("unknown error")
                );
            }
        }
    }

    println!(
        "\nSummary: {} suite(s), {passed_cases}/{total_cases} case(s) passed, {failed_cases} failed ({total_ms}ms)",
        reports.len()
    );
}

async fn run_test_suites(
    path: PathBuf,
    ws_url: Option<String>,
    token: Option<String>,
    update_baselines: bool,
    snapshots_dir: Option<PathBuf>,
    json_output: bool,
    headless: bool,
    chrome_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let compiled = compile_path(&path)?;
    if compiled.is_empty() {
        anyhow::bail!("no runnable test cases found under {}", path.display());
    }

    let resolved_snapshots = match snapshots_dir {
        Some(dir) => dir,
        None => match load_project_config(&path)? {
            Some((root, cfg)) => root.join(cfg.snapshots_dir),
            None => std::env::current_dir()?.join(".ferrite-snapshots"),
        },
    };

    let mut reports: Vec<SuiteReport> = Vec::with_capacity(compiled.len());
    if headless {
        let transport = HeadlessCdpTransport::launch(HeadlessOptions {
            chrome_path,
            ..Default::default()
        })
        .await?;
        let session = E2eSession::new(transport.clone(), &resolved_snapshots);

        for (_, suite) in compiled {
            let report = run_suite(&session, &suite, update_baselines).await;
            reports.push(report);
        }

        transport.shutdown().await;
    } else {
        let (resolved_url, resolved_token) = resolve_daemon_connection(ws_url, token)?;

        for (_, suite) in compiled {
            let report = run_suite_over_ws_with_snapshots(
                &resolved_url,
                &resolved_token,
                &suite,
                update_baselines,
                Some(&resolved_snapshots),
            )
            .await
            .map_err(|err| {
                anyhow::anyhow!(
                    "failed executing suite {:?} against {resolved_url}: {err}\n\
                     Hint: verify the Ferrite Chrome extension shows 'Connected' (click Reconnect Native Host in the popup if needed), \
                     or pass `--headless` to run without the extension.",
                    suite.name
                )
            })?;
            reports.push(report);
        }
    }

    let all_passed = reports.iter().all(|r| r.passed);
    if json_output {
        if reports.len() == 1 {
            println!("{}", serde_json::to_string_pretty(&reports[0])?);
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "passed": all_passed,
                    "suites": reports,
                }))?
            );
        }
    } else {
        print_human_report(&reports);
    }

    if !all_passed {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_record(
    url: Option<String>,
    suite: String,
    case_name: String,
    out: Option<PathBuf>,
    mask_passwords: bool,
    ws_url: Option<String>,
    token: Option<String>,
) -> anyhow::Result<()> {
    let (resolved_url, resolved_token) = resolve_daemon_connection(ws_url, token)?;

    if let Some(ref target_url) = url {
        let _ = send_ws_rpc(
            &resolved_url,
            &resolved_token,
            "goto",
            json!({ "url": target_url, "waitUntil": "load", "timeoutMs": 15_000 }),
        )
        .await?;
    }

    let _ = send_ws_rpc(
        &resolved_url,
        &resolved_token,
        "record.start",
        json!({ "maskPasswords": mask_passwords }),
    )
    .await?;

    eprintln!("Recording Chrome interactions... Perform your flow in Chrome, then press Ctrl-C to generate `.ferrite` code.");
    tokio::signal::ctrl_c().await?;

    let stopped = send_ws_rpc(
        &resolved_url,
        &resolved_token,
        "record.stop",
        json!({ "suiteName": suite, "caseName": case_name }),
    )
    .await?;

    let script = stopped
        .get("ferriteScript")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if let Some(dest) = out {
        if let Some(parent) = dest.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, script)?;
        println!("Saved recorded .ferrite test to {}", dest.display());
    } else {
        println!("{script}");
    }
    Ok(())
}

fn validate_extension_id(extension_id: &str) -> anyhow::Result<()> {
    let valid_id = extension_id.len() == 32
        && extension_id
            .bytes()
            .all(|b| (b'a'..=b'p').contains(&b));
    if !valid_id {
        anyhow::bail!(
            "invalid Chrome extension ID {extension_id:?}: expected 32 lowercase characters in 'a'..='p'"
        );
    }
    Ok(())
}

fn build_manifest_json(extension_id: &str, binary_path: Option<PathBuf>) -> anyhow::Result<String> {
    validate_extension_id(extension_id)?;
    let path = match binary_path {
        Some(p) => p,
        None => std::env::current_exe()?,
    };
    let canonical = fs::canonicalize(&path).unwrap_or(path);
    // Strip Windows UNC verbatim prefix `\\?\` if present so Chrome Native Messaging accepts the path.
    let path_str = canonical.to_string_lossy();
    let clean_path = path_str.strip_prefix(r"\\?\").unwrap_or(&path_str);

    let manifest = json!({
        "name": NATIVE_HOST_NAME,
        "description": "Ferrite E2E Native Messaging Host (CDP + Tokio Automation Daemon)",
        "path": clean_path,
        "type": "stdio",
        "allowed_origins": [
            format!("chrome-extension://{extension_id}/")
        ]
    });

    Ok(serde_json::to_string_pretty(&manifest)?)
}

fn run_setup(
    extension_id: Option<String>,
    browser: BrowserKind,
    binary_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let ext_id = match extension_id {
        Some(id) => id,
        None => std::env::var("FERRITE_EXTENSION_ID").map_err(|_| {
            anyhow::anyhow!(
                "missing --extension-id <32-char-id> (or FERRITE_EXTENSION_ID environment variable)"
            )
        })?,
    };

    let manifest_json = build_manifest_json(&ext_id, binary_path)?;

    #[cfg(windows)]
    {
        let local_app_data = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("LOCALAPPDATA environment variable is not set"))?;
        let install_dir = local_app_data.join("FerriteE2E");
        fs::create_dir_all(&install_dir)?;
        let manifest_path = install_dir.join(format!("{NATIVE_HOST_NAME}.json"));
        fs::write(&manifest_path, &manifest_json)?;

        let reg_path = match browser {
            BrowserKind::Chrome => format!(r"HKCU\Software\Google\Chrome\NativeMessagingHosts\{NATIVE_HOST_NAME}"),
            BrowserKind::Edge => format!(r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts\{NATIVE_HOST_NAME}"),
            BrowserKind::Brave => format!(r"HKCU\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\{NATIVE_HOST_NAME}"),
            BrowserKind::Chromium => format!(r"HKCU\Software\Chromium\NativeMessagingHosts\{NATIVE_HOST_NAME}"),
        };

        let status = std::process::Command::new("reg.exe")
            .args([
                "add",
                &reg_path,
                "/ve",
                "/t",
                "REG_SZ",
                "/d",
                &manifest_path.to_string_lossy(),
                "/f",
            ])
            .status()?;

        if !status.success() {
            anyhow::bail!("reg.exe failed while registering {reg_path}");
        }

        println!("Registered Ferrite Native Messaging manifest:");
        println!("  manifest: {}", manifest_path.display());
        println!("  registry: {reg_path}");
    }

    #[cfg(not(windows))]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("HOME environment variable is not set"))?;

        #[cfg(target_os = "macos")]
        let base_dir = match browser {
            BrowserKind::Chrome => home.join("Library/Application Support/Google/Chrome/NativeMessagingHosts"),
            BrowserKind::Chromium => home.join("Library/Application Support/Chromium/NativeMessagingHosts"),
            BrowserKind::Brave => home.join("Library/Application Support/BraveSoftware/Brave-Browser/NativeMessagingHosts"),
            BrowserKind::Edge => home.join("Library/Application Support/Microsoft Edge/NativeMessagingHosts"),
        };

        #[cfg(not(target_os = "macos"))]
        let base_dir = match browser {
            BrowserKind::Chrome => home.join(".config/google-chrome/NativeMessagingHosts"),
            BrowserKind::Chromium => home.join(".config/chromium/NativeMessagingHosts"),
            BrowserKind::Brave => home.join(".config/BraveSoftware/Brave-Browser/NativeMessagingHosts"),
            BrowserKind::Edge => home.join(".config/microsoft-edge/NativeMessagingHosts"),
        };

        fs::create_dir_all(&base_dir)?;
        let manifest_path = base_dir.join(format!("{NATIVE_HOST_NAME}.json"));
        fs::write(&manifest_path, &manifest_json)?;
        println!("Installed Ferrite Native Messaging manifest at: {}", manifest_path.display());
    }

    Ok(())
}

fn print_manifest(extension_id: &str, binary_path: Option<PathBuf>) -> anyhow::Result<()> {
    println!("{}", build_manifest_json(extension_id, binary_path)?);
    Ok(())
}
