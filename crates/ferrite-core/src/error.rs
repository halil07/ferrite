#![forbid(unsafe_code)]

use thiserror::Error;

/// Typed errors returned across the CDP boundary, transport bridge, and locator/domain layers.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum CdpError {
    #[error("CDP protocol error{}: {message}", code.map(|c| format!(" ({c})")).unwrap_or_default())]
    Protocol { code: Option<i64>, message: String },

    #[error("debugger detached from target: {0}")]
    Detached(String),

    #[error("target tab or session was closed")]
    TargetClosed,

    #[error("native messaging bridge disconnected")]
    Disconnected,

    #[error("CDP command timed out after {0} ms")]
    Timeout(u64),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("DOM/AX node is stale and must be re-resolved")]
    StaleNode,
}

/// Maps Chrome DevTools Protocol error messages into typed [`CdpError`] variants.
pub fn classify_cdp_error(message: impl Into<String>, code: Option<i64>) -> CdpError {
    let message = message.into();

    if message.contains("Detached while handling command")
        || message.contains("Debugger is not attached")
    {
        return CdpError::Detached(message);
    }

    if message.contains("Target closed")
        || message.contains("No target with given id")
        || message.contains("Session closed")
        || message.contains("No session with given id")
    {
        return CdpError::TargetClosed;
    }

    if is_stale_cdp_message(&message) {
        return CdpError::StaleNode;
    }

    CdpError::Protocol { code, message }
}

fn is_stale_cdp_message(message: &str) -> bool {
    [
        "Could not find node with given id",
        "No node with given id found",
        "Cannot find context with specified id",
        "Execution context was destroyed",
        "Cannot find default execution context",
        "Inspected target navigated or closed",
        "Could not find object with given id",
        "No frame with given id",
        "Frame with the given id was not found",
        "Cannot find object with id",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_detached_closed_and_stale() {
        assert!(matches!(
            classify_cdp_error("Detached while handling command.", None),
            CdpError::Detached(_)
        ));
        assert!(matches!(
            classify_cdp_error("Target closed", Some(-32000)),
            CdpError::TargetClosed
        ));
        assert!(matches!(
            classify_cdp_error("Could not find node with given id", Some(-32000)),
            CdpError::StaleNode
        ));
        assert!(matches!(
            classify_cdp_error("Execution context was destroyed.", None),
            CdpError::StaleNode
        ));
        assert!(matches!(
            classify_cdp_error("Method not found", Some(-32601)),
            CdpError::Protocol {
                code: Some(-32601),
                ..
            }
        ));
    }
}
