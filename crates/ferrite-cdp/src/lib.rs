#![forbid(unsafe_code)]

//! Typed Chrome DevTools Protocol (CDP) domain managers for `Page`, `Target`, `Network`, and `Fetch`.

pub mod mock;
pub mod network;
pub mod page;
pub mod target;

pub use mock::{MockCdpTransport, RecordedCdpCall};
pub use network::{
    CapturedExchange, FetchInterceptor, MockRouteResponse, NetworkConditions, NetworkDomain,
    NetworkMonitor, ResponseBody,
};
pub use page::{
    FrameInfo, FrameTree, LoadState, PageDomain, ScreenshotClip, ScreenshotFormat,
    ScreenshotOptions,
};
pub use target::{ChildSession, TargetDomain, TargetInfo, TargetSessionRegistry};
