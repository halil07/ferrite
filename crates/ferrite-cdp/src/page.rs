#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ferrite_core::{CdpError, CdpTransport};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LoadState {
    Commit,
    DomContentLoaded,
    Load,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenshotFormat {
    Png,
    Jpeg,
    Webp,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScreenshotClip {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScreenshotOptions {
    pub format: ScreenshotFormat,
    pub quality: Option<u8>,
    pub clip: Option<ScreenshotClip>,
    pub from_surface: bool,
    pub capture_beyond_viewport: bool,
}

impl Default for ScreenshotOptions {
    fn default() -> Self {
        Self {
            format: ScreenshotFormat::Png,
            quality: None,
            clip: None,
            from_surface: true,
            capture_beyond_viewport: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FrameInfo {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub security_origin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FrameTree {
    pub frame: FrameInfo,
    #[serde(default)]
    pub child_frames: Vec<FrameTree>,
}

/// Normalizes `about:blank#...` URLs to plain `about:blank` to avoid a known Chromium CDP crash
/// when navigating `about:blank` with URL fragments.
pub fn normalize_navigation_url(raw: &str) -> String {
    if let Ok(parsed) = Url::parse(raw)
        && parsed.scheme().eq_ignore_ascii_case("about")
        && parsed.path().eq_ignore_ascii_case("blank")
        && parsed.query().is_none()
        && parsed.fragment().is_some()
    {
        return "about:blank".to_string();
    }
    raw.to_string()
}

/// High-level typed wrapper for the CDP `Page` domain on a single tab.
pub struct PageDomain {
    transport: Arc<dyn CdpTransport>,
    tab_id: i64,
}

impl PageDomain {
    pub fn new(transport: Arc<dyn CdpTransport>, tab_id: i64) -> Self {
        Self { transport, tab_id }
    }

    pub fn tab_id(&self) -> i64 {
        self.tab_id
    }

    pub async fn enable(&self) -> Result<(), CdpError> {
        self.transport
            .send_cdp(self.tab_id, "Page.enable", json!({}), None)
            .await?;
        Ok(())
    }

    pub async fn navigate(
        &self,
        url: &str,
        wait_until: LoadState,
        timeout: Duration,
    ) -> Result<String, CdpError> {
        let mut events = self.transport.subscribe_events();
        let normalized = normalize_navigation_url(url);

        let result = self
            .transport
            .send_cdp(self.tab_id, "Page.navigate", json!({ "url": normalized }), None)
            .await?;

        if let Some(err_text) = result.get("errorText").and_then(Value::as_str)
            && !err_text.is_empty()
        {
            return Err(CdpError::Protocol {
                code: None,
                message: format!("navigation failed: {err_text}"),
            });
        }

        let frame_id = result
            .get("frameId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        if wait_until == LoadState::Commit {
            return Ok(frame_id);
        }

        let target_event = match wait_until {
            LoadState::DomContentLoaded => "Page.domContentLoadedEventFired",
            LoadState::Load => "Page.loadEventFired",
            LoadState::Commit => unreachable!(),
        };

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(CdpError::Timeout(timeout.as_millis() as u64));
            }
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(ev)) => {
                    if ev.tab_id == self.tab_id && ev.session_id.is_none() {
                        if ev.method == target_event {
                            return Ok(frame_id);
                        }
                        if ev.method == "Inspector.detached" {
                            let reason = ev
                                .params
                                .get("reason")
                                .and_then(Value::as_str)
                                .unwrap_or("detached")
                                .to_string();
                            return Err(CdpError::Detached(reason));
                        }
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    return Err(CdpError::Disconnected);
                }
                Err(_) => return Err(CdpError::Timeout(timeout.as_millis() as u64)),
            }
        }
    }

    pub async fn reload(&self, ignore_cache: bool) -> Result<(), CdpError> {
        self.transport
            .send_cdp(
                self.tab_id,
                "Page.reload",
                json!({ "ignoreCache": ignore_cache }),
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn get_frame_tree(&self) -> Result<FrameTree, CdpError> {
        let res = self
            .transport
            .send_cdp(self.tab_id, "Page.getFrameTree", json!({}), None)
            .await?;
        let tree_val = res.get("frameTree").cloned().unwrap_or(res);
        serde_json::from_value(tree_val).map_err(|err| CdpError::Protocol {
            code: None,
            message: format!("failed to decode Page.getFrameTree response: {err}"),
        })
    }

    pub async fn capture_screenshot(&self, opts: &ScreenshotOptions) -> Result<Vec<u8>, CdpError> {
        let mut params = json!({
            "format": opts.format,
            "fromSurface": opts.from_surface,
            "captureBeyondViewport": opts.capture_beyond_viewport,
        });
        if let Some(q) = opts.quality
            && matches!(opts.format, ScreenshotFormat::Jpeg | ScreenshotFormat::Webp)
        {
            params["quality"] = json!(q.min(100));
        }
        if let Some(clip) = opts.clip {
            params["clip"] = json!({
                "x": clip.x,
                "y": clip.y,
                "width": clip.width,
                "height": clip.height,
                "scale": clip.scale,
            });
        }

        let res = self
            .transport
            .send_cdp(self.tab_id, "Page.captureScreenshot", params, None)
            .await?;

        let b64 = res
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| CdpError::Protocol {
                code: None,
                message: "Page.captureScreenshot omitted base64 `data` field".to_string(),
            })?;

        STANDARD.decode(b64.as_bytes()).map_err(|err| CdpError::Protocol {
            code: None,
            message: format!("invalid base64 screenshot payload: {err}"),
        })
    }

    /// Creates an isolated V8 execution world inside a frame without calling `Runtime.enable`,
    /// avoiding stealth detection side effects and leaving page globals untouched.
    pub async fn create_isolated_world(
        &self,
        frame_id: &str,
        world_name: &str,
        session_id: Option<&str>,
    ) -> Result<i64, CdpError> {
        let res = self
            .transport
            .send_cdp(
                self.tab_id,
                "Page.createIsolatedWorld",
                json!({
                    "frameId": frame_id,
                    "worldName": world_name,
                    "grantUniveralAccess": true,
                }),
                session_id,
            )
            .await?;

        res.get("executionContextId")
            .and_then(Value::as_i64)
            .ok_or_else(|| CdpError::Protocol {
                code: None,
                message: "Page.createIsolatedWorld omitted executionContextId".to_string(),
            })
    }

    pub async fn handle_javascript_dialog(
        &self,
        accept: bool,
        prompt_text: Option<&str>,
    ) -> Result<(), CdpError> {
        let mut params = json!({ "accept": accept });
        if let Some(text) = prompt_text {
            params["promptText"] = json!(text);
        }
        self.transport
            .send_cdp(
                self.tab_id,
                "Page.handleJavaScriptDialog",
                params,
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn add_script_to_evaluate_on_new_document(
        &self,
        source: &str,
    ) -> Result<String, CdpError> {
        let res = self
            .transport
            .send_cdp(
                self.tab_id,
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": source }),
                None,
            )
            .await?;
        Ok(res
            .get("identifier")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use ferrite_core::CdpEvent;

    use super::*;
    use crate::mock::MockCdpTransport;

    #[test]
    fn normalizes_about_blank_fragments() {
        assert_eq!(normalize_navigation_url("about:blank#hash"), "about:blank");
        assert_eq!(
            normalize_navigation_url("https://example.com/#hash"),
            "https://example.com/#hash"
        );
    }

    #[tokio::test]
    async fn navigate_waits_for_load_event_and_decodes_screenshot() {
        let mock = MockCdpTransport::new();
        let page = PageDomain::new(mock.clone(), 1);

        mock.push_ok("Page.navigate", json!({ "frameId": "main-frame" }));
        let mock_clone = mock.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(15)).await;
            mock_clone.emit_event(CdpEvent {
                tab_id: 1,
                method: "Page.loadEventFired".to_string(),
                params: json!({ "timestamp": 1.0 }),
                session_id: None,
            });
        });

        let frame_id = page
            .navigate("about:blank#step1", LoadState::Load, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(frame_id, "main-frame");

        let calls = mock.recorded_calls();
        assert_eq!(calls[0].method, "Page.navigate");
        assert_eq!(calls[0].params["url"], "about:blank");

        let raw_png = b"\x89PNG\r\n\x1a\nfake";
        mock.push_ok(
            "Page.captureScreenshot",
            json!({ "data": STANDARD.encode(raw_png) }),
        );
        let bytes = page
            .capture_screenshot(&ScreenshotOptions::default())
            .await
            .unwrap();
        assert_eq!(bytes, raw_png);
    }
}
