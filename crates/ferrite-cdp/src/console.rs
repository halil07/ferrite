#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ferrite_core::{CdpError, CdpEvent, CdpTransport};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

pub const MAX_CONSOLE_BUFFER: usize = 500;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleLevel {
    Log,
    Info,
    Warn,
    Error,
    Debug,
    Exception,
    Browser,
}

impl ConsoleLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Debug => "debug",
            Self::Exception => "exception",
            Self::Browser => "browser",
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error | Self::Exception)
    }

    pub fn parse_level(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "log" => Some(Self::Log),
            "info" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "error" => Some(Self::Error),
            "debug" => Some(Self::Debug),
            "exception" => Some(Self::Exception),
            "browser" => Some(Self::Browser),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleEntry {
    pub id: u64,
    pub level: ConsoleLevel,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_trace: Option<String>,
}

#[derive(Default)]
struct CollectorState {
    next_id: u64,
    entries: VecDeque<ConsoleEntry>,
}

/// Captures `Runtime.consoleAPICalled`, `Runtime.exceptionThrown`, and `Log.entryAdded` events
/// into a bounded ring buffer for E2E console & zero-error assertions.
pub struct ConsoleCollector {
    state: Mutex<CollectorState>,
}

impl ConsoleCollector {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CollectorState {
                next_id: 1,
                entries: VecDeque::new(),
            }),
        })
    }

    pub async fn enable(transport: &Arc<dyn CdpTransport>, tab_id: i64) -> Result<(), CdpError> {
        transport
            .send_cdp(tab_id, "Runtime.enable", json!({}), None)
            .await?;
        let _ = transport
            .send_cdp(tab_id, "Log.enable", json!({}), None)
            .await;
        Ok(())
    }

    pub fn spawn_listener(
        self: &Arc<Self>,
        transport: Arc<dyn CdpTransport>,
        tab_id: i64,
    ) -> JoinHandle<()> {
        let mut rx = transport.subscribe_events();
        let collector = self.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.tab_id == tab_id => collector.ingest_event(&ev),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    pub fn ingest_event(&self, event: &CdpEvent) {
        match event.method.as_str() {
            "Runtime.consoleAPICalled" => {
                let level = match event
                    .params
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("log")
                {
                    "warning" | "warn" => ConsoleLevel::Warn,
                    "error" | "assert" => ConsoleLevel::Error,
                    "info" => ConsoleLevel::Info,
                    "debug" => ConsoleLevel::Debug,
                    _ => ConsoleLevel::Log,
                };
                let text = event
                    .params
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|args| {
                        args.iter()
                            .map(format_remote_object)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let stack = event.params.get("stackTrace");
                let (url, line_number) = top_frame_location(stack);
                let stack_trace = if level.is_error() {
                    format_stack_trace(stack)
                } else {
                    None
                };
                self.push(level, truncate(&text, 4000), url, line_number, stack_trace);
            }
            "Runtime.exceptionThrown" => {
                let details = &event.params["exceptionDetails"];
                let text = details
                    .get("exception")
                    .and_then(|e| e.get("description"))
                    .and_then(Value::as_str)
                    .or_else(|| details.get("text").and_then(Value::as_str))
                    .unwrap_or("Uncaught exception");
                let url = details
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let line_number = details
                    .get("lineNumber")
                    .and_then(Value::as_u64)
                    .map(|l| l + 1);
                let stack_trace = format_stack_trace(details.get("stackTrace"));
                self.push(
                    ConsoleLevel::Exception,
                    truncate(text, 4000),
                    url,
                    line_number,
                    stack_trace,
                );
            }
            "Log.entryAdded" => {
                let entry = &event.params["entry"];
                let level = match entry.get("level").and_then(Value::as_str).unwrap_or("") {
                    "error" => ConsoleLevel::Error,
                    "warning" => ConsoleLevel::Warn,
                    _ => ConsoleLevel::Browser,
                };
                let text = entry
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let url = entry.get("url").and_then(Value::as_str).map(str::to_string);
                let line_number = entry.get("lineNumber").and_then(Value::as_u64);
                let stack_trace = format_stack_trace(entry.get("stackTrace"));
                self.push(level, truncate(text, 4000), url, line_number, stack_trace);
            }
            _ => {}
        }
    }

    fn push(
        &self,
        level: ConsoleLevel,
        text: String,
        url: Option<String>,
        line_number: Option<u64>,
        stack_trace: Option<String>,
    ) {
        let mut guard = self.state.lock().expect("console mutex poisoned");
        let id = guard.next_id;
        guard.next_id += 1;
        guard.entries.push_back(ConsoleEntry {
            id,
            level,
            text,
            url,
            line_number,
            stack_trace,
        });
        while guard.entries.len() > MAX_CONSOLE_BUFFER {
            guard.entries.pop_front();
        }
    }

    pub fn entries(&self) -> Vec<ConsoleEntry> {
        self.state
            .lock()
            .expect("console mutex poisoned")
            .entries
            .iter()
            .cloned()
            .collect()
    }

    pub fn errors(&self) -> Vec<ConsoleEntry> {
        self.state
            .lock()
            .expect("console mutex poisoned")
            .entries
            .iter()
            .filter(|e| e.level.is_error())
            .cloned()
            .collect()
    }

    pub fn clear(&self) {
        self.state
            .lock()
            .expect("console mutex poisoned")
            .entries
            .clear();
    }
}

pub fn format_console_entries(entries: &[ConsoleEntry]) -> String {
    if entries.is_empty() {
        return "(no console entries)".to_string();
    }
    entries
        .iter()
        .map(|e| {
            let loc = match (&e.url, e.line_number) {
                (Some(u), Some(l)) => format!(" ({u}:{l})"),
                (Some(u), None) => format!(" ({u})"),
                _ => String::new(),
            };
            let stack = e
                .stack_trace
                .as_deref()
                .map(|s| format!("\n{s}"))
                .unwrap_or_default();
            format!(
                "[{}] {}{loc}{stack}",
                e.level.as_str().to_ascii_uppercase(),
                e.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_remote_object(obj: &Value) -> String {
    if let Some(val) = obj.get("value") {
        return match val {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    if let Some(desc) = obj.get("description").and_then(Value::as_str) {
        return desc.to_string();
    }
    obj.get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn top_frame_location(stack: Option<&Value>) -> (Option<String>, Option<u64>) {
    let Some(frame) = stack
        .and_then(|s| s.get("callFrames"))
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
    else {
        return (None, None);
    };
    let url = frame.get("url").and_then(Value::as_str).map(str::to_string);
    let line = frame
        .get("lineNumber")
        .and_then(Value::as_u64)
        .map(|l| l + 1);
    (url, line)
}

fn format_stack_trace(stack: Option<&Value>) -> Option<String> {
    let frames = stack?.get("callFrames")?.as_array()?;
    if frames.is_empty() {
        return None;
    }
    let lines: Vec<String> = frames
        .iter()
        .take(10)
        .map(|f| {
            let func = f
                .get("functionName")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("(anonymous)");
            let url = f
                .get("url")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("<eval>");
            let line = f.get("lineNumber").and_then(Value::as_u64).unwrap_or(0) + 1;
            let col = f.get("columnNumber").and_then(Value::as_u64).unwrap_or(0) + 1;
            format!("  at {func} ({url}:{line}:{col})")
        })
        .collect();
    Some(lines.join("\n"))
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_console_errors_exceptions_and_browser_logs() {
        let collector = ConsoleCollector::new();
        collector.ingest_event(&CdpEvent {
            tab_id: 1,
            method: "Runtime.consoleAPICalled".to_string(),
            params: json!({
                "type": "info",
                "args": [{ "type": "string", "value": "app booted" }]
            }),
            session_id: None,
        });
        collector.ingest_event(&CdpEvent {
            tab_id: 1,
            method: "Runtime.exceptionThrown".to_string(),
            params: json!({
                "exceptionDetails": {
                    "text": "Uncaught",
                    "url": "https://app.example/main.js",
                    "lineNumber": 41,
                    "exception": { "description": "TypeError: Cannot read properties of null" }
                }
            }),
            session_id: None,
        });
        collector.ingest_event(&CdpEvent {
            tab_id: 1,
            method: "Log.entryAdded".to_string(),
            params: json!({
                "entry": {
                    "level": "error",
                    "text": "Failed to load resource: 500",
                    "url": "https://app.example/api"
                }
            }),
            session_id: None,
        });

        assert_eq!(collector.entries().len(), 3);
        let errs = collector.errors();
        assert_eq!(errs.len(), 2);
        let formatted = format_console_entries(&errs);
        assert!(formatted.contains("TypeError: Cannot read properties of null"));
        assert!(formatted.contains("Failed to load resource: 500"));

        collector.clear();
        assert!(collector.entries().is_empty());
    }
}
