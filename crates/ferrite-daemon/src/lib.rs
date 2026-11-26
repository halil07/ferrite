#![forbid(unsafe_code)]

//! Tokio WebSocket gateway, authenticated session runtime, E2E test suite runner,
//! and CSWSH-hardened authentication primitives.

pub mod auth;
pub mod headless;
pub mod runner;
pub mod session;
pub mod ws_server;

pub use auth::{
    ConnectionInfo, constant_time_eq, default_connection_file, default_snapshots_dir,
    generate_session_token, read_connection_file, write_connection_file,
};
pub use headless::{HeadlessCdpTransport, HeadlessOptions, find_chrome_binary};
pub use runner::{
    CaseReport, StepResult, SuiteReport, TestCase, TestStep, TestSuite, run_suite,
    run_suite_over_ws, run_suite_over_ws_with_snapshots, send_ws_rpc,
};
pub use session::{E2eSession, is_attachable_url, is_blocked_cdp_method};
pub use ws_server::WsServer;
