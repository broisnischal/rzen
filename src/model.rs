//! Persistent data: settings, session (workspaces + tabs), and JSON file helpers.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub type Id = u64;

pub const SEARCH_ENGINES: &[(&str, &str, &str)] = &[
    ("google", "Google", "https://www.google.com/search?q={}"),
    ("duckduckgo", "DuckDuckGo", "https://duckduckgo.com/?q={}"),
    ("bing", "Bing", "https://www.bing.com/search?q={}"),
    ("brave", "Brave Search", "https://search.brave.com/search?q={}"),
    ("ecosia", "Ecosia", "https://www.ecosia.org/search?q={}"),
    ("startpage", "Startpage", "https://www.startpage.com/do/search?q={}"),
    ("kagi", "Kagi", "https://kagi.com/search?q={}"),
];

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    /// A preset id from `SEARCH_ENGINES`, or a custom URL containing `{}`.
    pub search_engine: String,
    /// "restore": reopen all tabs. "fresh": keep pinned tabs and essentials only.
    pub startup: String,
    /// "system", "light" or "dark".
    pub theme: String,
    /// Native (compiled) tracker/ad blocking. macOS only for now.
    pub block_trackers: bool,
    /// Background tabs idle this long are unloaded (their web process is freed). 0 = never.
    pub unload_after_minutes: u64,
    /// Hard cap on tabs holding a live web process; least-recently-used are unloaded first.
    pub max_live_tabs: usize,
    /// AI side panel provider id (see `AI_PROVIDERS`): "assistant" is rzen's own.
    pub ai_provider: String,
    /// Claude model for the built-in assistant.
    pub ai_model: String,
    /// Local automation API for AI agents (`rzen mcp`). Off unless the user opts in.
    pub agent_enabled: bool,
    /// Route pages through a proxy/VPN endpoint: "socks5://host:port" or "http://host:port". Empty = direct.
    pub proxy: String,
    /// Sidebar mini player for whatever is playing (video docks there when you switch tabs).
    pub mini_player: bool,
    /// Unload least-recently-used background tabs while rzen's total memory is above this. 0 = no limit.
    pub memory_limit_mb: u64,
    /// Which defaults this file was written with (see `Settings::migrate`). Missing = 0.
    #[serde(default)]
    pub defaults_version: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            search_engine: "google".into(),
            startup: "restore".into(),
            theme: "system".into(),
            block_trackers: true,
            unload_after_minutes: 60,
            max_live_tabs: 16,
            ai_provider: "assistant".into(),
            ai_model: "claude-opus-5-5".into(),
            agent_enabled: false,
            proxy: String::new(),
            mini_player: true,
            memory_limit_mb: 0,
            defaults_version: SETTINGS_DEFAULTS,
        }
    }
}

/// Bumped when defaults change in a way existing profiles should pick up.
const SETTINGS_DEFAULTS: u32 = 1;

impl Settings {
    /// Profiles from before v1 carry the old tab-unloading defaults (1 GB limit, 8 live tabs,
    /// unload after 20 min), which reload tabs far too often on any Mac with room to spare.
    /// Values still at those old defaults move to the new ones; anything the user picked stays.
    /// macOS memory pressure still unloads background tabs when memory is really short.
    pub fn migrate(&mut self) {
        if self.defaults_version < 1 {
            if self.memory_limit_mb == 1024 {
                self.memory_limit_mb = 0;
            }
            if self.max_live_tabs == 8 {
                self.max_live_tabs = 16;
            }
            if self.unload_after_minutes == 20 {
                self.unload_after_minutes = 60;
            }
        }
        self.defaults_version = SETTINGS_DEFAULTS;
    }

