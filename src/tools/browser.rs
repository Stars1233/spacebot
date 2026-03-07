//! Browser automation tools for workers.
//!
//! Provides a suite of separate tools (navigate, click, type, snapshot, etc.)
//! backed by a shared browser state. Uses an ARIA-tree DOM extraction script
//! (ported from browser-use-rs) that assigns numeric indices to interactive
//! elements. The LLM sees a compact YAML snapshot and interacts via index.
//!
//! Element resolution: index → CSS selector (built from DOM position during
//! extraction) → chromiumoxide `page.find_element()`. This avoids stale CDP
//! node IDs and works with both native HTML elements and ARIA widgets.

use crate::config::BrowserConfig;

use chromiumoxide::browser::{Browser, BrowserConfig as ChromeConfig};
use chromiumoxide::fetcher::{BrowserFetcher, BrowserFetcherOptions};
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide_cdp::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType,
};
use chromiumoxide_cdp::cdp::browser_protocol::page::CaptureScreenshotFormat;
use futures::StreamExt as _;
use reqwest::Url;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// URL validation (SSRF protection)
// ---------------------------------------------------------------------------

/// Validate that a URL is safe for the browser to navigate to.
/// Blocks private/loopback IPs, link-local addresses, and cloud metadata endpoints
/// to prevent server-side request forgery.
fn validate_url(url: &str) -> Result<(), BrowserError> {
    let parsed = Url::parse(url)
        .map_err(|error| BrowserError::new(format!("invalid URL '{url}': {error}")))?;

    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(BrowserError::new(format!(
                "scheme '{other}' is not allowed — only http and https are permitted"
            )));
        }
    }

    let Some(host) = parsed.host_str() else {
        return Err(BrowserError::new("URL has no host"));
    };

    if host == "metadata.google.internal"
        || host == "169.254.169.254"
        || host == "metadata.aws.internal"
    {
        return Err(BrowserError::new(
            "access to cloud metadata endpoints is blocked",
        ));
    }

    if let Ok(ip) = host.parse::<IpAddr>()
        && is_blocked_ip(ip)
    {
        return Err(BrowserError::new(format!(
            "navigation to private/loopback address {ip} is blocked"
        )));
    }

    if let Some(stripped) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'))
        && let Ok(ip) = stripped.parse::<IpAddr>()
        && is_blocked_ip(ip)
    {
        return Err(BrowserError::new(format!(
            "navigation to private/loopback address {ip} is blocked"
        )));
    }

    Ok(())
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || is_v4_cgnat(v4)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || is_v6_unique_local(v6)
                || is_v6_link_local(v6)
                || is_v4_mapped_blocked(v6)
        }
    }
}

fn is_v4_cgnat(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0xC0) == 64
}

fn is_v6_unique_local(ip: std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xFE00) == 0xFC00
}

fn is_v6_link_local(ip: std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xFFC0) == 0xFE80
}

fn is_v4_mapped_blocked(ip: std::net::Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        is_blocked_ip(IpAddr::V4(v4))
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// DOM snapshot types (ported from browser-use-rs)
// ---------------------------------------------------------------------------

/// An ARIA tree snapshot of a page, extracted via injected JavaScript.
///
/// Contains the tree structure for LLM display and a parallel `selectors`
/// array that maps element indices to CSS selectors for interaction.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DomSnapshot {
    pub root: AriaNode,
    /// CSS selector for each indexed element. `selectors[i]` corresponds to the
    /// element with `index == i` in the ARIA tree.
    pub selectors: Vec<String>,
    #[allow(dead_code)]
    pub iframe_indices: Vec<usize>,
    /// Extraction error from the JS side, if any.
    pub error: Option<String>,
}

/// A node in the ARIA accessibility tree.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AriaNode {
    pub role: String,
    pub name: String,
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub children: Vec<AriaChild>,
    #[serde(default)]
    pub props: HashMap<String, String>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub checked: Option<serde_json::Value>,
    #[serde(default)]
    pub disabled: Option<bool>,
    #[serde(default)]
    pub expanded: Option<bool>,
    #[serde(default)]
    pub level: Option<u32>,
    #[serde(default)]
    pub pressed: Option<serde_json::Value>,
    #[serde(default)]
    pub selected: Option<bool>,
    #[serde(default)]
    pub box_info: Option<BoxInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BoxInfo {
    #[allow(dead_code)]
    pub visible: bool,
    pub cursor: Option<String>,
}

/// A child of an `AriaNode` — either a text string or a nested node.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum AriaChild {
    Text(String),
    Node(Box<AriaNode>),
}

/// The JS script injected into pages to extract the ARIA tree.
/// Ported from browser-use-rs's `extract_dom.js`.
const EXTRACT_DOM_JS: &str = include_str!("browser_extract_dom.js");

impl DomSnapshot {
    /// Render the ARIA tree as compact YAML-like text for LLM consumption.
    pub fn render(&self) -> String {
        let mut output = String::with_capacity(4096);
        render_node(&self.root, 0, &mut output);
        output
    }

    /// Look up the CSS selector for an element index.
    pub fn selector_for_index(&self, index: usize) -> Option<&str> {
        self.selectors.get(index).map(|s| s.as_str())
    }
}

