#![forbid(unsafe_code)]

use std::fs;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ferrite_cdp::{LoadState, MockCdpTransport};
use ferrite_core::{CdpEvent, TabInfo};
use ferrite_daemon::{
    E2eSession, TestCase, TestStep, TestSuite, WsServer, generate_session_token,
    run_suite_over_ws,
};
use ferrite_visual::{RgbaImage, encode_png};
use serde_json::json;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn solid_png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
    let img = RgbaImage {
        width,
        height,
        data: rgba
            .iter()
            .copied()
            .cycle()
            .take((width * height * 4) as usize)
            .collect(),
    };
    encode_png(&img).unwrap()
}

#[tokio::test]
async fn ws_gateway_rejects_cswsh_origin_and_bad_token_and_runs_e2e_suite() {
    let mock = MockCdpTransport::new();
    let temp_dir = std::env::temp_dir().join(format!("ferrite-e2e-ws-{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);

    let session = E2eSession::new(mock.clone(), &temp_dir);
    let token = generate_session_token().unwrap();
    let server = WsServer::bind(0, session, token.clone()).await.unwrap();
    let ws_url = server.ws_url();

    // 1. CSWSH check: valid token + Origin header must be rejected with HTTP 403.
    let mut evil_req = ws_url.clone().into_client_request().unwrap();
    evil_req
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    evil_req
        .headers_mut()
        .insert("Origin", "https://evil.example".parse().unwrap());
    let evil_err = connect_async(evil_req).await.unwrap_err();
    match evil_err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => {
            assert_eq!(resp.status().as_u16(), 403);
        }
        other => panic!("expected HTTP 403 error, got {other:?}"),
    }

    // 2. Unauthorized check: wrong bearer token must be rejected with HTTP 401.
    let mut bad_token_req = ws_url.clone().into_client_request().unwrap();
    bad_token_req
        .headers_mut()
        .insert("Authorization", "Bearer wrong-token".parse().unwrap());
    let unauth_err = connect_async(bad_token_req).await.unwrap_err();
    match unauth_err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => {
            assert_eq!(resp.status().as_u16(), 401);
        }
        other => panic!("expected HTTP 401 error, got {other:?}"),
    }

    // 3. Full E2E suite execution over authenticated WebSocket.
    let screenshot_b64 = STANDARD.encode(solid_png(16, 16, [20, 120, 220, 255]));
    let mock_clone = Arc::clone(&mock);
    mock.add_handler(move |tab_id, method, _params, _session| match method {
        "Page.navigate" => {
            let emitter = mock_clone.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                emitter.emit_event(CdpEvent {
                    tab_id,
                    method: "Page.loadEventFired".to_string(),
                    params: json!({ "timestamp": 1.0 }),
                    session_id: None,
                });
            });
            Some(Ok(json!({ "frameId": "main" })))
        }
        "Accessibility.getFullAXTree" => Some(Ok(json!({
            "nodes": [
                {
                    "nodeId": "1",
                    "role": { "type": "role", "value": "RootWebArea" },
                    "childIds": ["2", "3"]
                },
                {
                    "nodeId": "2",
                    "parentId": "1",
                    "role": { "type": "role", "value": "button" },
                    "name": { "type": "computedString", "value": "Continue" },
                    "backendDOMNodeId": 101,
                    "childIds": []
                },
                {
                    "nodeId": "3",
                    "parentId": "1",
                    "role": { "type": "role", "value": "heading" },
                    "name": { "type": "computedString", "value": "Dashboard Ready" },
                    "backendDOMNodeId": 102,
                    "childIds": []
                }
            ]
        }))),
        "DOM.getBoxModel" => Some(Ok(json!({
            "model": { "content": [10.0, 10.0, 90.0, 10.0, 90.0, 40.0, 10.0, 40.0] }
        }))),
        "Page.captureScreenshot" => Some(Ok(json!({ "data": screenshot_b64 }))),
        _ => None,
    });

    let suite = TestSuite {
        name: "checkout-smoke".to_string(),
        cases: vec![TestCase {
            name: "navigate-click-and-visual-check".to_string(),
            tab_id: Some(1),
            steps: vec![
                TestStep::Goto {
                    url: "https://app.example/checkout".to_string(),
                    wait_until: LoadState::Load,
                    timeout_ms: 2_000,
                },
                TestStep::Snapshot {
                    interactive_only: false,
                },
                TestStep::ClickRef {
                    ref_id: "e1".to_string(),
                },
                TestStep::AssertVisible {
                    text: "Dashboard Ready".to_string(),
                    exact: true,
                    timeout_ms: 2_000,
                },
                TestStep::AssertScreenshot {
                    name: "checkout-home".to_string(),
                    max_diff_ratio: None,
                    threshold: None,
                },
                // Second assertion compares against the newly saved baseline.
                TestStep::AssertScreenshot {
                    name: "checkout-home".to_string(),
                    max_diff_ratio: None,
                    threshold: None,
                },
            ],
        }],
    };

    let report = run_suite_over_ws(&ws_url, &token, &suite, false)
        .await
        .unwrap();
    assert!(report.passed, "suite failed: {report:?}");
    assert_eq!(report.passed_cases, 1);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn skips_restricted_chrome_urls_and_opens_clean_tab() {
    let mock = MockCdpTransport::new();
    mock.set_tabs(vec![
        TabInfo {
            tab_id: 10,
            window_id: 1,
            url: "chrome://extensions/".to_string(),
            title: "Extensions".to_string(),
            active: true,
            attached: false,
        },
        TabInfo {
            tab_id: 11,
            window_id: 1,
            url: "chrome://newtab/".to_string(),
            title: "New Tab".to_string(),
            active: false,
            attached: false,
        },
    ]);

    let temp_dir = std::env::temp_dir().join(format!("ferrite-e2e-tabs-{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let session = E2eSession::new(mock.clone(), &temp_dir);

    let resolved = session.resolve_tab(None).await.unwrap();
    assert_eq!(resolved, 12, "must create clean tab instead of attaching chrome://extensions/");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn sample_e2e_suite_matches_test_suite_schema() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let sample_path = manifest_dir.join("..").join("..").join("examples").join("sample-e2e-suite.json");
    let raw = fs::read_to_string(&sample_path).unwrap();
    let suite: TestSuite = serde_json::from_str(&raw).unwrap();
    assert!(!suite.cases.is_empty());
}
