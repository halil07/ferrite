#![forbid(unsafe_code)]

//! Typed Chrome DevTools Protocol (CDP) domain managers for `Page`, `Target`, `Network`, and `Fetch`.

pub mod console;
pub mod mock;
pub mod network;
pub mod page;
pub mod recorder;
pub mod target;

pub use console::{ConsoleCollector, ConsoleEntry, ConsoleLevel, format_console_entries};
pub use mock::{MockCdpTransport, RecordedCdpCall};
pub use network::{
    CapturedExchange, FetchInterceptor, MockRouteResponse, NetworkConditions, NetworkDomain,
    NetworkMonitor, ResponseBody,
};
pub use page::{
    FrameInfo, FrameTree, LoadState, PageDomain, ScreenshotClip, ScreenshotFormat,
    ScreenshotOptions,
};
pub use recorder::{
    ActionRecorder, RecordedAction, build_capture_script, compress_recorded_actions,
    render_ferrite_script,
};
pub use target::{ChildSession, TargetDomain, TargetInfo, TargetSessionRegistry};