fn render_node(node: &AriaNode, depth: usize, output: &mut String) {
    let indent = "  ".repeat(depth);

    // Skip the root fragment — just render children
    if node.role == "fragment" {
        for child in &node.children {
            render_child(child, depth, output);
        }
        return;
    }

    // Skip generic nodes without index or name — they're structural noise
    if node.role == "generic" && node.index.is_none() && node.name.is_empty() {
        for child in &node.children {
            render_child(child, depth, output);
        }
        return;
    }

    // Build the line: `- role "name" [attrs]:`
    output.push_str(&indent);
    output.push_str("- ");
    output.push_str(&node.role);

    if !node.name.is_empty() {
        output.push_str(" \"");
        // Escape quotes in name for YAML safety
        output.push_str(&node.name.replace('"', "\\\""));
        output.push('"');
    }

    // Attributes
    if let Some(index) = node.index {
        output.push_str(&format!(" [index={index}]"));
    }
    if let Some(level) = node.level {
        output.push_str(&format!(" [level={level}]"));
    }
    if let Some(true) = node.active {
        output.push_str(" [active]");
    }
    if let Some(true) = node.disabled {
        output.push_str(" [disabled]");
    }
    if let Some(true) = node.selected {
        output.push_str(" [selected]");
    }
    if let Some(ref checked) = node.checked {
        match checked {
            serde_json::Value::Bool(true) => output.push_str(" [checked]"),
            serde_json::Value::Bool(false) => output.push_str(" [unchecked]"),
            serde_json::Value::String(s) if s == "mixed" => output.push_str(" [checked=mixed]"),
            _ => {}
        }
    }
    if let Some(ref pressed) = node.pressed {
        match pressed {
            serde_json::Value::Bool(true) => output.push_str(" [pressed]"),
            serde_json::Value::Bool(false) => {}
            serde_json::Value::String(s) if s == "mixed" => output.push_str(" [pressed=mixed]"),
            _ => {}
        }
    }
    if let Some(true) = node.expanded {
        output.push_str(" [expanded]");
    } else if let Some(false) = node.expanded {
        output.push_str(" [collapsed]");
    }
    if let Some(ref box_info) = node.box_info
        && box_info.cursor.as_deref() == Some("pointer")
    {
        output.push_str(" [cursor=pointer]");
    }

    // Props on separate indented lines
    let has_children = !node.children.is_empty();
    let has_props = !node.props.is_empty();

    if has_children || has_props {
        output.push_str(":\n");
    } else {
        output.push('\n');
    }

    for (key, value) in &node.props {
        output.push_str(&indent);
        output.push_str("  /");
        output.push_str(key);
        output.push_str(": ");
        output.push_str(value);
        output.push('\n');
    }

    for child in &node.children {
        render_child(child, depth + 1, output);
    }
}

fn render_child(child: &AriaChild, depth: usize, output: &mut String) {
    match child {
        AriaChild::Text(text) => {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                let indent = "  ".repeat(depth);
                // Truncate long text for LLM context efficiency
                let display = if trimmed.len() > 200 {
                    format!("{}...", &trimmed[..200])
                } else {
                    trimmed.to_string()
                };
                output.push_str(&indent);
                output.push_str("- text \"");
                output.push_str(&display.replace('"', "\\\""));
                output.push_str("\"\n");
            }
        }
        AriaChild::Node(node) => render_node(node, depth, output),
    }
}

// ---------------------------------------------------------------------------
// Shared browser state
// ---------------------------------------------------------------------------

/// Opaque handle to shared browser state that persists across worker lifetimes.
///
/// Held by `RuntimeConfig` when `persist_session = true`. All workers for the
/// same agent clone this handle and share a single browser process / tab set.
pub type SharedBrowserHandle = Arc<Mutex<BrowserState>>;

/// Create a new shared browser handle for use in `RuntimeConfig`.
pub fn new_shared_browser_handle() -> SharedBrowserHandle {
    Arc::new(Mutex::new(BrowserState::new()))
}

/// Internal browser state managed across tool invocations.
///
/// When `persist_session` is enabled this struct lives in `RuntimeConfig` (via
/// `SharedBrowserHandle`) and is shared across worker lifetimes. Otherwise each
/// tool set owns its own instance.
pub struct BrowserState {
    browser: Option<Browser>,
    _handler_task: Option<JoinHandle<()>>,
    pages: HashMap<String, chromiumoxide::Page>,
    active_target: Option<String>,
    /// Cached DOM snapshot from the last `browser_snapshot` call. Invalidated
    /// on navigation, tab switch, and explicit snapshot refresh.
    snapshot: Option<DomSnapshot>,
    user_data_dir: Option<PathBuf>,
    /// When true, `user_data_dir` is a stable path that should NOT be deleted
    /// on drop — it holds cookies, localStorage, and login sessions.
    persistent_profile: bool,
}

impl BrowserState {
    fn new() -> Self {
        Self {
            browser: None,
            _handler_task: None,
            pages: HashMap::new(),
            active_target: None,
            snapshot: None,
            user_data_dir: None,
            persistent_profile: false,
        }
    }

    /// Invalidate the cached snapshot. Called after any page-mutating action.
    fn invalidate_snapshot(&mut self) {
        self.snapshot = None;
    }
}

