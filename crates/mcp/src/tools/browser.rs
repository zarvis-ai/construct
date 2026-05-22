//! Chrome DevTools browser tools exposed through agentd-mcp so every harness
//! with injected MCP can browse and refresh the TUI browser preview overlay.

use agentd_client::Client;
use agentd_protocol::{BrowserPreview, SessionEvent};
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use image::GenericImageView;
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio_tungstenite::connect_async;

pub const DEFAULT_PORT: u16 = 9222;
pub const DEFAULT_HOST: &str = "127.0.0.1";
const MAX_TEXT_CHARS: usize = 12_000;

pub fn catalog() -> Vec<Value> {
    vec![
        tool(
            "browser_open",
            "Open a URL in Chrome via DevTools. Starts a separate debug-profile Chrome if needed. Emits a UI-only browser preview overlay for the calling agentd session.",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "port": { "type": "integer", "minimum": 1, "maximum": 65535, "default": DEFAULT_PORT },
                    "host": { "type": "string", "default": DEFAULT_HOST },
                    "start_if_needed": { "type": "boolean", "default": true },
                    "preview": { "type": "boolean", "default": true }
                },
                "required": ["url"]
            }),
        ),
        tool(
            "browser_inspect",
            "Inspect Chrome tabs through DevTools. With no selector, lists tabs. With tab_id/url_contains, returns title, URL, body text, links, and refreshes the browser preview overlay.",
            json!({
                "type": "object",
                "properties": {
                    "tab_id": { "type": "string" },
                    "url_contains": { "type": "string" },
                    "port": { "type": "integer", "minimum": 1, "maximum": 65535, "default": DEFAULT_PORT },
                    "host": { "type": "string", "default": DEFAULT_HOST },
                    "max_text_chars": { "type": "integer", "minimum": 0, "maximum": 50000, "default": MAX_TEXT_CHARS },
                    "preview": { "type": "boolean", "default": true }
                }
            }),
        ),
        tool(
            "browser_screenshot",
            "Capture a Chrome tab screenshot through DevTools, return screenshot metadata, and refresh the browser preview overlay.",
            json!({
                "type": "object",
                "properties": {
                    "tab_id": { "type": "string" },
                    "url_contains": { "type": "string" },
                    "port": { "type": "integer", "minimum": 1, "maximum": 65535, "default": DEFAULT_PORT },
                    "host": { "type": "string", "default": DEFAULT_HOST },
                    "full_page": { "type": "boolean", "default": false },
                    "preview": { "type": "boolean", "default": true }
                }
            }),
        ),
        tool(
            "browser_eval",
            "Evaluate JavaScript in a Chrome tab selected by tab_id or url_contains. Refreshes the browser preview overlay after evaluation.",
            json!({
                "type": "object",
                "properties": {
                    "script": { "type": "string" },
                    "tab_id": { "type": "string" },
                    "url_contains": { "type": "string" },
                    "port": { "type": "integer", "minimum": 1, "maximum": 65535, "default": DEFAULT_PORT },
                    "host": { "type": "string", "default": DEFAULT_HOST },
                    "preview": { "type": "boolean", "default": true }
                },
                "required": ["script"]
            }),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

pub async fn call(
    client: Arc<Client>,
    session_id: Option<&str>,
    name: &str,
    args: Value,
) -> Result<Value> {
    match name {
        "browser_open" => browser_open(client, session_id, args).await,
        "browser_inspect" => browser_inspect(client, session_id, args).await,
        "browser_screenshot" => browser_screenshot(client, session_id, args).await,
        "browser_eval" => browser_eval(client, session_id, args).await,
        _ => Err(anyhow!("unknown browser tool: {name}")),
    }
}

async fn browser_open(
    client: Arc<Client>,
    session_id: Option<&str>,
    input: Value,
) -> Result<Value> {
    let url = input
        .get("url")
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow!("missing 'url'"))?;
    let endpoint = Endpoint::from_input(&input);
    if input
        .get("start_if_needed")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
        && devtools_version(&endpoint).await.is_err()
    {
        start_chrome(&endpoint).await?;
        wait_for_devtools(&endpoint).await?;
    }
    let resp: Value = reqwest::Client::new()
        .put(format!("{}/json/new?{}", endpoint.base(), urlencoding(url)))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if should_emit_preview(&input) {
        let _ = emit_browser_preview_from_response(client, session_id, &resp).await;
    }
    Ok(resp)
}

async fn browser_inspect(
    client: Arc<Client>,
    session_id: Option<&str>,
    input: Value,
) -> Result<Value> {
    let endpoint = Endpoint::from_input(&input);
    let tabs = list_tabs(&endpoint).await?;
    let Some(tab) = select_tab(&tabs, &input) else {
        return Ok(Value::Array(tabs));
    };
    let max_text = input
        .get("max_text_chars")
        .and_then(|n| n.as_u64())
        .unwrap_or(MAX_TEXT_CHARS as u64)
        .min(50_000) as usize;
    let script = format!(
        r#"(() => {{
        const text = (document.body && document.body.innerText || '').slice(0, {max_text});
        const links = Array.from(document.querySelectorAll('a')).map(a => ({{
            text: (a.innerText || a.textContent || '').trim(),
            href: a.href
        }})).filter(x => x.text && x.href).slice(0, 80);
        return {{ title: document.title, url: location.href, text, links }};
    }})()"#
    );
    let value = cdp_eval(&tab.websocket_url, &script).await?;
    if should_emit_preview(&input) {
        let _ = emit_browser_preview_from_tab(client, session_id, &tab, false).await;
    }
    Ok(value)
}