    pub fn search_template(&self) -> &str {
        SEARCH_ENGINES
            .iter()
            .find(|e| e.0 == self.search_engine)
            .map(|e| e.2)
            .or_else(|| self.search_engine.contains("{}").then_some(self.search_engine.as_str()))
            .unwrap_or(SEARCH_ENGINES[0].2)
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct TabData {
    pub id: Id,
    pub url: String,
    pub title: String,
    pub favicon: Option<String>,
    pub workspace: Id,
    pub pinned: bool,
    /// Essentials are shared by every workspace (Zen's top grid).
    pub essential: bool,
    pub zoom: f64,
}

impl Default for TabData {
    fn default() -> Self {
        Self {
            id: 0,
            url: String::new(),
            title: String::new(),
            favicon: None,
            workspace: 0,
            pinned: false,
            essential: false,
            zoom: 1.0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Workspace {
    pub id: Id,
    pub name: String,
    /// Separate cookie/storage jar (container). Needs macOS 14+.
    pub isolated: bool,
    pub active: Option<Id>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self { id: 0, name: "Home".into(), isolated: false, active: None }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct WindowGeom {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub workspaces: Vec<Workspace>,
    pub tabs: Vec<TabData>,
    pub active_ws: Id,
    pub closed: Vec<TabData>,
    pub next_id: Id,
    pub sidebar_open: bool,
    pub sidebar_w: f64,
    pub window: Option<WindowGeom>,
    pub ai_open: bool,
    pub ai_w: f64,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            workspaces: vec![Workspace { id: 1, ..Default::default() }],
            tabs: Vec::new(),
            active_ws: 1,
            closed: Vec::new(),
            next_id: 2,
            sidebar_open: true,
            sidebar_w: 252.0,
            window: None,
            ai_open: false,
            ai_w: 380.0,
        }
    }
}

impl Session {
    /// Repairs references that may be stale after a crash or a hand-edited file.
    pub fn sanitize(&mut self) {
        if self.workspaces.is_empty() {
            self.workspaces.push(Workspace { id: self.next_id, ..Default::default() });
            self.next_id += 1;
        }
        let ws_ids: Vec<Id> = self.workspaces.iter().map(|w| w.id).collect();
        let first = ws_ids[0];
        if !ws_ids.contains(&self.active_ws) {
            self.active_ws = first;
        }
        for t in &mut self.tabs {
            if !ws_ids.contains(&t.workspace) {
                t.workspace = first;
            }
        }
        let max_id = self
            .tabs
            .iter()
            .map(|t| t.id)
            .chain(ws_ids.iter().copied())
            .chain(self.closed.iter().map(|t| t.id))
            .max()
            .unwrap_or(0);
        self.next_id = self.next_id.max(max_id + 1);
        let tab_ids: Vec<Id> = self.tabs.iter().map(|t| t.id).collect();
        for w in &mut self.workspaces {
            if w.active.is_some_and(|a| !tab_ids.contains(&a)) {
                w.active = None;
            }
        }
        self.sidebar_w = self.sidebar_w.clamp(180.0, 420.0);
        self.ai_w = self.ai_w.clamp(300.0, 720.0);
    }
}

/// Profile directory. `RZEN_PROFILE=/path` runs a fully separate profile (testing, or a
/// second identity) without touching the default one.
pub fn root_dir() -> PathBuf {
    let dir = std::env::var_os("RZEN_PROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("rzen"));
    let _ = fs::create_dir_all(&dir);
    dir
}

pub fn load_json<T: DeserializeOwned + Default>(path: &Path) -> T {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| log!("rzen: ignoring {}: {e}", path.display())).ok())
        .unwrap_or_default()
}

/// Atomic write: a crash mid-save never leaves a truncated file.
pub fn save_json<T: Serialize>(path: &Path, value: &T, pretty: bool) {
    let bytes = if pretty { serde_json::to_vec_pretty(value) } else { serde_json::to_vec(value) };
    let Ok(bytes) = bytes else { return };
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, bytes).and_then(|_| fs::rename(&tmp, path)).is_err() {
        log!("rzen: failed to save {}", path.display());
    }
}