impl Drop for BrowserState {
    fn drop(&mut self) {
        // Persistent profiles store cookies and login sessions that must
        // survive across agent restarts — never delete them.
        if self.persistent_profile {
            return;
        }

        if let Some(dir) = self.user_data_dir.take() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn_blocking(move || {
                    if let Err(error) = std::fs::remove_dir_all(&dir) {
                        tracing::debug!(
                            path = %dir.display(),
                            %error,
                            "failed to clean up browser user data dir"
                        );
                    }
                });
            } else if let Err(error) = std::fs::remove_dir_all(&dir) {
                eprintln!(
                    "failed to clean up browser user data dir {}: {error}",
                    dir.display()
                );
            }
        }
    }
}

impl std::fmt::Debug for BrowserState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserState")
            .field("has_browser", &self.browser.is_some())
            .field("pages", &self.pages.len())
            .field("active_target", &self.active_target)
            .field("has_snapshot", &self.snapshot.is_some())
            .field("persistent_profile", &self.persistent_profile)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
#[error("Browser error: {message}")]
pub struct BrowserError {
    pub message: String,
}

impl BrowserError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Common output type
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct BrowserOutput {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tabs: Option<Vec<TabInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

impl BrowserOutput {
    fn success(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
            title: None,
            url: None,
            snapshot: None,
            tabs: None,
            screenshot_path: None,
            eval_result: None,
            content: None,
        }
    }

    fn with_page_info(mut self, title: Option<String>, url: Option<String>) -> Self {
        self.title = title;
        self.url = url;
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TabInfo {
    pub target_id: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub active: bool,
}

// ---------------------------------------------------------------------------
// Shared helper struct that all tools reference
// ---------------------------------------------------------------------------

/// Shared context cloned into each browser tool. Holds the browser state mutex,
/// config, and screenshot directory.
#[derive(Debug, Clone)]
pub(crate) struct BrowserContext {
    state: Arc<Mutex<BrowserState>>,
    config: BrowserConfig,
    screenshot_dir: PathBuf,
}

impl BrowserContext {
    fn new(
        state: Arc<Mutex<BrowserState>>,
        config: BrowserConfig,
        screenshot_dir: PathBuf,
    ) -> Self {
        Self {
            state,
            config,
            screenshot_dir,
        }
    }

    /// Get the active page or return an error. Does NOT hold the lock — caller
    /// must pass a reference to the already-locked state.
    fn require_active_page<'a>(
        &self,
        state: &'a BrowserState,
    ) -> Result<&'a chromiumoxide::Page, BrowserError> {
        let target = state
            .active_target
            .as_ref()
            .ok_or_else(|| BrowserError::new("no active tab — navigate or open a tab first"))?;
        state
            .pages
            .get(target)
            .ok_or_else(|| BrowserError::new("active tab no longer exists"))
    }

    /// Extract the DOM snapshot from the active page via injected JavaScript.
    /// Caches the result on `BrowserState` so repeated reads don't re-extract.
    async fn extract_snapshot<'a>(
        &self,
        state: &'a mut BrowserState,
    ) -> Result<&'a DomSnapshot, BrowserError> {
        if let Some(ref snapshot) = state.snapshot {
            return Ok(snapshot);
        }

        let page = self.require_active_page(state)?;

        let result = page
            .evaluate(EXTRACT_DOM_JS)
            .await
            .map_err(|error| BrowserError::new(format!("DOM extraction failed: {error}")))?;

        let value = result.value().cloned().unwrap_or(serde_json::Value::Null);

        // The JS wraps the result in JSON.stringify(), so we get a string back
        let json_value = if let Some(json_str) = value.as_str() {
            serde_json::from_str::<serde_json::Value>(json_str).unwrap_or(serde_json::Value::Null)
        } else {
            value
        };

        let snapshot: DomSnapshot = serde_json::from_value(json_value)
            .map_err(|error| BrowserError::new(format!("failed to parse DOM snapshot: {error}")))?;

        if let Some(ref error) = snapshot.error {
            tracing::warn!(%error, "DOM extraction JS reported an error");
        }

        state.snapshot = Some(snapshot);
        Ok(state.snapshot.as_ref().expect("just stored"))
    }

    /// Resolve a numeric element index to a chromiumoxide Element on the active page.
    /// Uses the cached snapshot's CSS selectors.
    async fn find_element_by_index(
        &self,
        state: &mut BrowserState,
        index: usize,
    ) -> Result<chromiumoxide::Element, BrowserError> {
        // Ensure snapshot is cached
        self.extract_snapshot(state).await?;
        let snapshot = state.snapshot.as_ref().expect("just extracted");

        let selector = snapshot.selector_for_index(index).ok_or_else(|| {
            BrowserError::new(format!(
                "element index {index} not found — run browser_snapshot to get fresh indices \
                 (max index in current snapshot: {})",
                snapshot.selectors.len().saturating_sub(1)
            ))
        })?;

        if selector.is_empty() {
            return Err(BrowserError::new(format!(
                "element index {index} has an empty CSS selector — the element may be in an iframe. \
                 Try browser_evaluate with a custom querySelector instead."
            )));
        }

        let page = self.require_active_page(state)?;

        page.find_element(selector).await.map_err(|error| {
            BrowserError::new(format!(
                "element at index {index} not found via selector '{selector}': {error}. \
                 The page may have changed — run browser_snapshot again."
            ))
        })
    }

    /// Launch the browser if not already running. Returns a status message.
    async fn ensure_launched(&self) -> Result<String, BrowserError> {
        {
            let mut state = self.state.lock().await;
            if state.browser.is_some() {
                if self.config.persist_session {
                    return self.reconnect_existing_tabs(&mut state).await;
                }
                return Ok("Browser already running".to_string());
            }
        }

        let executable = resolve_chrome_executable(&self.config).await?;

        let (user_data_dir, persistent_profile) = if self.config.persist_session {
            (self.config.chrome_cache_dir.join("profile"), true)
        } else {
            let dir =
                std::env::temp_dir().join(format!("spacebot-browser-{}", uuid::Uuid::new_v4()));
            (dir, false)
        };

        if persistent_profile {
            let lock_file = user_data_dir.join("SingletonLock");
            if lock_file.exists() {
                tracing::debug!(path = %lock_file.display(), "removing stale Chrome SingletonLock");
                let _ = std::fs::remove_file(&lock_file);
            }
        }

        let mut builder = ChromeConfig::builder()
            .no_sandbox()
            .chrome_executable(&executable)
            .user_data_dir(&user_data_dir);

        if !self.config.headless {
            builder = builder.with_head().window_size(1280, 900);
        }

        let chrome_config = builder.build().map_err(|error| {
            BrowserError::new(format!("failed to build browser config: {error}"))
        })?;

        tracing::info!(
            headless = self.config.headless,
            executable = %executable.display(),
            user_data_dir = %user_data_dir.display(),
            "launching chrome"
        );

        let (browser, mut handler) = Browser::launch(chrome_config)
            .await
            .map_err(|error| BrowserError::new(format!("failed to launch browser: {error}")))?;

        let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

        let mut state = self.state.lock().await;

        // Guard against concurrent launch race
        if state.browser.is_some() {
            drop(browser);
            handler_task.abort();
            if !persistent_profile {
                let dir = user_data_dir;
                tokio::spawn(async move {
                    if let Err(error) = tokio::fs::remove_dir_all(&dir).await {
                        tracing::debug!(
                            path = %dir.display(),
                            %error,
                            "failed to clean up browser user data dir (concurrent launch race)"
                        );
                    }
                });
            }
            if self.config.persist_session {
                return self.reconnect_existing_tabs(&mut state).await;
            }
            return Ok("Browser already running".to_string());
        }

        state.browser = Some(browser);
        state._handler_task = Some(handler_task);
        state.user_data_dir = Some(user_data_dir);
        state.persistent_profile = persistent_profile;

        tracing::info!("browser launched");
        Ok("Browser launched successfully".to_string())
    }

    /// Discover existing tabs from the browser and rebuild the page map.
    async fn reconnect_existing_tabs(
        &self,
        state: &mut BrowserState,
    ) -> Result<String, BrowserError> {
        let browser = state
            .browser
            .as_ref()
            .ok_or_else(|| BrowserError::new("browser not launched"))?;

        let pages = browser.pages().await.map_err(|error| {
            BrowserError::new(format!("failed to enumerate existing tabs: {error}"))
        })?;

        let previous_ids: std::collections::HashSet<String> = state.pages.keys().cloned().collect();
        let mut refreshed_pages = HashMap::with_capacity(pages.len());
        for page in pages {
            let target_id = page_target_id(&page);
            refreshed_pages.insert(target_id, page);
        }
        let discovered = refreshed_pages
            .keys()
            .filter(|id| !previous_ids.contains(*id))
            .count();

        state.pages = refreshed_pages;
        state.invalidate_snapshot();

        let active_valid = state
            .active_target
            .as_ref()
            .is_some_and(|id| state.pages.contains_key(id));
        if !active_valid {
            state.active_target = state.pages.keys().next().cloned();
        }

        let tab_count = state.pages.len();
        tracing::info!(tab_count, discovered, "reconnected to persistent browser");

        Ok(format!(
            "Connected to persistent browser ({tab_count} tab{} open, {discovered} newly discovered)",
            if tab_count == 1 { "" } else { "s" }
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_launch
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BrowserLaunchTool {
    context: BrowserContext,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserLaunchArgs {}

impl Tool for BrowserLaunchTool {
    const NAME: &'static str = "browser_launch";
    type Error = BrowserError;
    type Args = BrowserLaunchArgs;
    type Output = BrowserOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Launch the browser. Must be called before any other browser tool."
                .to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        let message = self.context.ensure_launched().await?;
        Ok(BrowserOutput::success(message))
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_navigate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BrowserNavigateTool {
    context: BrowserContext,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserNavigateArgs {
    /// The URL to navigate to.
    pub url: String,
}

impl Tool for BrowserNavigateTool {
    const NAME: &'static str = "browser_navigate";
    type Error = BrowserError;
    type Args = BrowserNavigateArgs;
    type Output = BrowserOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Navigate the active tab to a URL. Auto-launches the browser if needed."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to navigate to" }
                },
                "required": ["url"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        validate_url(&args.url)?;
        self.context.ensure_launched().await?;

        let mut state = self.context.state.lock().await;

        // Get or create the active page
        let page = get_or_create_page(&self.context, &mut state, Some(&args.url)).await?;

        page.goto(&args.url)
            .await
            .map_err(|error| BrowserError::new(format!("navigation failed: {error}")))?;

        let title = page.get_title().await.ok().flatten();
        let current_url = page.url().await.ok().flatten();
        state.invalidate_snapshot();

        Ok(BrowserOutput::success(format!("Navigated to {}", args.url))
            .with_page_info(title, current_url))
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BrowserSnapshotTool {
    context: BrowserContext,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserSnapshotArgs {}

impl Tool for BrowserSnapshotTool {
    const NAME: &'static str = "browser_snapshot";
    type Error = BrowserError;
    type Args = BrowserSnapshotArgs;
    type Output = BrowserOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Get the page's ARIA accessibility tree with numbered element indices. \
                          Use the [index=N] values in browser_click, browser_type, etc."
                .to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut state = self.context.state.lock().await;

        // Force a fresh snapshot
        state.invalidate_snapshot();
        let snapshot = self.context.extract_snapshot(&mut state).await?;

        let rendered = snapshot.render();
        let element_count = snapshot.selectors.len();
        let title = self
            .context
            .require_active_page(&state)?
            .get_title()
            .await
            .ok()
            .flatten();
        let url = self
            .context
            .require_active_page(&state)?
            .url()
            .await
            .ok()
            .flatten();

        Ok(BrowserOutput {
            success: true,
            message: format!("{element_count} interactive element(s) found"),
            title,
            url,
            snapshot: Some(rendered),
            tabs: None,
            screenshot_path: None,
            eval_result: None,
            content: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_click
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BrowserClickTool {
    context: BrowserContext,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserClickArgs {
    /// The element index from the snapshot (e.g., 5).
    pub index: usize,
}

impl Tool for BrowserClickTool {
    const NAME: &'static str = "browser_click";
    type Error = BrowserError;
    type Args = BrowserClickArgs;
    type Output = BrowserOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Click an element by its index from browser_snapshot.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "index": { "type": "integer", "description": "Element index from snapshot" }
                },
                "required": ["index"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut state = self.context.state.lock().await;
        let element = self
            .context
            .find_element_by_index(&mut state, args.index)
            .await?;

        element
            .click()
            .await
            .map_err(|error| BrowserError::new(format!("click failed: {error}")))?;

        state.invalidate_snapshot();

        Ok(BrowserOutput::success(format!(
            "Clicked element at index {}",
            args.index
        )))
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BrowserTypeTool {
    context: BrowserContext,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserTypeArgs {
    /// The element index from the snapshot.
    pub index: usize,
    /// The text to type into the element.
    pub text: String,
    /// Whether to clear the field before typing. Defaults to true.
    #[serde(default = "default_true")]
    pub clear: bool,
}

fn default_true() -> bool {
    true
}

impl Tool for BrowserTypeTool {
    const NAME: &'static str = "browser_type";
    type Error = BrowserError;
    type Args = BrowserTypeArgs;
    type Output = BrowserOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Type text into an input element by its index from browser_snapshot."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "index": { "type": "integer", "description": "Element index from snapshot" },
                    "text": { "type": "string", "description": "Text to type" },
                    "clear": { "type": "boolean", "default": true, "description": "Clear the field before typing (default true)" }
                },
                "required": ["index", "text"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut state = self.context.state.lock().await;

        // Use JS injection for reliable clear + type: focus, select all, then
        // set value and fire events. chromiumoxide's type_str sends individual
        // key events which is slow and fragile with JS frameworks.
        let snapshot = self.context.extract_snapshot(&mut state).await?;
        let selector = snapshot
            .selector_for_index(args.index)
            .ok_or_else(|| BrowserError::new(format!("element index {} not found", args.index)))?
            .to_string();

        let page = self.context.require_active_page(&state)?;

        let text_json = serde_json::to_string(&args.text).unwrap_or_default();
        let clear_js = if args.clear { "el.value = '';" } else { "" };
        let js = format!(
            r#"(() => {{
                const el = document.querySelector({selector_json});
                if (!el) return JSON.stringify({{success: false, error: 'element not found'}});
                el.focus();
                {clear_js}
                el.value = {text_json};
                el.dispatchEvent(new Event('input', {{bubbles: true}}));
                el.dispatchEvent(new Event('change', {{bubbles: true}}));
                return JSON.stringify({{success: true}});
            }})()"#,
            selector_json = serde_json::to_string(&selector).unwrap_or_default(),
            clear_js = clear_js,
            text_json = text_json,
        );

        let result = page
            .evaluate(js)
            .await
            .map_err(|error| BrowserError::new(format!("type failed: {error}")))?;

        // Check for JS-level errors
        if let Some(value) = result.value()
            && let Some(json_str) = value.as_str()
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str)
            && parsed.get("success").and_then(|v| v.as_bool()) == Some(false)
        {
            let error = parsed
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("type action failed");
            return Err(BrowserError::new(error.to_string()));
        }

        state.invalidate_snapshot();

        let display_text = if args.text.len() > 50 {
            format!("{}...", &args.text[..50])
        } else {
            args.text.clone()
        };
        Ok(BrowserOutput::success(format!(
            "Typed '{display_text}' into element at index {}",
            args.index
        )))
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_press_key
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BrowserPressKeyTool {
    context: BrowserContext,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserPressKeyArgs {
    /// The key to press (e.g., "Enter", "Tab", "Escape", "ArrowDown").
    pub key: String,
}

impl Tool for BrowserPressKeyTool {
    const NAME: &'static str = "browser_press_key";
    type Error = BrowserError;
    type Args = BrowserPressKeyArgs;
    type Output = BrowserOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Press a keyboard key (e.g., \"Enter\", \"Tab\", \"Escape\").".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Key name (Enter, Tab, Escape, ArrowDown, etc.)" }
                },
                "required": ["key"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let state = self.context.state.lock().await;
        let page = self.context.require_active_page(&state)?;
        dispatch_key_press(page, &args.key).await?;
        Ok(BrowserOutput::success(format!(
            "Pressed key '{}'",
            args.key
        )))
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_screenshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BrowserScreenshotTool {
    context: BrowserContext,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserScreenshotArgs {
    /// Whether to take a full-page screenshot.
    #[serde(default)]
    pub full_page: bool,
}

impl Tool for BrowserScreenshotTool {
    const NAME: &'static str = "browser_screenshot";
    type Error = BrowserError;
    type Args = BrowserScreenshotArgs;
    type Output = BrowserOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description:
                "Take a screenshot of the current page. Saves to disk and returns the file path."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "full_page": { "type": "boolean", "default": false, "description": "Capture entire page, not just viewport" }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let state = self.context.state.lock().await;
        let page = self.context.require_active_page(&state)?;

        let params = ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .full_page(args.full_page)
            .build();

        let screenshot_data = page
            .screenshot(params)
            .await
            .map_err(|error| BrowserError::new(format!("screenshot failed: {error}")))?;

        let filename = format!(
            "screenshot_{}.png",
            chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f")
        );
        let filepath = self.context.screenshot_dir.join(&filename);

        tokio::fs::create_dir_all(&self.context.screenshot_dir)
            .await
            .map_err(|error| {
                BrowserError::new(format!("failed to create screenshot dir: {error}"))
            })?;

        tokio::fs::write(&filepath, &screenshot_data)
            .await
            .map_err(|error| BrowserError::new(format!("failed to save screenshot: {error}")))?;

        let path_str = filepath.to_string_lossy().to_string();
        let size_kb = screenshot_data.len() / 1024;
        tracing::debug!(path = %path_str, size_kb, "screenshot saved");

        Ok(BrowserOutput {
            success: true,
            message: format!("Screenshot saved ({size_kb}KB)"),
            title: None,
            url: None,
            snapshot: None,
            tabs: None,
            screenshot_path: Some(path_str),
            eval_result: None,
            content: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_evaluate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BrowserEvaluateTool {
    context: BrowserContext,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserEvaluateArgs {
    /// JavaScript expression to evaluate in the page.
    pub script: String,
}

impl Tool for BrowserEvaluateTool {
    const NAME: &'static str = "browser_evaluate";
    type Error = BrowserError;
    type Args = BrowserEvaluateArgs;
    type Output = BrowserOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Evaluate JavaScript in the active page and return the result."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "script": { "type": "string", "description": "JavaScript expression to evaluate" }
                },
                "required": ["script"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        if !self.context.config.evaluate_enabled {
            return Err(BrowserError::new(
                "JavaScript evaluation is disabled in browser config (set evaluate_enabled = true)",
            ));
        }

        let state = self.context.state.lock().await;
        let page = self.context.require_active_page(&state)?;

        let result = page
            .evaluate(args.script)
            .await
            .map_err(|error| BrowserError::new(format!("evaluate failed: {error}")))?;

        let value = result.value().cloned();

        Ok(BrowserOutput {
            success: true,
            message: "JavaScript evaluated".to_string(),
            title: None,
            url: None,
            snapshot: None,
            tabs: None,
            screenshot_path: None,
            eval_result: value,
            content: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_tab_open
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BrowserTabOpenTool {
    context: BrowserContext,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserTabOpenArgs {
    /// URL to open in the new tab. Defaults to about:blank.
    #[serde(default)]
    pub url: Option<String>,
}

impl Tool for BrowserTabOpenTool {
    const NAME: &'static str = "browser_tab_open";
    type Error = BrowserError;
    type Args = BrowserTabOpenArgs;
    type Output = BrowserOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Open a new browser tab, optionally at a URL.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to open (default: about:blank)" }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let target_url = args.url.as_deref().unwrap_or("about:blank");
        if target_url != "about:blank" {
            validate_url(target_url)?;
        }

        let mut state = self.context.state.lock().await;
        let browser = state
            .browser
            .as_ref()
            .ok_or_else(|| BrowserError::new("browser not launched — call browser_launch first"))?;

        let page = browser
            .new_page(target_url)
            .await
            .map_err(|error| BrowserError::new(format!("failed to open tab: {error}")))?;

        let target_id = page_target_id(&page);
        let title = page.get_title().await.ok().flatten();
        let current_url = page.url().await.ok().flatten();

        state.pages.insert(target_id.clone(), page);
        state.active_target = Some(target_id.clone());
        state.invalidate_snapshot();

        Ok(BrowserOutput {
            success: true,
            message: format!("Opened new tab (target: {target_id})"),
            title,
            url: current_url,
            snapshot: None,
            tabs: None,
            screenshot_path: None,
            eval_result: None,
            content: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_tab_list
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BrowserTabListTool {
    context: BrowserContext,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserTabListArgs {}

impl Tool for BrowserTabListTool {
    const NAME: &'static str = "browser_tab_list";
    type Error = BrowserError;
    type Args = BrowserTabListArgs;
    type Output = BrowserOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "List all open browser tabs with their target IDs, titles, and URLs."
                .to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        let state = self.context.state.lock().await;
        let mut tabs = Vec::new();
        for (target_id, page) in &state.pages {
            let title = page.get_title().await.ok().flatten();
            let url = page.url().await.ok().flatten();
            let active = state.active_target.as_ref() == Some(target_id);
            tabs.push(TabInfo {
                target_id: target_id.clone(),
                title,
                url,
                active,
            });
        }

        let count = tabs.len();
        Ok(BrowserOutput {
            success: true,
            message: format!("{count} tab(s) open"),
            title: None,
            url: None,
            snapshot: None,
            tabs: Some(tabs),
            screenshot_path: None,
            eval_result: None,
            content: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_tab_close
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BrowserTabCloseTool {
    context: BrowserContext,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserTabCloseArgs {
    /// Target ID of the tab to close. If omitted, closes the active tab.
    #[serde(default)]
    pub target_id: Option<String>,
}

impl Tool for BrowserTabCloseTool {
    const NAME: &'static str = "browser_tab_close";
    type Error = BrowserError;
    type Args = BrowserTabCloseArgs;
    type Output = BrowserOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Close a browser tab by target_id, or the active tab if omitted."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "target_id": { "type": "string", "description": "Tab target ID (from browser_tab_list). Omit for active tab." }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut state = self.context.state.lock().await;
        let id = args
            .target_id
            .or_else(|| state.active_target.clone())
            .ok_or_else(|| BrowserError::new("no active tab to close"))?;

        let page = state
            .pages
            .remove(&id)
            .ok_or_else(|| BrowserError::new(format!("no tab with target_id '{id}'")))?;

        page.close()
            .await
            .map_err(|error| BrowserError::new(format!("failed to close tab: {error}")))?;

        if state.active_target.as_ref() == Some(&id) {
            state.active_target = state.pages.keys().next().cloned();
        }
        state.invalidate_snapshot();

        Ok(BrowserOutput::success(format!("Closed tab {id}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_close
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BrowserCloseTool {
    context: BrowserContext,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserCloseArgs {}

impl Tool for BrowserCloseTool {
    const NAME: &'static str = "browser_close";
    type Error = BrowserError;
    type Args = BrowserCloseArgs;
    type Output = BrowserOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Close or detach from the browser (behavior depends on config)."
                .to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        use crate::config::ClosePolicy;

        match self.context.config.close_policy {
            ClosePolicy::Detach => {
                let mut state = self.context.state.lock().await;
                state.invalidate_snapshot();
                tracing::info!(policy = "detach", "worker detached from browser");
                Ok(BrowserOutput::success(
                    "Detached from browser (tabs and session preserved)",
                ))
            }
            ClosePolicy::CloseTabs => {
                let pages_to_close: Vec<(String, chromiumoxide::Page)> = {
                    let mut state = self.context.state.lock().await;
                    let pages = state.pages.drain().collect();
                    state.active_target = None;
                    state.invalidate_snapshot();
                    pages
                };

                let mut close_errors = Vec::new();
                for (id, page) in pages_to_close {
                    if let Err(error) = page.close().await {
                        close_errors.push(format!("{id}: {error}"));
                    }
                }

                if !close_errors.is_empty() {
                    let message = format!(
                        "failed to close {} tab(s): {}",
                        close_errors.len(),
                        close_errors.join("; ")
                    );
                    tracing::warn!(policy = "close_tabs", %message);
                    return Err(BrowserError::new(message));
                }

                tracing::info!(
                    policy = "close_tabs",
                    "closed all tabs, browser still running"
                );
                Ok(BrowserOutput::success(
                    "All tabs closed (browser still running)",
                ))
            }
            ClosePolicy::CloseBrowser => {
                let (browser, handler_task, user_data_dir, persistent_profile) = {
                    let mut state = self.context.state.lock().await;
                    let browser = state.browser.take();
                    let handler_task = state._handler_task.take();
                    let user_data_dir = state.user_data_dir.take();
                    let persistent_profile = state.persistent_profile;
                    state.pages.clear();
                    state.active_target = None;
                    state.invalidate_snapshot();
                    (browser, handler_task, user_data_dir, persistent_profile)
                };

                if let Some(task) = handler_task {
                    task.abort();
                }

                if let Some(mut browser) = browser
                    && let Err(error) = browser.close().await
                {
                    let message = format!("failed to close browser: {error}");
                    tracing::warn!(policy = "close_browser", %message);
                    return Err(BrowserError::new(message));
                }

                if !persistent_profile && let Some(dir) = user_data_dir {
                    tokio::spawn(async move {
                        if let Err(error) = tokio::fs::remove_dir_all(&dir).await {
                            tracing::debug!(
                                path = %dir.display(),
                                %error,
                                "failed to clean up browser user data dir"
                            );
                        }
                    });
                }

                tracing::info!(policy = "close_browser", "browser closed");
                Ok(BrowserOutput::success("Browser closed"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tool registration helper
// ---------------------------------------------------------------------------

/// Register all browser tools on a `ToolServer`. The tools share a single
/// `BrowserState` (via `SharedBrowserHandle` for persistent sessions, or a
/// fresh instance for ephemeral sessions).
pub fn register_browser_tools(
    server: rig::tool::server::ToolServer,
    config: BrowserConfig,
    screenshot_dir: PathBuf,
    runtime_config: &crate::config::RuntimeConfig,
) -> rig::tool::server::ToolServer {
    let state = if let Some(shared) = runtime_config
        .shared_browser
        .as_ref()
        .filter(|_| config.persist_session)
    {
        shared.clone()
    } else {
        Arc::new(Mutex::new(BrowserState::new()))
    };

    let context = BrowserContext::new(state, config, screenshot_dir);

    server
        .tool(BrowserLaunchTool {
            context: context.clone(),
        })
        .tool(BrowserNavigateTool {
            context: context.clone(),
        })
        .tool(BrowserSnapshotTool {
            context: context.clone(),
        })
        .tool(BrowserClickTool {
            context: context.clone(),
        })
        .tool(BrowserTypeTool {
            context: context.clone(),
        })
        .tool(BrowserPressKeyTool {
            context: context.clone(),
        })
        .tool(BrowserScreenshotTool {
            context: context.clone(),
        })
        .tool(BrowserEvaluateTool {
            context: context.clone(),
        })
        .tool(BrowserTabOpenTool {
            context: context.clone(),
        })
        .tool(BrowserTabListTool {
            context: context.clone(),
        })
        .tool(BrowserTabCloseTool {
            context: context.clone(),
        })
        .tool(BrowserCloseTool { context })
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Get the active page, or create a first one if the browser has no pages yet.
async fn get_or_create_page<'a>(
    context: &BrowserContext,
    state: &'a mut BrowserState,
    url: Option<&str>,
) -> Result<&'a chromiumoxide::Page, BrowserError> {
    if let Some(target) = state.active_target.as_ref()
        && state.pages.contains_key(target)
    {
        return Ok(&state.pages[target]);
    }

    let browser = state
        .browser
        .as_ref()
        .ok_or_else(|| BrowserError::new("browser not launched — call browser_launch first"))?;

    let target_url = url.unwrap_or("about:blank");
    let page = browser
        .new_page(target_url)
        .await
        .map_err(|error| BrowserError::new(format!("failed to create page: {error}")))?;

    let target_id = page_target_id(&page);
    state.pages.insert(target_id.clone(), page);
    state.active_target = Some(target_id.clone());

    // Suppress the "unused variable" warning — we need `context` for the type
    // signature to match the pattern used by the navigate tool.
    let _ = context;

    Ok(&state.pages[&target_id])
}

/// Dispatch a key press event to the page via CDP Input domain.
async fn dispatch_key_press(page: &chromiumoxide::Page, key: &str) -> Result<(), BrowserError> {
    let key_down = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyDown)
        .key(key)
        .build()
        .map_err(|error| BrowserError::new(format!("failed to build key event: {error}")))?;

    page.execute(key_down)
        .await
        .map_err(|error| BrowserError::new(format!("key down failed: {error}")))?;

    let key_up = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyUp)
        .key(key)
        .build()
        .map_err(|error| BrowserError::new(format!("failed to build key event: {error}")))?;

    page.execute(key_up)
        .await
        .map_err(|error| BrowserError::new(format!("key up failed: {error}")))?;

    Ok(())
}

fn page_target_id(page: &chromiumoxide::Page) -> String {
    page.target_id().inner().clone()
}

// ---------------------------------------------------------------------------
// Chrome executable resolution
// ---------------------------------------------------------------------------

async fn resolve_chrome_executable(config: &BrowserConfig) -> Result<PathBuf, BrowserError> {
    if let Some(path) = &config.executable_path {
        let path = PathBuf::from(path);
        if path.exists() {
            tracing::debug!(path = %path.display(), "using configured chrome executable");
            return Ok(path);
        }
        tracing::warn!(
            path = %path.display(),
            "configured executable_path does not exist, falling through to detection"
        );
    }

    if let Some(path) = detect_chrome_from_env() {
        tracing::debug!(path = %path.display(), "using chrome from environment variable");
        return Ok(path);
    }

    if let Ok(path) = chromiumoxide::detection::default_executable(Default::default()) {
        tracing::debug!(path = %path.display(), "using system-detected chrome");
        return Ok(path);
    }

    tracing::info!(
        cache_dir = %config.chrome_cache_dir.display(),
        "no system Chrome found, downloading via fetcher"
    );
    fetch_chrome(&config.chrome_cache_dir).await
}

fn detect_chrome_from_env() -> Option<PathBuf> {
    for variable in ["CHROME", "CHROME_PATH"] {
        if let Ok(value) = std::env::var(variable) {
            let path = PathBuf::from(&value);
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

async fn fetch_chrome(cache_dir: &Path) -> Result<PathBuf, BrowserError> {
    tokio::fs::create_dir_all(cache_dir)
        .await
        .map_err(|error| {
            BrowserError::new(format!(
                "failed to create chrome cache dir {}: {error}",
                cache_dir.display()
            ))
        })?;

    let options = BrowserFetcherOptions::builder()
        .with_path(cache_dir)
        .build()
        .map_err(|error| {
            BrowserError::new(format!("failed to build browser fetcher options: {error}"))
        })?;

    let fetcher = BrowserFetcher::new(options);
    let info = fetcher
        .fetch()
        .await
        .map_err(|error| BrowserError::new(format!("failed to download chrome: {error}")))?;

    tracing::info!(
        path = %info.executable_path.display(),
        "chrome downloaded and cached"
    );
    Ok(info.executable_path)
}