async fn browser_screenshot(
    client: Arc<Client>,
    session_id: Option<&str>,
    input: Value,
) -> Result<Value> {
    let endpoint = Endpoint::from_input(&input);
    let tabs = list_tabs(&endpoint).await?;
    let tab = select_tab(&tabs, &input).ok_or_else(|| anyhow!("no matching tab"))?;
    let full_page = input
        .get("full_page")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let png = cdp_capture_screenshot(&tab.websocket_url, full_page).await?;
    let dims = image::load_from_memory(&png)?.dimensions();
    if should_emit_preview(&input) {
        emit_browser_preview(
            client,
            session_id,
            &tab.url,
            tab.title.clone(),
            png.clone(),
            dims,
        )
        .await?;
    }
    Ok(
        json!({ "screenshot": { "width": dims.0, "height": dims.1, "format": "png", "bytes": png.len() }}),
    )
}

async fn browser_eval(
    client: Arc<Client>,
    session_id: Option<&str>,
    input: Value,
) -> Result<Value> {
    let script = input
        .get("script")
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow!("missing 'script'"))?;
    let endpoint = Endpoint::from_input(&input);
    let tabs = list_tabs(&endpoint).await?;
    let tab = select_tab(&tabs, &input).ok_or_else(|| anyhow!("no matching tab"))?;
    let value = cdp_eval(&tab.websocket_url, script).await?;
    if should_emit_preview(&input) {
        let _ = emit_browser_preview_from_tab(client, session_id, &tab, false).await;
    }
    Ok(value)
}

