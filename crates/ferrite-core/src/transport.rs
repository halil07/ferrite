#![forbid(unsafe_code)]

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::error::CdpError;
use crate::protocol::TabInfo;

/// An incoming CDP event emitted from a debugged Chrome tab or child target session (e.g., OOPIF).
#[derive(Debug, Clone, PartialEq)]
pub struct CdpEvent {
    pub tab_id: i64,
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

/// Abstract CDP transport implemented by both the Native Messaging bridge (`ferrite-ipc`)
/// and deterministic test doubles (`MockCdpTransport`).
#[async_trait]
pub trait CdpTransport: Send + Sync {
    async fn send_cdp(
        &self,
        tab_id: i64,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, CdpError>;

    async fn attach_tab(&self, tab_id: i64) -> Result<(), CdpError>;

    async fn detach_tab(&self, tab_id: i64) -> Result<(), CdpError>;

    async fn list_tabs(&self) -> Result<Vec<TabInfo>, CdpError>;

    async fn create_tab(&self, url: &str, active: bool) -> Result<TabInfo, CdpError>;

    async fn close_tab(&self, tab_id: i64) -> Result<(), CdpError>;

    fn subscribe_events(&self) -> broadcast::Receiver<CdpEvent>;
}