fn should_emit_preview(input: &Value) -> bool {
    input
        .get("preview")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

async fn emit_browser_preview_from_tab(
    client: Arc<Client>,
    session_id: Option<&str>,
    tab: &Tab,
    full_page: bool,
) -> Result<()> {
    let png = cdp_capture_screenshot(&tab.websocket_url, full_page).await?;
    let dims = image::load_from_memory(&png)?.dimensions();
    emit_browser_preview(client, session_id, &tab.url, tab.title.clone(), png, dims).await
}

async fn emit_browser_preview_from_response(
    client: Arc<Client>,
    session_id: Option<&str>,
    tab_response: &Value,
) -> Result<()> {
    let ws_url = tab_response
        .get("webSocketDebuggerUrl")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("browser_open response had no webSocketDebuggerUrl"))?;
    for _ in 0..20 {
        let ready = cdp_eval(ws_url, "document.readyState").await.ok();
        if matches!(
            ready.as_ref().and_then(|v| v.as_str()),
            Some("complete" | "interactive")
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let png = cdp_capture_screenshot(ws_url, false).await?;
    let dims = image::load_from_memory(&png)?.dimensions();
    let url = tab_response
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let title = tab_response
        .get("title")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    emit_browser_preview(client, session_id, url, title, png, dims).await
}

async fn emit_browser_preview(
    client: Arc<Client>,
    session_id: Option<&str>,
    url: &str,
    title: Option<String>,
    png: Vec<u8>,
    dims: (u32, u32),
) -> Result<()> {
    let Some(session_id) = session_id.filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    client
        .emit_event(
            session_id,
            SessionEvent::BrowserPreview(BrowserPreview {
                url: url.to_string(),
                title,
                image: base64::engine::general_purpose::STANDARD.encode(png),
                width: dims.0,
                height: dims.1,
            }),
        )
        .await
}

struct Endpoint {
    host: String,
    port: u16,
}

impl Endpoint {
    fn from_input(input: &Value) -> Self {
        let host = input
            .get("host")
            .and_then(|s| s.as_str())
            .unwrap_or(DEFAULT_HOST)
            .to_string();
        let port = input
            .get("port")
            .and_then(|n| n.as_u64())
            .unwrap_or(DEFAULT_PORT as u64)
            .clamp(1, 65535) as u16;
        Self { host, port }
    }

    fn base(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

struct Tab {
    websocket_url: String,
    url: String,
    title: Option<String>,
}

async fn devtools_version(endpoint: &Endpoint) -> Result<Value> {
    Ok(reqwest::get(format!("{}/json/version", endpoint.base()))
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn wait_for_devtools(endpoint: &Endpoint) -> Result<()> {
    for _ in 0..40 {
        if devtools_version(endpoint).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(anyhow!(
        "Chrome DevTools did not respond on {}",
        endpoint.base()
    ))
}

async fn start_chrome(endpoint: &Endpoint) -> Result<()> {
    if endpoint.host != "127.0.0.1" && endpoint.host != "localhost" {
        return Err(anyhow!(
            "refusing to auto-start Chrome for non-local host {}",
            endpoint.host
        ));
    }
    let chrome = chrome_path().ok_or_else(|| anyhow!("Chrome/Chromium binary not found"))?;
    let profile = format!("/tmp/agentd-chrome-debug-{}", endpoint.port);
    let mut cmd = Command::new(chrome);
    cmd.arg(format!("--remote-debugging-port={}", endpoint.port))
        .arg(format!("--user-data-dir={profile}"))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false);
    let _child = cmd.spawn().context("spawn Chrome with remote debugging")?;
    Ok(())
}

fn chrome_path() -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    {
        let candidates = [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ];
        for p in candidates {
            if std::path::Path::new(p).exists() {
                return Some(p);
            }
        }
    }
    None
}

async fn list_tabs(endpoint: &Endpoint) -> Result<Vec<Value>> {
    Ok(reqwest::get(format!("{}/json/list", endpoint.base()))
        .await?
        .error_for_status()?
        .json()
        .await?)
}

fn select_tab(tabs: &[Value], input: &Value) -> Option<Tab> {
    let tab_id = input.get("tab_id").and_then(|s| s.as_str());
    let url_contains = input.get("url_contains").and_then(|s| s.as_str());
    let tab = tabs.iter().find(|t| {
        let is_page = t.get("type").and_then(|v| v.as_str()) == Some("page");
        let id_match = tab_id
            .map(|id| t.get("id").and_then(|v| v.as_str()) == Some(id))
            .unwrap_or(false);
        let url_match = url_contains
            .map(|needle| {
                t.get("url")
                    .and_then(|v| v.as_str())
                    .map(|url| url.contains(needle))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        is_page && (id_match || url_match || (tab_id.is_none() && url_contains.is_none()))
    })?;
    Some(Tab {
        websocket_url: tab.get("webSocketDebuggerUrl")?.as_str()?.to_string(),
        url: tab
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        title: tab
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

async fn cdp_eval(ws_url: &str, expression: &str) -> Result<Value> {
    let value = cdp_call(
        ws_url,
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "awaitPromise": true,
            "returnByValue": true
        }),
    )
    .await?;
    if let Some(exception) = value.pointer("/result/exceptionDetails") {
        return Err(anyhow!("JavaScript exception: {exception}"));
    }
    Ok(value
        .pointer("/result/result/value")
        .cloned()
        .unwrap_or_else(|| {
            value
                .pointer("/result/result")
                .cloned()
                .unwrap_or(Value::Null)
        }))
}

async fn cdp_capture_screenshot(ws_url: &str, full_page: bool) -> Result<Vec<u8>> {
    let value = cdp_call(
        ws_url,
        "Page.captureScreenshot",
        json!({
            "format": "png",
            "captureBeyondViewport": full_page,
            "fromSurface": true
        }),
    )
    .await?;
    let data = value
        .pointer("/result/data")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Page.captureScreenshot returned no data"))?;
    Ok(base64::engine::general_purpose::STANDARD.decode(data.as_bytes())?)
}

async fn cdp_call(ws_url: &str, method: &str, params: Value) -> Result<Value> {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let (mut ws, _) = connect_async(ws_url).await?;
    if method.starts_with("Runtime.") {
        let enable_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({ "id": enable_id, "method": "Runtime.enable" }).to_string(),
        ))
        .await?;
    } else if method.starts_with("Page.") {
        let enable_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({ "id": enable_id, "method": "Page.enable" }).to_string(),
        ))
        .await?;
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({ "id": id, "method": method, "params": params }).to_string(),
    ))
    .await?;
    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let text = match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            _ => continue,
        };
        let value: Value = serde_json::from_str(&text)?;
        if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
            if let Some(err) = value.get("error") {
                return Err(anyhow!("CDP {method} error: {err}"));
            }
            return Ok(value);
        }
    }
    Err(anyhow!(
        "DevTools websocket closed before {method} response"
    ))
}

fn urlencoding(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencoding_escapes_query_chars() {
        assert_eq!(
            urlencoding("https://x.test/search?q=a b"),
            "https%3A%2F%2Fx.test%2Fsearch%3Fq%3Da%20b"
        );
    }

    #[test]
    fn select_tab_matches_url_substring() {
        let tabs = vec![json!({
            "id": "t1",
            "type": "page",
            "url": "https://example.com/search",
            "webSocketDebuggerUrl": "ws://127.0.0.1/devtools/page/t1"
        })];
        let tab = select_tab(&tabs, &json!({ "url_contains": "example.com" })).unwrap();
        assert_eq!(tab.websocket_url, "ws://127.0.0.1/devtools/page/t1");
        assert_eq!(tab.url, "https://example.com/search");
    }

    #[test]
    fn catalog_includes_preview_defaults() {
        for tool in catalog() {
            let preview = tool
                .pointer("/inputSchema/properties/preview/default")
                .and_then(|value| value.as_bool());
            assert_eq!(
                preview,
                Some(true),
                "{} missing preview default",
                tool["name"]
            );
        }
    }
}
