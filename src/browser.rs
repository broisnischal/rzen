//! Browser core: window, chrome UI, tabs, workspaces and the tab lifecycle.
//!
//! Memory model: a tab only owns a web view (and therefore a web-content process) while
//! it is in use. Restored tabs start unloaded, idle tabs are unloaded, and a hard LRU cap
//! bounds how many pages can be resident at once.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::Write as _,
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant, SystemTime},
};

use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tao::{
    dpi::{LogicalPosition, LogicalSize},
    event_loop::{ControlFlow, EventLoop, EventLoopProxy},
    window::{Theme, Window, WindowBuilder},
};
use std::borrow::Cow;
use wry::{PageLoadEvent, Rect, WebView, WebViewBuilder};

mod features;

use crate::{
    agent::{AgentCall, AgentServer},
    history::History,
    menu,
    model::{self, Id, SEARCH_ENGINES, Session, Settings, TabData, WindowGeom, Workspace},
    perf::{self, Kind},
    platform, profiles, shields, util,
};

const GAP: f64 = 8.0;
const TICK: Duration = Duration::from_secs(15);
const PERF_EVERY: Duration = Duration::from_secs(2);
/// Loaded-tab cap while macOS reports memory pressure.
const PRESSURE_CAP: usize = 3;
const SAVE_DELAY: Duration = Duration::from_secs(2);
const MAX_CLOSED: usize = 30;
const ZOOM_STEPS: &[f64] = &[0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];
pub const SETTINGS_URL: &str = "rzen://settings";
pub const SPEED_URL: &str = "rzen://speedtest";
pub const DNS_URL: &str = "rzen://dns";
pub const BOOSTS_URL: &str = "rzen://boosts";
pub const AI_MODELS: &[(&str, &str)] = &[
    ("claude-opus-5-5", "Claude Opus 5.5"),
    ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
    ("claude-haiku-5-5", "Claude Haiku 5.5"),
];
/// Pseudo tab id for the AI panel's web view (its events never match a real tab).
const AI_ID: Id = Id::MAX - 1;
/// Height of the AI panel's provider header, drawn by the chrome above the panel's page.
const AI_HEAD: f64 = 44.0;
/// The page never gets narrower than this because of the AI panel; the panel shrinks first.
const MIN_PAGE: f64 = 420.0;
const AI_MIN: f64 = 300.0;
const MEDIA_POLL: Duration = Duration::from_secs(1);
const AGENT_BADGE: Duration = Duration::from_secs(6);
/// (id, name, home URL, prompt URL with `{}` for the encoded prompt)
pub const AI_PROVIDERS: &[(&str, &str, &str, Option<&str>)] = &[
    ("assistant", "rzen AI", "", None),
    ("chatgpt", "ChatGPT", "https://chatgpt.com/", Some("https://chatgpt.com/?q={}")),
    ("claude", "Claude", "https://claude.ai/new", Some("https://claude.ai/new?q={}")),
    ("gemini", "Gemini", "https://gemini.google.com/app", None),
    ("perplexity", "Perplexity", "https://www.perplexity.ai/", Some("https://www.perplexity.ai/search?q={}")),
];
const MEDIA_INFO_JS: &str = r#"(()=>{const m=navigator.mediaSession&&navigator.mediaSession.metadata;const a=m&&m.artwork&&m.artwork.length?m.artwork[m.artwork.length-1].src:'';const v=[...document.querySelectorAll('video')].find(x=>!x.paused&&x.videoWidth>0);return {title:(m&&m.title)||document.title,artist:(m&&m.artist)||'',art:a,video:!!v}})()"#;
/// Docked mini player: the playing <video> is lifted into the top layer (as a popover) so it
/// fills the small page viewport no matter how the site nests or offsets its player; a
/// click layer above it navigates to a sentinel URL that rzen intercepts to bring the tab
/// back. Older WebKit without popovers falls back to fixed positioning.
/// The fill is forced from a stylesheet rule, not inline styles: YouTube rewrites the video's
/// inline size and offset on every resize, which used to shift and crop the docked video.
const MINI_ON_JS: &str = r#"(()=>{if(document.getElementById('__rzen_mini'))return;const vs=[...document.querySelectorAll('video')];const v=vs.find(x=>!x.paused&&x.videoWidth>0)||vs.find(x=>x.videoWidth>0);if(!v)return;v.setAttribute('data-rzen-mini','');const I='!important;';const fill='position:fixed'+I+'inset:0'+I+'top:0'+I+'left:0'+I+'right:auto'+I+'bottom:auto'+I+'width:100vw'+I+'height:100vh'+I+'max-width:none'+I+'max-height:none'+I+'min-width:0'+I+'min-height:0'+I+'margin:0'+I+'padding:0'+I+'border:0'+I+'border-radius:0'+I+'transform:none'+I+'translate:none'+I+'scale:none'+I+'visibility:visible'+I+'opacity:1'+I+'pointer-events:auto'+I;const st=document.createElement('style');st.id='__rzen_mini_css';st.textContent='html,body{overflow:hidden!important}body *{visibility:hidden!important;pointer-events:none!important}video[data-rzen-mini]{'+fill+'object-fit:contain'+I+'background:#000'+I+'z-index:2147483646'+I+'}#__rzen_mini{'+fill+'background:transparent'+I+'z-index:2147483647'+I+'cursor:pointer}video[data-rzen-mini]::backdrop,#__rzen_mini::backdrop{background:transparent}[data-rzen-anc]{transform:none!important;filter:none!important;contain:none!important;container-type:normal!important;content-visibility:visible!important;will-change:auto!important;perspective:none!important;backdrop-filter:none!important;clip-path:none!important;mask:none!important;overflow:visible!important}';(document.head||document.documentElement).appendChild(st);const h=document.createElement('div');h.id='__rzen_mini';h.onclick=()=>{location.href='rzen-mini://focus'};document.documentElement.appendChild(h);let top=false;if(v.showPopover){try{v.setAttribute('popover','manual');v.showPopover();h.setAttribute('popover','manual');h.showPopover();top=true}catch(e){}}if(!top)for(let e=v.parentElement;e&&e!==document.documentElement;e=e.parentElement)e.setAttribute('data-rzen-anc','');dispatchEvent(new Event('resize'))})()"#;
const MINI_OFF_JS: &str = r#"(()=>{const h=document.getElementById('__rzen_mini');for(const e of[...document.querySelectorAll('video[data-rzen-mini]'),h]){if(!e)continue;try{e.matches(':popover-open')&&e.hidePopover()}catch(x){}e.removeAttribute('popover');e.removeAttribute('data-rzen-mini')}h?.remove();document.getElementById('__rzen_mini_css')?.remove();for(const e of document.querySelectorAll('[data-rzen-anc]'))e.removeAttribute('data-rzen-anc');dispatchEvent(new Event('resize'))})()"#;
const MEDIA_TOGGLE_JS: &str = r#"(()=>{const ms=[...document.querySelectorAll('video,audio')];const m=ms.find(x=>!x.paused)||ms.find(x=>x.currentTime>0)||ms[0];if(m){m.paused?m.play():m.pause()}})()"#;
const MEDIA_STEP_JS: &str = r#"((next)=>{const sel=next?['.ytp-next-button','[data-testid="control-button-skip-forward"]','button[aria-label^="Next" i]','[aria-label*="next track" i]']:['.ytp-prev-button','[data-testid="control-button-skip-back"]','button[aria-label^="Previous" i]','[aria-label*="previous track" i]'];for(const s of sel){const b=document.querySelector(s);if(b){b.click();return}}const m=[...document.querySelectorAll('video,audio')].find(x=>!x.paused);if(m)m.currentTime=Math.max(0,m.currentTime+(next?10:-10))})"#;
const MEDIA_TIME_JS: &str = "(()=>{const ms=[...document.querySelectorAll('video,audio')];const m=ms.find(x=>!x.paused)||ms.find(x=>x.currentTime>0);return m?[m.currentTime,isFinite(m.duration)?m.duration:0,!m.paused]:null})()";
const MEDIA_SEEK_JS: &str = "((f)=>{const ms=[...document.querySelectorAll('video,audio')];const m=ms.find(x=>!x.paused)||ms.find(x=>x.currentTime>0)||ms[0];if(m&&isFinite(m.duration))m.currentTime=f*m.duration})";
const MEDIA_MUTE_JS: &str = "((on)=>document.querySelectorAll('video,audio').forEach(m=>m.muted=on))";

const CHROME_HTML: &str = include_str!("ui/chrome.html");
const SETTINGS_HTML: &str = include_str!("ui/settings.html");
/// Built-in pages: (url, title, document). They share one privileged IPC channel and can
/// never navigate away, so no website can reach it.
const INTERNAL: &[(&str, &str, &str)] = &[
    (SETTINGS_URL, "Settings", SETTINGS_HTML),
    (SPEED_URL, "Speed Test", include_str!("ui/speedtest.html")),
    (DNS_URL, "DNS", include_str!("ui/dns.html")),
    (BOOSTS_URL, "Boosts", include_str!("ui/boosts.html")),
];

fn internal(url: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    INTERNAL.iter().find(|p| p.0 == url)
}
/// Without Apple's browser entitlement WebKit rejects every passkey request, so stop
/// advertising passkeys: sites then go straight to password + code instead of a passkey
/// prompt that can only fail ("Something went wrong").
const NO_PASSKEYS_JS: &str = r#"(()=>{try{Object.defineProperty(window,'PublicKeyCredential',{value:undefined,configurable:true});const c=navigator.credentials;if(c){const no=()=>Promise.reject(new DOMException('Passkeys are unavailable in this build','NotSupportedError'));const g=c.get.bind(c),m=c.create.bind(c);c.get=o=>o&&o.publicKey?no():g(o);c.create=o=>o&&o.publicKey?no():m(o);}}catch(e){}})();"#;
/// The sharpest icon the page offers at 16pt on a Retina screen: SVG, else the smallest
/// raster at least 32px, else the biggest there is.
const FAVICON_JS: &str = r#"(()=>{if(!/^https?:$/.test(location.protocol))return '';const ls=[...document.querySelectorAll('link[rel~="icon"],link[rel~="apple-touch-icon"],link[rel~="apple-touch-icon-precomposed"]')].filter(l=>l.href);const svg=ls.find(l=>/svg/.test(l.type)||/\.svg([?#]|$)/i.test(l.href));if(svg)return svg.href;const px=l=>{const s=Math.max(0,...String(l.getAttribute('sizes')||'').split(/\s+/).map(v=>parseInt(v)||0));return s||(/apple-touch/.test(l.rel)?180:16)};let best=null,bs=0;for(const l of ls){const s=px(l);if(!best||(s>=32?(bs<32||s<bs):(bs<32&&s>bs))){best=l;bs=s}}return best?best.href:location.origin+'/favicon.ico'})()"#;

pub enum UserEvent {
    Chrome(String),
    Settings(String),
    Title(Id, String),
    Load(Id, bool, String),
    Favicon(Id, String),
    /// A site icon finished downloading into the local cache.
    IconCached,
    OpenTab(String, Option<Id>),
    AdoptPopups,
    Crashed(Id),
    DownloadStarted(String),
    DownloadDone(bool),
    FindResult(bool),
    Menu(String),
    BlockerReady,
    DefaultBrowserChanged,
    DnsResult(Value),
    /// Filter lists changed state (downloading, compiling, ready…).
    BlockerStatus,
    Agent(AgentCall),
    MediaInfo(Id, Value),
    AiAsk(String),
    MediaTime(Id, Value),
    MiniFocus(Id),
    Speed(Result<Value, String>),
    NetDone(String),
    NetInfo(Value),
    ToolResult(String, Result<Value, String>),
}

/// A web view WebKit asked us to create synchronously for `window.open`; adopted as a tab
/// on the next event-loop turn.
pub struct Popup {
    pub id: Id,
    pub opener: Id,
    pub url: String,
    pub view: WebView,
}

/// State that web-view callbacks need while the browser itself is borrowed by the loop.
pub struct Shared {
    pub window: Window,
    pub proxy: EventLoopProxy<UserEvent>,
    pub content: Cell<Rect>,
    pub popups: RefCell<Vec<Popup>>,
    next_id: Cell<Id>,
    user_agent: Option<String>,
    /// `RZEN_PRIVATE=1`: nothing (cookies, cache, storage) touches disk.
    private: bool,
    /// Proxy/VPN endpoint applied to new pages (see `Settings::proxy`).
    pub proxy_url: RefCell<String>,
    /// Ad blocking is on: new pages also get the YouTube ad script.
    block_ads: Cell<bool>,
}

impl Shared {
    pub fn alloc_id(&self) -> Id {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        id
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect { position: LogicalPosition::new(x, y).into(), size: LogicalSize::new(w.max(1.0), h.max(1.0)).into() }
}

fn open_memlog() -> Option<std::fs::File> {
    let path = std::env::var_os("RZEN_MEMLOG")?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
    let _ = writeln!(f, "time,total_mb,browser_mb,pages_mb,gpu_mb,network_mb,page_processes,live_tabs,total_tabs,cpu_pct,pressure");
    Some(f)
}

fn debug() -> bool {
    std::env::var_os("RZEN_DEBUG").is_some()
}

pub fn build_tab_view(
    sh: &Rc<Shared>,
    id: Id,
    url: Option<&str>,
    store: Option<[u8; 16]>,
    customize: impl for<'b> FnOnce(WebViewBuilder<'b>) -> WebViewBuilder<'b>,
) -> wry::Result<WebView> {
    let p = &sh.proxy;
    let mut b = WebViewBuilder::new()
        .with_bounds(sh.content.get())
        .with_visible(false)
        .with_devtools(true)
        .with_accept_first_mouse(true)
        .with_back_forward_navigation_gestures(true)
        .with_incognito(sh.private)
        .with_document_title_changed_handler({
            let p = p.clone();
            move |title| {
                let _ = p.send_event(UserEvent::Title(id, title));
            }
        })
        .with_on_page_load_handler({
            let p = p.clone();
            move |event, url| {
                let _ = p.send_event(UserEvent::Load(id, matches!(event, PageLoadEvent::Finished), url));
            }
        })
        .with_navigation_handler({
            let p = p.clone();
            move |url| {
                if url.starts_with("rzen-mini:") {
                    let _ = p.send_event(UserEvent::MiniFocus(id));
                    return false;
                }
                !util::open_externally(&url)
            }
        })
        .with_download_started_handler({
            let p = p.clone();
            move |_, path| {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let _ = p.send_event(UserEvent::DownloadStarted(name));
                true
            }
        })
        .with_download_completed_handler({
            let p = p.clone();
            move |_, _, ok| {
                let _ = p.send_event(UserEvent::DownloadDone(ok));
            }
        })
        .with_new_window_req_handler({
            let sh = sh.clone();
            move |url, features| platform::new_window(&sh, id, url, features)
        });
    if let Some(ua) = &sh.user_agent {
        b = b.with_user_agent(ua);
    }
    if let Some(proxy) = util::parse_proxy(&sh.proxy_url.borrow()) {
        b = b.with_proxy_config(proxy);
    }
    if !platform::passkeys_available() {
        b = b.with_initialization_script_for_main_only(NO_PASSKEYS_JS, false);
    }
    if sh.block_ads.get() {
        b = b.with_initialization_script_for_main_only(shields::YOUTUBE_JS, false);
    }
    b = customize(platform::tab_builder_ext(b, p, id, store, sh.private));
    if let Some(url) = url {
        b = b.with_url(url);
    }
    let view = b.build_as_child(&sh.window)?;
    platform::style_content(&view);
    platform::apply_page_scripts(&view);
    Ok(view)
}

/// Built-in pages are local documents with their own IPC channel; they may never
/// navigate anywhere else, so no website can ever reach that channel.
fn build_internal_view(sh: &Rc<Shared>, html: &str) -> wry::Result<WebView> {
    let p = sh.proxy.clone();
    let view = WebViewBuilder::new()
        .with_html(html)
        .with_bounds(sh.content.get())
        .with_visible(false)
        .with_accept_first_mouse(true)
        .with_ipc_handler(move |req| {
            let _ = p.send_event(UserEvent::Settings(req.into_body()));
        })
        .with_navigation_handler(|url| url.starts_with("about:") || url.starts_with("data:"))
        .build_as_child(&sh.window)?;
    platform::style_content(&view);
    Ok(view)
}

pub struct Tab {
    pub d: TabData,
    pub view: Option<WebView>,
    pub last_active: Instant,
    pub loading: bool,
    pub can_back: bool,
    pub can_fwd: bool,
    /// Page host the current favicon was fetched for.
    icon_host: String,
    audible: bool,
    muted: bool,
    media: Option<Value>,
}

impl Tab {
    fn new(d: TabData) -> Self {
        Self { d, view: None, last_active: Instant::now(), loading: false, can_back: false, can_fwd: false, icon_host: String::new(), audible: false, muted: false, media: None }
    }

    fn is_settings(&self) -> bool {
        self.d.url == SETTINGS_URL
    }

    fn is_internal(&self) -> bool {
        internal(&self.d.url).is_some()
    }

    /// Live URL (catches SPA navigations) falling back to the last known one.
    fn current_url(&self) -> String {
        if self.is_internal() {
            return self.d.url.clone();
        }
        self.view
            .as_ref()
            .and_then(platform::view_url)
            .filter(|u| !u.is_empty() && u != "about:blank")
            .unwrap_or_else(|| self.d.url.clone())
    }
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
enum Msg {
    Ready,
    Select { id: Id },
    Close { id: Id },
    NewTab,
    EditUrl,
    Back,
    Forward,
    Reload,
    PaletteClose,
    Go {
        value: String,
        mode: String,
        tab: Option<Id>,
        #[serde(default)]
        search: bool,
    },
    Ws { id: Id },
    WsNew,
    WsRename { id: Id, name: String },
    TabCtx { id: Id, x: f64, y: f64 },
    WsCtx { id: Id, x: f64, y: f64 },
    Move { id: Id, before: Option<Id>, section: String },
    MoveWs { id: Id, ws: Id },
    Sidebar,
    SidebarW { w: f64 },
    ContentX { x: f64 },
    SidebarDone,
    Peek { on: bool },
    Drag,
    TitlebarDblClick,
    Find { q: String, back: bool, again: bool },
    FindClose,
    OpenDownloads,
    Ai,
    AiW { w: f64 },
    AiProvider { id: String },
    AiAskPage,
    MediaToggle,
    MediaStep { next: bool },
    MediaMute { id: Option<Id> },
    MediaFocus,
    MediaClose,
    ProfileMenu { x: f64, y: f64 },
    AiMenu { x: f64, y: f64 },
    /// Right-click on the address bar; `r` is the bar's rect (anchor for Share…).
    UrlMenu { x: f64, y: f64, r: (f64, f64, f64, f64) },
    Share { r: (f64, f64, f64, f64) },
    CopyUrl,
    OpenSettings,
    MediaSeek { frac: f64 },
    /// A command chosen in the command palette (⌃⇧P).
    Command { id: String },
    /// Where the sidebar's video slot is (None = no slot): the docked mini player goes there.
    MiniRect { r: Option<(f64, f64, f64, f64)> },
    AiKeyGet,
    AiKeySet { key: String },
    Tool { call: String, method: String, params: Value },
    Unlock { password: String },
    Setup { name: String, password: String },
    NewProfile { name: String, password: String },
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
enum SettingsMsg {
    Get,
    Set { key: String, value: Value },
    ClearHistory,
    ClearData,
    MakeDefault,
    OpenDownloads,
    Unload { id: Id },
    ProfileName { name: String },
    AiKeyRemove,
    SpeedTest,
    FlushDns,
    ClearCache,
    InstallDns { provider: String, custom: Option<String> },
    NetInfo,
    OpenVpnSettings,
    DnsBench { servers: Vec<String> },
    DnsLookup { name: String, server: String, kind: String },
    ProfilePassword { password: String },
    OpenPage { url: String },
    SetSystemDns { provider: String },
    RefreshFilters,
    BoostSave { boost: crate::boosts::Boost },
    BoostDelete { id: u64 },
    BoostReload { id: u64 },
    BoostOpen { id: u64 },
}

pub struct Browser {
    sh: Rc<Shared>,
    chrome: WebView,
    _menu: Menu,
    tabs: Vec<Tab>,
    workspaces: Vec<Workspace>,
    active_ws: Id,
    closed: Vec<TabData>,
    settings: Settings,
    settings_mtime: Option<SystemTime>,
    history: History,
    dir: PathBuf,
    /// Tab whose web view is currently shown.
    visible: Option<Id>,
    sidebar_open: bool,
    sidebar_w: f64,
    /// Sidebar opened temporarily (find bar / inline command bar) while in compact mode.
    sidebar_forced: bool,
    /// Chrome is stacked above page content.
    overlay: bool,
    peek: bool,
    finding: bool,
    chrome_ready: bool,
    palette_on_ready: bool,
    /// History version the command bar's local index was built from.
    palette_history: Option<u64>,
    toast: Option<(String, Instant)>,
    ui_dirty: bool,
    save_at: Option<Instant>,
    next_tick: Instant,
    exiting: bool,
    /// CPU time per pid at the previous sample, for CPU %.
    perf_prev: HashMap<i32, u64>,
    perf_at: Instant,
    perf_next: Instant,
    /// `RZEN_MEMLOG=/path/file.csv` appends a sample row every tick.
    memlog: Option<std::fs::File>,
    pressure: u32,
    profile: ProfileCtx,
    /// "setup" (first run: create a profile) or "lock" (password) blocks the UI.
    gate: Option<&'static str>,
    ai_open: bool,
    ai_w: f64,
    ai_view: Option<WebView>,
    media_next: Instant,
    /// Tab shown in the sidebar's mini player.
    media_tab: Option<Id>,
    agent: Option<AgentServer>,
    agent_seen: Option<Instant>,
    /// Agent `page.wait` calls: (tab, started, deadline, reply).
    waiters: Vec<(Id, Instant, Instant, mpsc::Sender<Result<Value, String>>)>,
    /// Exit after this turn (profile switch).
    quit: bool,
    /// Where the last address-bar menu was opened, so its Share… item can anchor there.
    share_anchor: (f64, f64, f64, f64),
    /// Sidebar video slot reported by the chrome, and the tab docked in it.
    mini_rect: Option<(f64, f64, f64, f64)>,
    mini_tab: Option<Id>,
    /// Settings section to scroll to once the settings page is ready.
    settings_focus: Option<String>,
    /// Your site customizations (Boosts).
    boosts: Vec<crate::boosts::Boost>,
    /// "Boost This Site" asked for this host before the Boosts page was open.
    boost_intent: Option<String>,
}

/// Which local profile this window runs, chosen in `main`.
/// Profile id of a private window.
pub const PRIVATE_ID: &str = "private";

pub struct ProfileCtx {
    pub id: String,
    pub dir: PathBuf,
    pub gate: Option<&'static str>,
}

impl Browser {
    pub fn new(event_loop: &EventLoop<UserEvent>, profile: ProfileCtx) -> Result<Self, Box<dyn std::error::Error>> {
        let dir = profile.dir.clone();
        let settings_path = dir.join("settings.json");
        let mut settings: Settings = model::load_json(&settings_path);
        if profile.id == PRIVATE_ID {
            // Agents drive your normal profile, never a private window.
            settings.agent_enabled = false;
        }
        // Rewrite so new keys show up for hand-editing.
        model::save_json(&settings_path, &settings, true);
        let mut session: Session = model::load_json(&dir.join("session.json"));
        if settings.startup == "fresh" {
            session.tabs.retain(|t| t.pinned || t.essential);
        }
        session.sanitize();
        let history = History::load(dir.join("history.json"));

        let mut wb = WindowBuilder::new().with_title("rzen").with_visible(false).with_min_inner_size(LogicalSize::new(640.0, 420.0));
        wb = match session.window {
            Some(g) => wb.with_inner_size(LogicalSize::new(g.w, g.h)).with_position(LogicalPosition::new(g.x, g.y)),
            None => wb.with_inner_size(LogicalSize::new(1320.0, 860.0)),
        };
        #[cfg(target_os = "macos")]
        {
            use tao::platform::macos::WindowBuilderExtMacOS;
            wb = wb
                .with_titlebar_transparent(true)
                .with_title_hidden(true)
                .with_fullsize_content_view(true)
                // Close button centered on the sidebar's icon column (x 26) and on the toolbar
                // row (y 22): tao places the button center at (x + 7, y - 2).
                .with_traffic_light_inset(LogicalPosition::new(19.0, 24.0));
        }
        let window = wb.build(event_loop)?;
        platform::set_window_background(&window);

        let menu = menu::build();
        #[cfg(target_os = "macos")]
        menu.init_for_nsapp();
        #[cfg(target_os = "windows")]
        unsafe {
            use tao::platform::windows::WindowExtWindows;
            let _ = menu.init_for_hwnd(window.hwnd() as _);
            let _ = menu.hide_for_hwnd(window.hwnd() as _);
        }
        let proxy = event_loop.create_proxy();
        {
            let p = proxy.clone();
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                let _ = p.send_event(UserEvent::Menu(e.id.0));
            }));
        }

        let size = window.inner_size().to_logical::<f64>(window.scale_factor());
        let sh = Rc::new(Shared {
            window,
            proxy: proxy.clone(),
            content: Cell::new(rect(0.0, 0.0, 1.0, 1.0)),
            popups: RefCell::new(Vec::new()),
            next_id: Cell::new(session.next_id),
            user_agent: platform::user_agent(),
            private: std::env::var_os("RZEN_PRIVATE").is_some() || profile.id == PRIVATE_ID,
            proxy_url: RefCell::new(String::new()),
            block_ads: Cell::new(false),
        });
        {
            let p = proxy.clone();
            crate::filters::set_listener(move || {
                let _ = p.send_event(UserEvent::BlockerStatus);
            });
        }

        let chrome = WebViewBuilder::new()
            .with_html(CHROME_HTML)
            .with_transparent(true)
            .with_bounds(rect(0.0, 0.0, size.width, size.height))
            .with_accept_first_mouse(true)
            .with_devtools(cfg!(debug_assertions))
            .with_initialization_script(format!(
                "window.__RZEN={{platform:{:?},overlay:{},share:{}}};",
                std::env::consts::OS,
                platform::OVERLAY,
                platform::SHARE
            ))
            .with_ipc_handler({
                let p = proxy.clone();
                move |req| {
                    let _ = p.send_event(UserEvent::Chrome(req.into_body()));
                }
            })
            .with_navigation_handler(|url| url.starts_with("about:") || url.starts_with("data:"))
            .with_custom_protocol(crate::favicons::SCHEME.into(), |_, req| {
                let res = wry::http::Response::builder();
                match crate::favicons::serve(req.uri().path()) {
                    Some((bytes, mime)) => res.header("Content-Type", mime).header("Cache-Control", "max-age=31536000").body(Cow::Owned(bytes)),
                    None => res.status(404).body(Cow::Borrowed(&[][..])),
                }
                .unwrap_or_else(|_| wry::http::Response::new(Cow::Borrowed(&[][..])))
            })
            .build_as_child(&sh.window)?;
        platform::plain_text_input(&chrome);
        // Icons stored before this run: make sure every tab's is on disk.
        for href in session.tabs.iter().filter_map(|t| t.favicon.clone()) {
            let p = proxy.clone();
            crate::favicons::ensure(&href, move || {
                let _ = p.send_event(UserEvent::IconCached);
            });
        }

        let settings_mtime = std::fs::metadata(&settings_path).and_then(|m| m.modified()).ok();
        let palette_on_ready = session.tabs.is_empty() && profile.gate.is_none();
        let (ai_open, ai_w) = (session.ai_open, session.ai_w);
        let mut b = Self {
            sh,
            chrome,
            _menu: menu,
            tabs: session.tabs.into_iter().map(Tab::new).collect(),
            workspaces: session.workspaces,
            active_ws: session.active_ws,
            closed: session.closed,
            settings,
            settings_mtime,
            history,
            boosts: crate::boosts::load(&dir),
            dir,
            visible: None,
            sidebar_open: session.sidebar_open,
            sidebar_w: session.sidebar_w.round(),
            sidebar_forced: false,
            overlay: false,
            peek: false,
            finding: false,
            chrome_ready: false,
            palette_on_ready,
            palette_history: None,
            toast: None,
            ui_dirty: true,
            save_at: None,
            next_tick: Instant::now() + TICK,
            exiting: false,
            perf_prev: HashMap::new(),
            perf_at: Instant::now(),
            perf_next: Instant::now(),
            memlog: open_memlog(),
            pressure: 1,
            gate: profile.gate,
            profile,
            ai_open,
            ai_w,
            ai_view: None,
            media_next: Instant::now(),
            media_tab: None,
            agent: None,
            agent_seen: None,
            waiters: Vec::new(),
            quit: false,
            share_anchor: (0.0, 0.0, 1.0, 1.0),
            mini_rect: None,
            mini_tab: None,
            settings_focus: None,
            boost_intent: None,
        };
        platform::set_page_scripts(crate::boosts::scripts(&b.boosts));
        b.apply_settings();
        b.layout();
        platform::set_traffic_lights(&b.sh.window, b.sidebar_open);
        if b.gate.is_none() {
            b.enter();
        }
        b.sh.window.set_visible(true);
        Ok(b)
    }

    // ---------- lookup helpers ----------

    fn idx(&self, id: Id) -> Option<usize> {
        self.tabs.iter().position(|t| t.d.id == id)
    }

    fn tab(&self, id: Id) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.d.id == id)
    }

    fn ws(&self) -> &Workspace {
        self.workspaces.iter().find(|w| w.id == self.active_ws).unwrap_or(&self.workspaces[0])
    }

    fn ws_mut(&mut self) -> &mut Workspace {
        let i = self.workspaces.iter().position(|w| w.id == self.active_ws).unwrap_or(0);
        &mut self.workspaces[i]
    }

    fn active_id(&self) -> Option<Id> {
        self.ws().active
    }

    fn active_view(&self) -> Option<&WebView> {
        self.visible.and_then(|id| self.tab(id)).and_then(|t| t.view.as_ref())
    }

    /// The visible page, unless WebKit has moved it into its element-fullscreen window.
    fn attached_view(&self) -> Option<&WebView> {
        self.active_view().filter(|v| platform::attached(v, &self.sh.window))
    }

    /// Sidebar order for the current workspace: essentials, pinned, then regular tabs.
    fn display_order(&self) -> Vec<Id> {
        let ws = self.active_ws;
        let mut v: Vec<&Tab> = self.tabs.iter().filter(|t| t.d.essential || t.d.workspace == ws).collect();
        // stable sort keeps the user's order inside each section
        v.sort_by_key(|t| if t.d.essential { 0 } else if t.d.pinned { 1 } else { 2 });
        v.into_iter().map(|t| t.d.id).collect()
    }

    fn neighbor(&self, id: Id) -> Option<Id> {
        let order = self.display_order();
        let i = order.iter().position(|&x| x == id)?;
        order.get(i + 1).or_else(|| i.checked_sub(1).and_then(|j| order.get(j))).copied()
    }

    /// Website-data store: one per profile, plus one per isolated space inside it.
    fn store_for(&self, d: &TabData) -> Option<[u8; 16]> {
        let isolated = !d.essential && self.workspaces.iter().any(|w| w.id == d.workspace && w.isolated);
        profiles::store_id(&self.profile.id, isolated.then_some(d.workspace))
    }

    fn win_size(&self) -> (f64, f64) {
        let s = self.sh.window.inner_size().to_logical::<f64>(self.sh.window.scale_factor());
        (s.width, s.height)
    }

    /// Width the AI panel takes from the page (panel + gutter), 0 when closed.
    fn ai_extra(&self) -> f64 {
        let w = self.ai_width();
        if w > 0.0 { w + GAP } else { 0.0 }
    }

    /// Panel width actually used: the preferred width, shrunk (down to `AI_MIN`) so the page
    /// keeps `MIN_PAGE` on narrow windows. 0 when closed.
    fn ai_width(&self) -> f64 {
        if !self.ai_open || self.gate.is_some() {
            return 0.0;
        }
        let (w, _) = self.win_size();
        let x = if self.sidebar_open { self.sidebar_w } else { GAP };
        self.ai_w.min((w - x - 3.0 * GAP - MIN_PAGE).max(AI_MIN))
    }

    /// With the AI panel open, a window too narrow for sidebar + panel + page collapses the
    /// sidebar (it still peeks from the edge) rather than crushing the page.
    fn fit_ai(&mut self) {
        let (w, _) = self.win_size();
        if self.ai_open && self.sidebar_open && w - self.sidebar_w - 3.0 * GAP - AI_MIN < MIN_PAGE {
            self.set_sidebar(false);
        }
    }

    fn content_rect(&self) -> Rect {
        let (w, h) = self.win_size();
        let x = if self.sidebar_open { self.sidebar_w } else { GAP };
        rect(x, GAP, w - x - GAP - self.ai_extra(), h - 2.0 * GAP)
    }

    fn ai_rect(&self) -> Rect {
        let (w, h) = self.win_size();
        let aw = self.ai_width();
        rect(w - GAP - aw, GAP + AI_HEAD, aw, h - 2.0 * GAP - AI_HEAD)
    }

    fn js(&self, script: &str) {
        let _ = self.chrome.evaluate_script(script);
    }

    fn dirty(&mut self) {
        self.ui_dirty = true;
        self.save_at.get_or_insert(Instant::now() + SAVE_DELAY);
    }

    fn set_toast(&mut self, msg: impl Into<String>, secs: u64) {
        self.toast = Some((msg.into(), Instant::now() + Duration::from_secs(secs)));
        self.ui_dirty = true;
    }

    // ---------- layout & stacking ----------

    pub fn layout(&mut self) {
        self.fit_ai();
        let (w, h) = self.win_size();
        let _ = self.chrome.set_bounds(rect(0.0, 0.0, w, h));
        let r = self.content_rect();
        self.sh.content.set(r);
        if let Some(v) = self.attached_view() {
            let _ = v.set_bounds(r);
        }
        if let Some(v) = &self.ai_view {
            let _ = v.set_bounds(self.ai_rect());
        }
    }

    fn set_overlay(&mut self, on: bool) {
        if self.overlay == on {
            return;
        }
        self.overlay = on;
        platform::set_chrome_on_top(&self.chrome, on);
        if on {
            let _ = self.chrome.focus();
        } else {
            self.focus_content();
        }
    }

    fn focus_content(&self) {
        match self.attached_view() {
            Some(v) => {
                let _ = v.focus();
            }
            None => {
                let _ = self.chrome.focus();
            }
        }
    }

    /// The chrome animates the sidebar and streams the page's left edge back every frame
    /// (`ContentX`), so page and sidebar move as one; `SidebarDone` snaps the final layout.
    fn set_sidebar(&mut self, open: bool) {
        self.sidebar_open = open;
        if !open {
            self.mini_rect = None;
        }
        if std::mem::take(&mut self.peek) {
            self.set_overlay(false);
        }
        platform::set_traffic_lights(&self.sh.window, open);
        if self.chrome_ready {
            self.js(&format!("__rzen.setSidebar({open})"));
        } else {
            self.layout();
        }
        self.dirty();
    }

    fn force_sidebar(&mut self) {
        if !self.sidebar_open {
            self.sidebar_forced = true;
            self.set_sidebar(true);
        }
    }

    fn release_sidebar(&mut self) {
        if self.sidebar_forced && !self.finding {
            self.sidebar_forced = false;
            self.set_sidebar(false);
        }
    }

    // ---------- tab lifecycle ----------

    fn ensure_view(&mut self, i: usize) {
        if self.tabs[i].view.is_some() {
            return;
        }
        let d = &self.tabs[i].d;
        let built = if let Some(page) = internal(&d.url) {
            build_internal_view(&self.sh, page.2)
        } else {
            let url = if d.url.is_empty() { "about:blank" } else { d.url.as_str() };
            build_tab_view(&self.sh, d.id, Some(url), self.store_for(d), |b| b)
        };
        match built {
            Ok(view) => {
                if (d.zoom - 1.0).abs() > 1e-3 {
                    let _ = view.zoom(d.zoom);
                }
                let t = &mut self.tabs[i];
                t.loading = !t.is_internal();
                t.view = Some(view);
            }
            Err(e) => eprintln!("rzen: could not create web view: {e}"),
        }
    }

    fn hide_visible(&mut self) {
        if let Some(v) = self.visible.take().and_then(|id| self.tab(id)).and_then(|t| t.view.as_ref()) {
            let _ = v.set_visible(false);
            platform::background(v);
        }
    }

    fn show(&mut self, id: Id) {
        let Some(i) = self.idx(id) else { return };
        if !self.tabs[i].d.essential && self.tabs[i].d.workspace != self.active_ws {
            self.hide_visible();
            self.active_ws = self.tabs[i].d.workspace;
        }
        self.ws_mut().active = Some(id);
        // Nothing shows behind the sign-up or lock screen; `enter` shows this tab afterwards.
        if self.gate.is_some() {
            self.hide_visible();
            return self.dirty();
        }
        if self.visible != Some(id) {
            self.hide_visible();
        }
        let was_live = self.tabs[i].view.is_some();
        self.ensure_view(i);
        let r = self.content_rect();
        let t = &mut self.tabs[i];
        t.last_active = Instant::now();
        if let Some(v) = t.view.as_ref().filter(|v| platform::attached(v, &self.sh.window)) {
            let _ = v.set_bounds(r);
            let _ = v.set_visible(true);
            platform::foreground(v);
            if !self.overlay {
                let _ = v.focus();
            }
        }
        let title = if t.d.title.is_empty() { util::short_host(&t.d.url).to_string() } else { t.d.title.clone() };
        self.sh.window.set_title(&if self.sh.private { format!("Private — {title}") } else { title });
        self.visible = Some(id);
        if was_live {
            self.sync_load_state(id);
        }
        self.enforce_live_cap();
        self.sync_mini();
        self.dirty();
    }

    fn fetch_favicon(&self, id: Id) {
        let Some(v) = self.tab(id).and_then(|t| t.view.as_ref()) else { return };
        let p = self.sh.proxy.clone();
        let _ = v.evaluate_script_with_callback(FAVICON_JS, move |r| {
            if let Ok(href) = serde_json::from_str::<String>(&r) {
                let _ = p.send_event(UserEvent::Favicon(id, href));
            }
        });
    }

    /// WebKit doesn't report loads that finish while a view is hidden; ask the page.
    fn sync_load_state(&self, id: Id) {
        let Some(v) = self.tab(id).filter(|t| t.loading).and_then(|t| t.view.as_ref()) else { return };
        let p = self.sh.proxy.clone();
        let _ = v.evaluate_script_with_callback("document.readyState", move |r| {
            if r.contains("complete") {
                let _ = p.send_event(UserEvent::Load(id, true, String::new()));
            }
        });
    }

    fn open_tab(&mut self, url: String, after: Option<Id>, activate: bool) -> Id {
        let id = self.sh.alloc_id();
        let workspace = self.active_ws;
        let title = internal(&url).map_or_else(String::new, |p| p.1.to_string());
        let pos = after.and_then(|a| self.idx(a)).map_or(self.tabs.len(), |i| i + 1);
        self.tabs.insert(pos, Tab::new(TabData { id, url, title, workspace, ..Default::default() }));
        if activate {
            self.show(id);
        }
        self.dirty();
        id
    }

    fn open_settings(&mut self) {
        self.open_internal(SETTINGS_URL);
    }

    /// One tab per built-in page: reuse it if it's already open.
    fn open_internal(&mut self, url: &str) {
        match self.tabs.iter().find(|t| t.d.url == url).map(|t| t.d.id) {
            Some(id) => self.show(id),
            None => {
                self.open_tab(url.into(), None, true);
            }
        }
    }

    /// Runs a script in every open built-in page (each defines the hooks it cares about).
    fn internal_js(&self, script: &str) {
        for v in self.tabs.iter().filter(|t| t.is_internal()).filter_map(|t| t.view.as_ref()) {
            let _ = v.evaluate_script(script);
        }
    }

    fn navigate(&mut self, id: Id, url: String) {
        let Some(i) = self.idx(id) else { return };
        if self.tabs[i].is_internal() {
            self.open_tab(url, Some(id), true);
            return;
        }
        let t = &mut self.tabs[i];
        t.d.url = url;
        match &t.view {
            Some(v) => {
                let _ = v.load_url(&t.d.url);
                t.loading = true;
                self.dirty();
            }
            None => self.show(id),
        }
    }

    /// Drops the web view (and its process); the tab stays in the sidebar.
    fn unload(&mut self, id: Id) {
        let Some(i) = self.idx(id) else { return };
        let url = self.tabs[i].current_url();
        let t = &mut self.tabs[i];
        t.d.url = url;
        if let Some(v) = t.view.take() {
            platform::release(&v);
        }
        t.loading = false;
        t.audible = false;
        t.muted = false;
        if self.visible == Some(id) {
            self.visible = None;
        }
        if self.media_tab == Some(id) {
            self.media_tab = None;
        }
        if self.mini_tab == Some(id) {
            self.mini_tab = None;
        }
        self.ui_dirty = true;
    }

    /// Unloads a background tab unless it's playing audio or using the camera/microphone.
    /// Decided from WebKit's own state in this process: no JavaScript, so it also works for
    /// pages WebKit has suspended in the background.
    fn request_unload(&mut self, id: Id) {
        let busy = self.tab(id).and_then(|t| t.view.as_ref()).is_some_and(platform::is_busy);
        if !busy && self.visible != Some(id) {
            self.unload(id);
        }
    }

    fn enforce_live_cap(&mut self) {
        let mut live: Vec<(Instant, Id)> = self
            .tabs
            .iter()
            // Essentials are always-on apps (mail, chat, Teams): never unloaded to make room.
            .filter(|t| t.view.is_some() && Some(t.d.id) != self.visible && !t.d.essential)
            .map(|t| (t.last_active, t.d.id))
            .collect();
        let cap = if self.pressure >= perf::PRESSURE_WARN { PRESSURE_CAP } else { self.settings.max_live_tabs };
        let keep = cap.max(1) - 1;
        if live.len() > keep {
            live.sort_unstable();
            let excess = live.len() - keep;
            live[..excess].iter().for_each(|&(_, id)| self.request_unload(id));
            self.ui_dirty = true;
        }
    }

    /// Keeps rzen under the user's memory limit by unloading least-recently-used background
    /// tabs, counting what each one's page process actually frees. The visible tab, the
    /// docked mini player, Essentials and tabs playing audio or using the camera/mic are never touched.
    fn enforce_memory_limit(&mut self) {
        let limit = self.settings.memory_limit_mb * 1_048_576;
        if limit == 0 {
            return;
        }
        let tab_pids: HashMap<i32, Id> = self.tabs.iter().filter_map(|t| Some((platform::web_process_pid(t.view.as_ref()?)?, t.d.id))).collect();
        let mut procs = perf::processes();
        if !perf::exact() {
            procs.retain(|p| p.kind != Kind::Page || tab_pids.contains_key(&p.pid));
        }
        let mut over = procs.iter().map(|p| p.footprint).sum::<u64>().saturating_sub(limit);
        if over == 0 {
            return;
        }
        let mut cands: Vec<(Instant, Id, u64)> = procs
            .iter()
            .filter_map(|p| tab_pids.get(&p.pid).map(|&id| (id, p.footprint)))
            .filter(|&(id, _)| Some(id) != self.visible && Some(id) != self.mini_tab && !self.tab(id).is_some_and(|t| t.d.essential))
            .filter_map(|(id, bytes)| Some((self.tab(id)?.last_active, id, bytes)))
            .collect();
        cands.sort_unstable_by_key(|c| c.0);
        let mut freed = 0;
        for (_, id, bytes) in cands {
            if over == 0 {
                break;
            }
            if self.tab(id).and_then(|t| t.view.as_ref()).is_some_and(platform::is_busy) {
                continue;
            }
            self.unload(id);
            over = over.saturating_sub(bytes);
            freed += 1;
        }
        if freed > 0 {
            self.ui_dirty = true;
        }
    }

    /// Clears `active` in other workspaces that point at a tab no longer shared with them.
    fn detach(&mut self, id: Id, keep_ws: Option<Id>) {
        for w in &mut self.workspaces {
            if w.active == Some(id) && Some(w.id) != keep_ws {
                w.active = None;
            }
        }
    }

    fn close(&mut self, id: Id) {
        let Some(i) = self.idx(id) else { return };
        let next = if self.active_id() == Some(id) { self.neighbor(id) } else { self.active_id() };
        let mut t = self.tabs.remove(i);
        t.d.url = t.current_url();
        if let Some(v) = t.view.take() {
            platform::release(&v);
        }
        if self.visible == Some(id) {
            self.visible = None;
        }
        if !t.is_internal() {
            self.closed.push(t.d);
            if self.closed.len() > MAX_CLOSED {
                self.closed.remove(0);
            }
        }
        self.detach(id, None);
        match next {
            Some(n) => self.show(n),
            None => self.sh.window.set_title("rzen"),
        }
        self.dirty();
    }

    /// ⌘W: pinned tabs and essentials are unloaded instead of closed (Zen/Arc behaviour).
    fn close_or_unload_active(&mut self) {
        let Some(id) = self.active_id() else { return };
        let keep = self.tab(id).is_some_and(|t| t.d.pinned || t.d.essential);
        if !keep {
            return self.close(id);
        }
        let next = self.neighbor(id);
        self.hide_visible();
        self.unload(id);
        self.ws_mut().active = None;
        match next {
            Some(n) => self.show(n),
            None => self.dirty(),
        }
    }

    fn reopen_closed(&mut self) {
        let Some(mut d) = self.closed.pop() else { return };
        if !self.workspaces.iter().any(|w| w.id == d.workspace) {
            d.workspace = self.active_ws;
        }
        let id = d.id;
        self.tabs.push(Tab::new(d));
        self.show(id);
    }

    fn cycle_tab(&mut self, step: isize) {
        let order = self.display_order();
        if order.is_empty() {
            return;
        }
        let cur = self.active_id().and_then(|a| order.iter().position(|&x| x == a)).unwrap_or(0) as isize;
        let next = (cur + step).rem_euclid(order.len() as isize) as usize;
        self.show(order[next]);
    }

    fn zoom(&mut self, dir: i8) {
        let Some(i) = self.active_id().and_then(|id| self.idx(id)) else { return };
        let t = &mut self.tabs[i];
        let cur = ZOOM_STEPS.iter().position(|z| (z - t.d.zoom).abs() < 0.01).unwrap_or(5);
        let z = match dir {
            0 => 1.0,
            d if d > 0 => ZOOM_STEPS[(cur + 1).min(ZOOM_STEPS.len() - 1)],
            _ => ZOOM_STEPS[cur.saturating_sub(1)],
        };
        t.d.zoom = z;
        if let Some(v) = &t.view {
            let _ = v.zoom(z);
        }
        self.set_toast(format!("Zoom {:.0}%", z * 100.0), 2);
        self.dirty();
    }

    // ---------- workspaces ----------

    fn switch_ws(&mut self, id: Id) {
        if id == self.active_ws || !self.workspaces.iter().any(|w| w.id == id) {
            return;
        }
        self.hide_visible();
        self.active_ws = id;
        match self.ws().active {
            Some(a) => self.show(a),
            None => {
                self.sh.window.set_title("rzen");
                self.focus_content();
            }
        }
        self.dirty();
    }

    fn cycle_ws(&mut self, step: isize) {
        let n = self.workspaces.len() as isize;
        let cur = self.workspaces.iter().position(|w| w.id == self.active_ws).unwrap_or(0) as isize;
        let id = self.workspaces[(cur + step).rem_euclid(n) as usize].id;
        self.switch_ws(id);
    }

    fn new_workspace(&mut self) {
        let id = self.sh.alloc_id();
        let name = format!("Space {}", self.workspaces.len() + 1);
        self.workspaces.push(Workspace { id, name, isolated: false, active: None });
        self.switch_ws(id);
        self.js(&format!("__rzen.renameWs({id})"));
    }

    fn delete_ws(&mut self, id: Id) {
        if self.workspaces.len() < 2 {
            return;
        }
        if self.visible.and_then(|v| self.tab(v)).is_some_and(|t| !t.d.essential && t.d.workspace == id) {
            self.hide_visible();
        }
        self.tabs.retain(|t| t.d.essential || t.d.workspace != id);
        self.workspaces.retain(|w| w.id != id);
        if self.active_ws == id {
            self.active_ws = self.workspaces[0].id;
            if let Some(a) = self.ws().active {
                self.show(a);
            }
        }
        self.dirty();
    }

    fn toggle_isolation(&mut self, id: Id) {
        let Some(w) = self.workspaces.iter_mut().find(|w| w.id == id) else { return };
        w.isolated = !w.isolated;
        let msg = if w.isolated { "Space now has its own cookies and logins" } else { "Space now shares cookies and logins" };
        let ids: Vec<Id> = self.tabs.iter().filter(|t| !t.d.essential && t.d.workspace == id).map(|t| t.d.id).collect();
        let shown = self.visible.filter(|v| ids.contains(v));
        ids.into_iter().for_each(|t| self.unload(t));
        if let Some(v) = shown {
            self.show(v);
        }
        self.set_toast(msg, 3);
        self.dirty();
    }

    fn move_tab(&mut self, id: Id, before: Option<Id>, section: &str) {
        let Some(i) = self.idx(id) else { return };
        let mut t = self.tabs.remove(i);
        let was_essential = t.d.essential;
        t.d.essential = section == "essential";
        t.d.pinned = section == "pinned";
        if !t.d.essential {
            t.d.workspace = self.active_ws;
        }
        let pos = before.and_then(|b| self.idx(b)).unwrap_or(self.tabs.len());
        self.tabs.insert(pos, t);
        if was_essential && section != "essential" {
            self.detach(id, Some(self.active_ws));
        }
        self.dirty();
    }

    fn move_to_ws(&mut self, id: Id, ws: Id) {
        if !self.workspaces.iter().any(|w| w.id == ws) {
            return;
        }
        let next = if self.active_id() == Some(id) { Some(self.neighbor(id)) } else { None };
        let Some(i) = self.idx(id) else { return };
        self.tabs[i].d.workspace = ws;
        self.tabs[i].d.essential = false;
        self.detach(id, None);
        if let Some(w) = self.workspaces.iter_mut().find(|w| w.id == ws) {
            w.active = Some(id);
        }
        if let Some(next) = next {
            self.hide_visible();
            self.ws_mut().active = None;
            if let Some(n) = next {
                self.show(n);
            }
        }
        self.dirty();
    }

    // ---------- command bar & find ----------

    fn open_palette(&mut self, mode: &str) {
        if !self.chrome_ready {
            self.palette_on_ready = true;
            return;
        }
        let value = match mode {
            "edit" => self.active_id().and_then(|id| self.tab(id)).map(Tab::current_url).unwrap_or_default(),
            _ => String::new(),
        };
        if !platform::OVERLAY {
            self.force_sidebar();
        }
        // Most recently used first, so ⌃P ⏎ jumps back to the previous tab.
        let mut order: Vec<&Tab> = self.tabs.iter().collect();
        order.sort_by_key(|t| std::cmp::Reverse(t.last_active));
        let space = |t: &Tab| -> String {
            if t.d.essential {
                "Essentials".into()
            } else {
                self.workspaces.iter().find(|w| w.id == t.d.workspace).map(|w| w.name.clone()).unwrap_or_default()
            }
        };
        let tabs: Vec<Value> = order
            .into_iter()
            .map(|t| json!([t.d.id, internal(&t.d.url).map_or(t.d.title.as_str(), |p| p.1), t.d.url, t.d.favicon.as_deref().map(crate::favicons::local), space(t), self.visible == Some(t.d.id)]))
            .collect();
        let history = (self.palette_history != Some(self.history.version)).then(|| {
            self.palette_history = Some(self.history.version);
            let h = &self.history;
            h.top(2000).into_iter().map(|(url, title, score)| json!([url, title, score, h.icon_for(url).map(crate::favicons::local)])).collect::<Vec<_>>()
        });
        self.js(&format!(
            "__rzen.palette({})",
            json!({ "mode": mode, "value": value, "tabs": tabs, "history": history })
        ));
        self.set_overlay(true);
    }

    fn close_palette(&mut self) {
        self.peek = false;
        self.set_overlay(false);
        self.release_sidebar();
        self.ui_dirty = true;
    }

    fn open_find(&mut self) {
        self.finding = true;
        self.force_sidebar();
        self.ui_dirty = true;
        let _ = self.chrome.focus();
        self.js("__rzen.find()");
    }

    fn find(&self, q: &str, back: bool, again: bool) {
        let Some(v) = self.active_view() else { return };
        let script = format!(
            "((q,b,a)=>{{const s=getSelection();if(!a&&s.rangeCount)s.collapseToStart();return window.find(q,false,b,true,false,false,false)}})({},{back},{again})",
            json!(q)
        );
        let p = self.sh.proxy.clone();
        let _ = v.evaluate_script_with_callback(&script, move |r| {
            let _ = p.send_event(UserEvent::FindResult(r.trim() == "true"));
        });
    }

    // ---------- settings ----------

    fn apply_settings(&mut self) {
        let theme = match self.settings.theme.as_str() {
            "light" => Some(Theme::Light),
            "dark" => Some(Theme::Dark),
            _ => None,
        };
        self.sh.window.set_theme(theme);
        self.sh.block_ads.set(self.settings.block_trackers);
        let p = self.sh.proxy.clone();
        platform::set_blocking(self.settings.block_trackers, shields::rules_json, move || {
            let _ = p.send_event(UserEvent::BlockerReady);
        });
        self.tabs.iter().filter_map(|t| t.view.as_ref()).for_each(platform::apply_blocker);
        self.enforce_live_cap();
        self.apply_proxy();
        match (self.settings.agent_enabled, self.agent.is_some()) {
            (true, false) => match AgentServer::start(self.sh.proxy.clone(), &self.dir) {
                Ok(s) => self.agent = Some(s),
                Err(e) => eprintln!("rzen: agent API failed to start: {e}"),
            },
            (false, true) => self.agent = None,
            _ => {}
        }
    }

    fn save_settings(&mut self) {
        let path = self.dir.join("settings.json");
        model::save_json(&path, &self.settings, true);
        self.settings_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    }

    fn push_settings(&self) {
        if !self.tabs.iter().any(|t| t.is_internal() && t.view.is_some()) {
            return;
        }
        let state = json!({
            "settings": self.settings,
            "engines": SEARCH_ENGINES.iter().map(|(id, name, _)| json!({"id": id, "name": name})).collect::<Vec<_>>(),
            "isDefault": platform::is_default_browser(),
            "passkeys": platform::passkeys_available(),
            "live": self.tabs.iter().filter(|t| t.view.is_some()).count(),
            "total": self.tabs.len(),
            "history": self.history.len(),
            "downloads": dirs::download_dir().map(|d| d.to_string_lossy().into_owned()),
            "version": env!("CARGO_PKG_VERSION"),
            "engine": wry::webview_version().unwrap_or_default(),
            "platform": std::env::consts::OS,
            "aiProviders": AI_PROVIDERS.iter().map(|p| json!({"id": p.0, "name": p.1})).collect::<Vec<_>>(),
            "aiModels": AI_MODELS.iter().map(|m| json!({"id": m.0, "name": m.1})).collect::<Vec<_>>(),
            "aiKey": platform::get_secret(&self.profile.id).is_some(),
            "agent": { "enabled": self.agent.is_some(), "command": self.mcp_command() },
            "shields": platform::blocker_status(),
            "boosts": self.boosts,
            "currentHost": self.tabs.iter().filter(|t| !t.is_internal()).max_by_key(|t| t.last_active).and_then(|t| crate::boosts::clean_host(&t.d.url)),
            "dnsProviders": features::DNS_PROVIDERS.iter().map(|d| json!({"id": d.0, "name": d.1, "doh": d.2, "ips": d.3})).collect::<Vec<_>>(),
            "profile": profiles::Registry::load().get(&self.profile.id).map(|p| json!({ "name": p.name, "hasPassword": p.password.is_some() })),
        });
        self.internal_js(&format!("window.__settings&&__settings.load({state})"));
    }

    fn on_settings(&mut self, m: SettingsMsg) {
        match m {
            SettingsMsg::Get => {
                if let Some(section) = self.settings_focus.take() {
                    self.focus_settings(&section);
                }
                if let Some(host) = self.boost_intent.take() {
                    self.internal_js(&format!("window.__boosts&&__boosts.create({})", json!(host)));
                }
            }
            SettingsMsg::Set { key, value } => {
                let s = &mut self.settings;
                match (key.as_str(), &value) {
                    ("search_engine", Value::String(v)) => s.search_engine = v.clone(),
                    ("startup", Value::String(v)) if v == "restore" || v == "fresh" => s.startup = v.clone(),
                    ("theme", Value::String(v)) if ["system", "light", "dark"].contains(&v.as_str()) => s.theme = v.clone(),
                    ("block_trackers", Value::Bool(v)) => s.block_trackers = *v,
                    ("unload_after_minutes", v) => s.unload_after_minutes = v.as_u64().unwrap_or(s.unload_after_minutes),
                    ("max_live_tabs", v) => s.max_live_tabs = v.as_u64().map_or(s.max_live_tabs, |n| (n as usize).clamp(2, 64)),
                    ("ai_provider", Value::String(v)) if AI_PROVIDERS.iter().any(|p| p.0 == v) => s.ai_provider = v.clone(),
                    ("ai_model", Value::String(v)) if AI_MODELS.iter().any(|m| m.0 == v) => s.ai_model = v.clone(),
                    ("agent_enabled", Value::Bool(v)) => s.agent_enabled = *v,
                    ("proxy", Value::String(v)) if v.is_empty() || util::parse_proxy(v).is_some() => s.proxy = v.trim().to_string(),
                    ("mini_player", Value::Bool(v)) => s.mini_player = *v,
                    ("memory_limit_mb", v) => s.memory_limit_mb = v.as_u64().map_or(s.memory_limit_mb, |n| if n == 0 { 0 } else { n.max(512) }),
                    _ => return eprintln!("rzen: unknown setting {key}={value}"),
                }
                self.save_settings();
                self.apply_settings();
                self.sync_mini();
                self.enforce_memory_limit();
                self.ui_dirty = true;
            }
            SettingsMsg::ClearHistory => {
                self.history.clear();
                self.set_toast("History cleared", 3);
            }
            SettingsMsg::ClearData => {
                let _ = self.chrome.clear_all_browsing_data();
                let isolated: Vec<Id> = self.workspaces.iter().filter(|w| w.isolated).map(|w| w.id).collect();
                for t in &self.tabs {
                    if let Some(v) = t.view.as_ref().filter(|_| isolated.contains(&t.d.workspace)) {
                        let _ = v.clear_all_browsing_data();
                    }
                }
                self.set_toast("Cookies and site data cleared", 3);
            }
            SettingsMsg::MakeDefault => {
                let p = self.sh.proxy.clone();
                if !platform::make_default_browser(move || {
                    let _ = p.send_event(UserEvent::DefaultBrowserChanged);
                }) {
                    self.set_toast("Run rzen.app (scripts/bundle-macos.sh) to set it as default", 5);
                }
            }
            SettingsMsg::OpenDownloads => self.open_downloads(),
            SettingsMsg::ProfileName { name } => {
                let mut reg = profiles::Registry::load();
                if let Some(p) = reg.get_mut(&self.profile.id).filter(|_| !name.trim().is_empty()) {
                    p.name = name.trim().chars().take(40).collect();
                    reg.save();
                }
                self.ui_dirty = true;
            }
            SettingsMsg::SpeedTest => self.speed_test(),
            SettingsMsg::FlushDns => self.flush_dns(),
            SettingsMsg::ClearCache => {
                let p = self.sh.proxy.clone();
                platform::clear_cache(profiles::store_id(&self.profile.id, None), move || {
                    let _ = p.send_event(UserEvent::NetDone("Page cache cleared".into()));
                });
            }
            SettingsMsg::InstallDns { provider, custom } => self.install_dns(&provider, custom.as_deref()),
            SettingsMsg::SetSystemDns { provider } => self.set_system_dns(&provider),
            SettingsMsg::RefreshFilters => platform::refresh_filters(true),
            SettingsMsg::BoostSave { mut boost } => {
                let Some(host) = crate::boosts::clean_host(&boost.host) else { return self.set_toast("Enter a site, like youtube.com", 3) };
                boost.host = host;
                match self.boosts.iter_mut().find(|b| b.id == boost.id) {
                    Some(b) => *b = boost,
                    None => self.boosts.push(boost),
                }
                self.boosts_changed();
            }
            SettingsMsg::BoostDelete { id } => {
                self.boosts.retain(|b| b.id != id);
                self.boosts_changed();
            }
            SettingsMsg::BoostReload { id } => self.reload_boosted(id),
            SettingsMsg::BoostOpen { id } => self.open_boosted(id),
            SettingsMsg::NetInfo => self.net_info(),
            SettingsMsg::DnsBench { servers } => {
                let p = self.sh.proxy.clone();
                for server in servers.into_iter().take(16) {
                    let p = p.clone();
                    std::thread::spawn(move || {
                        let b = crate::dns::bench(&server);
                        let _ = p.send_event(UserEvent::DnsResult(json!({ "bench": b })));
                    });
                }
            }
            SettingsMsg::DnsLookup { name, server, kind } => {
                let p = self.sh.proxy.clone();
                std::thread::spawn(move || {
                    let qtype = match kind.as_str() {
                        "AAAA" => crate::dns::TYPE_AAAA,
                        "CNAME" => crate::dns::TYPE_CNAME,
                        _ => crate::dns::TYPE_A,
                    };
                    let out = match crate::dns::query(&server, name.trim(), qtype, std::time::Duration::from_secs(3)) {
                        Ok((rtt, msg)) => match crate::dns::parse_answers(&msg) {
                            Ok(records) => json!({ "lookup": { "name": name, "server": server, "ms": rtt.as_secs_f64() * 1000.0, "records": records } }),
                            Err(e) => json!({ "lookup": { "name": name, "server": server, "error": e } }),
                        },
                        Err(e) => json!({ "lookup": { "name": name, "server": server, "error": e } }),
                    };
                    let _ = p.send_event(UserEvent::DnsResult(out));
                });
            }
            SettingsMsg::OpenPage { url } if internal(&url).is_some() => self.open_internal(&url),
            SettingsMsg::OpenPage { .. } => {}
            SettingsMsg::OpenVpnSettings => util::open_with_os("x-apple.systempreferences:com.apple.Network-Settings.extension"),
            SettingsMsg::AiKeyRemove => {
                platform::set_secret(&self.profile.id, "");
                self.js("__rzen.aiKey(null)");
                self.set_toast("API key removed from the Keychain", 3);
            }
            SettingsMsg::ProfilePassword { password } => {
                let mut reg = profiles::Registry::load();
                if let Some(p) = reg.get_mut(&self.profile.id) {
                    p.password = (!password.is_empty()).then(|| profiles::hash_password(&password));
                    reg.save();
                }
                self.set_toast(if password.is_empty() { "Password removed" } else { "Password set" }, 3);
            }
            SettingsMsg::Unload { id } => {
                if self.visible != Some(id) {
                    self.unload(id);
                }
                self.perf_next = Instant::now();
            }
        }
        self.push_settings();
    }

    fn open_downloads(&self) {
        if let Some(d) = dirs::download_dir() {
            util::open_with_os(&d.to_string_lossy());
        }
    }

    /// Picks up hand edits to settings.json when the window regains focus.
    pub fn reload_settings(&mut self) {
        let path = self.dir.join("settings.json");
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if mtime != self.settings_mtime {
            self.settings_mtime = mtime;
            self.settings = model::load_json(&path);
            self.apply_settings();
            self.push_settings();
        }
    }

    // ---------- context menus ----------

    fn tab_menu(&self, id: Id, x: f64, y: f64) {
        let Some(t) = self.tab(id) else { return };
        let it = |a: &str, label: &str, enabled: bool| MenuItem::with_id(format!("tab.{a}:{id}"), label, enabled, None);
        let sep = PredefinedMenuItem::separator();
        let move_to = Submenu::new("Move to Space", true);
        for w in self.workspaces.iter().filter(|w| t.d.essential || w.id != t.d.workspace) {
            let _ = move_to.append(&MenuItem::with_id(format!("tab.move:{id}:{}", w.id), &w.name, true, None));
        }
        let m = Menu::new();
        let _ = m.append_items(&[
            &it("reload", "Reload", t.view.is_some()),
            &it("duplicate", "Duplicate Tab", !t.is_internal()),
            &it("copy", "Copy Link", !t.is_internal()),
            &sep,
            &it("pin", if t.d.pinned { "Unpin Tab" } else { "Pin Tab" }, !t.d.essential),
            &it("essential", if t.d.essential { "Remove from Essentials" } else { "Add to Essentials" }, true),
            &move_to,
            &sep,
            &it("unload", "Unload Tab", t.view.is_some() && self.visible != Some(id)),
            &sep,
            &it("close", "Close Tab", true),
            &it("close_others", "Close Other Tabs", true),
        ]);
        platform::show_menu(&self.sh.window, &m, x, y);
    }

    fn ws_menu(&self, id: Id, x: f64, y: f64) {
        let Some(w) = self.workspaces.iter().find(|w| w.id == id) else { return };
        let m = Menu::new();
        let sep = PredefinedMenuItem::separator();
        let _ = m.append_items(&[
            &MenuItem::with_id(format!("ws.rename:{id}"), "Rename…", true, None),
            &CheckMenuItem::with_id(format!("ws.isolate:{id}"), "Separate Cookies and Logins", true, w.isolated, None),
            &sep,
            &MenuItem::with_id(format!("ws.delete:{id}"), "Delete Space", self.workspaces.len() > 1, None),
        ]);
        platform::show_menu(&self.sh.window, &m, x, y);
    }

    // ---------- event handling ----------

    pub fn on_event(&mut self, ev: UserEvent) {
        match ev {
            UserEvent::Chrome(s) => match serde_json::from_str::<Msg>(&s) {
                Ok(m) => self.on_msg(m),
                Err(e) => eprintln!("rzen: bad chrome message {s}: {e}"),
            },
            UserEvent::Settings(s) => match serde_json::from_str::<SettingsMsg>(&s) {
                Ok(m) => self.on_settings(m),
                Err(e) => eprintln!("rzen: bad settings message {s}: {e}"),
            },
            UserEvent::Title(id, title) => {
                let Some(i) = self.idx(id) else { return };
                let url = self.tabs[i].current_url();
                let t = &mut self.tabs[i];
                t.d.url = url;
                if !title.is_empty() {
                    t.d.title = title;
                }
                self.history.set_title(&t.d.url, &t.d.title);
                // Fetch the icon as soon as the document has a title, not only on "finished"
                // (which never arrives for pages WebKit suspends in the background).
                let host = util::short_host(&t.d.url).to_string();
                if t.d.favicon.is_none() || t.icon_host != host {
                    t.icon_host = host;
                    self.fetch_favicon(id);
                }
                if self.visible == Some(id) {
                    self.sh.window.set_title(&self.tabs[i].d.title);
                }
                self.dirty();
            }
            UserEvent::Load(id, finished, url) => {
                if debug() {
                    eprintln!("load {id} finished={finished} {url}");
                }
                let Some(i) = self.idx(id) else { return };
                let t = &mut self.tabs[i];
                t.loading = !finished;
                if !url.is_empty() && url != "about:blank" {
                    t.d.url = url;
                }
                if let Some(v) = &t.view {
                    t.can_back = v.can_go_back().unwrap_or(false);
                    t.can_fwd = v.can_go_forward().unwrap_or(false);
                }
                if finished {
                    self.history.visit(&t.d.url, &t.d.title);
                    self.fetch_favicon(id);
                }
                self.dirty();
            }
            UserEvent::MiniFocus(id) => self.show(id),
            UserEvent::Speed(result) => {
                let v = match result {
                    Ok(v) => v,
                    Err(e) => json!({ "error": e }),
                };
                self.internal_js(&format!("window.__settings&&__settings.speed&&__settings.speed({v})"));
            }
            UserEvent::NetDone(msg) => {
                self.set_toast(msg, 4);
                self.net_info();
            }
            UserEvent::NetInfo(info) => {
                self.internal_js(&format!("window.__settings&&__settings.net&&__settings.net({info})"));
            }
            UserEvent::BlockerStatus => self.push_settings(),
            UserEvent::DnsResult(v) => {
                self.internal_js(&format!("window.__dns&&__dns.result({v})"));
            }
            UserEvent::MediaTime(id, t) => {
                if self.media_tab == Some(id) {
                    self.js(&format!("__rzen.mediaTime({t})"));
                }
            }
            UserEvent::IconCached => {
                self.palette_history = None;
                self.ui_dirty = true;
            }
            UserEvent::Favicon(id, href) => {
                // The shared icon cache would reveal where a private window went.
                if !self.sh.private {
                    let p = self.sh.proxy.clone();
                    crate::favicons::ensure(&href, move || {
                        let _ = p.send_event(UserEvent::IconCached);
                    });
                }
                if let Some(t) = self.tab(id) {
                    let url = t.d.url.clone();
                    self.history.set_icon(&url, &href);
                }
                if let Some(i) = self.idx(id).filter(|_| !href.is_empty()) {
                    self.tabs[i].d.favicon = Some(href);
                    self.dirty();
                }
            }
            UserEvent::OpenTab(url, opener) => {
                self.open_tab(url, opener, true);
            }
            UserEvent::AdoptPopups => {
                let popups = std::mem::take(&mut *self.sh.popups.borrow_mut());
                for p in popups {
                    let workspace = self
                        .tab(p.opener)
                        .map(|t| if t.d.essential { self.active_ws } else { t.d.workspace })
                        .unwrap_or(self.active_ws);
                    let pos = self.idx(p.opener).map_or(self.tabs.len(), |i| i + 1);
                    let mut tab = Tab::new(TabData { id: p.id, url: p.url, workspace, ..Default::default() });
                    tab.view = Some(p.view);
                    tab.loading = true;
                    self.tabs.insert(pos, tab);
                    self.show(p.id);
                }
            }
            UserEvent::Crashed(id) => {
                let was_visible = self.visible == Some(id);
                self.unload(id);
                if was_visible {
                    self.show(id);
                    self.set_toast("Page reloaded after its process stopped", 4);
                }
            }
            UserEvent::DownloadStarted(name) => self.set_toast(format!("Downloading {name}…"), 4),
            UserEvent::DownloadDone(ok) => {
                self.set_toast(if ok { "Download finished. Click to open." } else { "Download failed" }, 6)
            }
            UserEvent::FindResult(found) => self.js(&format!("__rzen.findResult({found})")),
            UserEvent::Menu(id) => self.on_menu(&id),
            UserEvent::BlockerReady => {
                self.tabs.iter().filter_map(|t| t.view.as_ref()).for_each(platform::apply_blocker);
            }
            UserEvent::DefaultBrowserChanged => self.push_settings(),
            UserEvent::Agent(call) => self.on_agent(call),
            UserEvent::MediaInfo(id, info) => {
                if let Some(i) = self.idx(id) {
                    self.tabs[i].media = Some(info);
                    self.ui_dirty = true;
                    self.sync_mini();
                }
            }
            UserEvent::AiAsk(selection) => self.ai_ask(&selection),
            UserEvent::ToolResult(call, result) => {
                let (ok, value) = match result {
                    Ok(v) => (true, v),
                    Err(e) => (false, Value::String(e)),
                };
                self.js(&format!("__rzen.toolResult({},{ok},{})", json!(call), value));
            }
        }
    }

    fn on_msg(&mut self, m: Msg) {
        let allowed_while_gated = matches!(
            m,
            Msg::Ready | Msg::Unlock { .. } | Msg::Setup { .. } | Msg::NewProfile { .. } | Msg::ProfileMenu { .. } | Msg::Drag | Msg::TitlebarDblClick | Msg::PaletteClose
        );
        if self.gate.is_some() && !allowed_while_gated {
            return;
        }
        match m {
            Msg::Ready => {
                self.chrome_ready = true;
                self.palette_history = None;
                self.ui_dirty = true;
                if std::mem::take(&mut self.palette_on_ready) {
                    self.open_palette("new");
                }
            }
            Msg::Select { id } => {
                if self.peek {
                    self.close_palette();
                }
                self.show(id);
            }
            Msg::Close { id } => self.close(id),
            Msg::NewTab => self.open_palette("new"),
            Msg::EditUrl => self.open_palette("edit"),
            Msg::Back => {
                if let Some(v) = self.active_view() {
                    let _ = v.go_back();
                }
            }
            Msg::Forward => {
                if let Some(v) = self.active_view() {
                    let _ = v.go_forward();
                }
            }
            Msg::Reload => {
                if let Some(v) = self.active_view() {
                    let _ = v.reload();
                }
            }
            Msg::PaletteClose => self.close_palette(),
            Msg::Go { value, mode, tab, search } => {
                self.close_palette();
                match tab {
                    Some(id) => self.show(id),
                    None => {
                        let engine = self.settings.search_template();
                        let url = if search { util::search_url(&value, engine) } else { util::resolve_input(&value, engine) };
                        if internal(&url).is_some() {
                            self.open_internal(&url);
                        } else {
                            match self.active_id().filter(|_| mode == "edit") {
                                Some(id) => self.navigate(id, url),
                                None => {
                                    self.open_tab(url, None, true);
                                }
                            }
                        }
                    }
                }
                self.focus_content();
            }
            Msg::Ws { id } => self.switch_ws(id),
            Msg::WsNew => self.new_workspace(),
            Msg::WsRename { id, name } => {
                let name = name.trim();
                if let Some(w) = self.workspaces.iter_mut().find(|w| w.id == id).filter(|_| !name.is_empty()) {
                    w.name = name.chars().take(40).collect();
                }
                self.dirty();
            }
            Msg::TabCtx { id, x, y } => self.tab_menu(id, x, y),
            Msg::WsCtx { id, x, y } => self.ws_menu(id, x, y),
            Msg::Move { id, before, section } => self.move_tab(id, before, &section),
            Msg::MoveWs { id, ws } => self.move_to_ws(id, ws),
            Msg::Sidebar => {
                self.sidebar_forced = false;
                self.set_sidebar(!self.sidebar_open);
            }
            Msg::SidebarW { w } => {
                // Whole points: fractional widths blur the sidebar's icons.
                self.sidebar_w = w.clamp(180.0, 420.0).round();
                self.layout();
                self.dirty();
            }
            // Mid-animation the page keeps its full (compact) width and only moves: moving a
            // view is free for WebKit, resizing forces a reflow. So it reflows once — before
            // sliding when the sidebar closes, after `SidebarDone` when it opens — never per frame.
            Msg::ContentX { x } => {
                let (w, h) = self.win_size();
                let r = rect(x, GAP, w - 2.0 * GAP - self.ai_extra(), h - 2.0 * GAP);
                if let Some(v) = self.attached_view() {
                    let _ = v.set_bounds(r);
                }
            }
            Msg::SidebarDone => self.layout(),
            Msg::Peek { on } => {
                if self.sidebar_open || self.peek == on {
                    return;
                }
                self.peek = on;
                platform::set_traffic_lights(&self.sh.window, on);
                self.set_overlay(on);
                self.ui_dirty = true;
            }
            Msg::Drag => {
                let _ = self.sh.window.drag_window();
            }
            Msg::TitlebarDblClick => {
                let w = &self.sh.window;
                w.set_maximized(!w.is_maximized());
            }
            Msg::Find { q, back, again } => self.find(&q, back, again),
            Msg::FindClose => {
                self.finding = false;
                self.release_sidebar();
                self.focus_content();
                self.ui_dirty = true;
            }
            Msg::OpenDownloads => self.open_downloads(),
            Msg::Ai => self.set_ai(!self.ai_open),
            Msg::AiW { w } => {
                self.ai_w = w.clamp(300.0, 720.0);
                self.layout();
                self.dirty();
            }
            Msg::AiProvider { id } => {
                if AI_PROVIDERS.iter().any(|p| p.0 == id) {
                    self.settings.ai_provider = id;
                    self.save_settings();
                    // Rebuild: web providers get a page, rzen AI renders in the chrome.
                    if let Some(v) = self.ai_view.take() {
                        platform::release(&v);
                    }
                    if self.ai_open {
                        self.ensure_ai();
                        self.layout();
                    }
                    self.ui_dirty = true;
                }
            }
            Msg::AiAskPage => {
                let p = self.sh.proxy.clone();
                match self.active_view() {
                    Some(v) => {
                        let _ = v.evaluate_script_with_callback("String(getSelection()||'').slice(0,4000)", move |r| {
                            let _ = p.send_event(UserEvent::AiAsk(serde_json::from_str(&r).unwrap_or_default()));
                        });
                    }
                    None => self.set_toast("Open a page first", 2),
                }
            }
            Msg::MediaToggle => self.media_js(MEDIA_TOGGLE_JS),
            Msg::MediaStep { next } => self.media_js(&format!("{MEDIA_STEP_JS}({next})")),
            Msg::MediaMute { id } => {
                if let Some(id) = id.or(self.media_tab) {
                    self.toggle_mute(id);
                }
            }
            Msg::MediaFocus => {
                if let Some(id) = self.media_tab {
                    self.show(id);
                }
            }
            Msg::MediaClose => {
                if let Some(v) = self.media_tab.and_then(|id| self.tab(id)).and_then(|t| t.view.as_ref()) {
                    let _ = v.evaluate_script("document.querySelectorAll('video,audio').forEach(m=>m.pause())");
                }
                self.media_tab = None;
                self.ui_dirty = true;
            }
            Msg::ProfileMenu { x, y } => self.profile_menu(x, y),
            Msg::AiMenu { x, y } => self.ai_menu(x, y),
            Msg::UrlMenu { x, y, r } => {
                self.share_anchor = r;
                self.url_menu(x, y);
            }
            Msg::Share { r } => self.share_page(r),
            Msg::OpenSettings => self.open_settings(),
            Msg::MiniRect { r } => {
                self.mini_rect = r;
                self.sync_mini();
            }
            Msg::Command { id } => {
                self.close_palette();
                self.run_command(&id);
            }
            Msg::MediaSeek { frac } => self.media_js(&format!("{MEDIA_SEEK_JS}({})", frac.clamp(0.0, 1.0))),
            Msg::CopyUrl => {
                if let Some(url) = self.active_id().and_then(|i| self.tab(i)).map(Tab::current_url) {
                    platform::copy_text(&url);
                }
            }
            Msg::AiKeyGet => {
                let key = platform::get_secret(&self.profile.id);
                self.js(&format!("__rzen.aiKey({})", json!(key)));
            }
            Msg::AiKeySet { key } => {
                let key = key.trim();
                let ok = platform::set_secret(&self.profile.id, key);
                self.js(&format!("__rzen.aiKey({})", json!(if ok && !key.is_empty() { Some(key) } else { None })));
                if !ok {
                    self.set_toast("Couldn't save the key to the Keychain", 4);
                }
            }
            Msg::Tool { call, method, params } => self.assistant_tool(call, method, params),
            Msg::Unlock { password } => {
                let reg = profiles::Registry::load();
                let ok = reg.get(&self.profile.id).is_some_and(|p| p.password.as_deref().is_none_or(|h| profiles::verify_password(&password, h)));
                if ok {
                    self.gate = None;
                    self.enter();
                } else {
                    self.js("__rzen.gateError('Wrong password')");
                }
            }
            Msg::Setup { name, password } => {
                let name = name.trim();
                if name.is_empty() {
                    return self.js("__rzen.gateError('Enter your name')");
                }
                let mut reg = profiles::Registry::load();
                if let Some(p) = reg.get_mut(&self.profile.id) {
                    p.name = name.chars().take(40).collect();
                    p.password = (!password.is_empty()).then(|| profiles::hash_password(&password));
                    reg.save();
                }
                self.gate = None;
                self.palette_on_ready = true;
                self.enter();
            }
            Msg::NewProfile { name, password } => {
                if name.trim().is_empty() {
                    return self.js("__rzen.gateError('Enter a name')");
                }
                let mut reg = profiles::Registry::load();
                let id = reg.create(&name, Some(&password));
                self.switch_profile(&id);
            }
        }
    }

    fn on_menu(&mut self, id: &str) {
        if self.gate.is_some() && !id.starts_with("profile.switch:") && id != "new_profile" {
            return;
        }
        if let Some(rest) = id.strip_prefix("tab.") {
            let mut parts = rest.split(':');
            let action = parts.next().unwrap_or("");
            let Some(tid) = parts.next().and_then(|s| s.parse::<Id>().ok()) else { return };
            match action {
                "reload" => {
                    if let Some(v) = self.tab(tid).and_then(|t| t.view.as_ref()) {
                        let _ = v.reload();
                    }
                }
                "duplicate" => {
                    if let Some(url) = self.tab(tid).map(Tab::current_url) {
                        self.open_tab(url, Some(tid), true);
                    }
                }
                "copy" => {
                    if let Some(url) = self.tab(tid).map(Tab::current_url) {
                        platform::copy_text(&url);
                        self.set_toast("Link copied", 2);
                    }
                }
                "pin" => {
                    if let Some(i) = self.idx(tid) {
                        self.tabs[i].d.pinned = !self.tabs[i].d.pinned;
                        self.dirty();
                    }
                }
                "essential" => {
                    if let Some(t) = self.tab(tid) {
                        let section = if t.d.essential { "tabs" } else { "essential" };
                        self.move_tab(tid, None, section);
                    }
                }
                "move" => {
                    if let Some(ws) = parts.next().and_then(|s| s.parse::<Id>().ok()) {
                        self.move_to_ws(tid, ws);
                    }
                }
                "unload" => self.unload(tid),
                "close" => self.close(tid),
                "close_others" => {
                    let others: Vec<Id> = self
                        .tabs
                        .iter()
                        .filter(|t| t.d.id != tid && !t.d.pinned && !t.d.essential && t.d.workspace == self.active_ws)
                        .map(|t| t.d.id)
                        .collect();
                    others.into_iter().for_each(|o| self.close(o));
                    self.show(tid);
                }
                _ => {}
            }
            return;
        }
        match id {
            "url.copy_md" => {
                if let Some(t) = self.active_id().and_then(|i| self.tab(i)) {
                    let title = if t.d.title.is_empty() { util::short_host(&t.d.url).to_string() } else { t.d.title.replace(['[', ']'], "") };
                    platform::copy_text(&format!("[{title}]({})", t.current_url()));
                    self.set_toast("Copied as Markdown", 2);
                }
                return;
            }
            "url.share" => return self.share_page(self.share_anchor),
            "url.safari" => {
                if let Some(url) = self.active_id().and_then(|i| self.tab(i)).map(Tab::current_url) {
                    #[cfg(target_os = "macos")]
                    let _ = std::process::Command::new("open").args(["-a", "Safari", &url]).spawn();
                    #[cfg(not(target_os = "macos"))]
                    util::open_with_os(&url);
                }
                return;
            }
            "ai.ask" => return self.on_msg(Msg::AiAskPage),
            _ => {}
        }
        if let Some(pid) = id.strip_prefix("ai.provider:") {
            return self.on_msg(Msg::AiProvider { id: pid.to_string() });
        }
        if let Some(pid) = id.strip_prefix("profile.switch:") {
            if pid != self.profile.id {
                self.switch_profile(&pid.to_string());
            }
            return;
        }
        if let Some(rest) = id.strip_prefix("ws.") {
            let Some((action, wid)) = rest.split_once(':') else { return };
            let Ok(wid) = wid.parse::<Id>() else { return };
            match action {
                "rename" => self.js(&format!("__rzen.renameWs({wid})")),
                "isolate" => self.toggle_isolation(wid),
                "delete" => self.delete_ws(wid),
                _ => {}
            }
            return;
        }
        match id {
            "new_tab" => self.open_palette("new"),
            "open_location" => self.open_palette("edit"),
            "tab_switcher" => self.open_palette("tabs"),
            "command_palette" => self.open_palette("commands"),
            "new_ws" => self.new_workspace(),
            "private_window" => self.open_private_window(),
            "reopen" => self.reopen_closed(),
            "close_tab" => self.close_or_unload_active(),
            "print" => {
                if let Some(v) = self.active_view() {
                    let _ = v.print();
                }
            }
            "copy_url" => {
                if let Some(url) = self.active_id().and_then(|i| self.tab(i)).map(Tab::current_url) {
                    platform::copy_text(&url);
                    self.set_toast("Link copied", 2);
                }
            }
            "find" => self.open_find(),
            "find_next" | "find_prev" => {
                if self.finding {
                    self.js(&format!("__rzen.findStep({})", id == "find_prev"));
                } else {
                    self.open_find();
                }
            }
            "toggle_sidebar" => {
                self.sidebar_forced = false;
                self.set_sidebar(!self.sidebar_open);
            }
            "reload" => {
                if let Some(v) = self.active_view() {
                    let _ = v.reload();
                }
            }
            "stop" => {
                if let Some(v) = self.active_view() {
                    let _ = v.evaluate_script("window.stop()");
                }
            }
            "zoom_in" => self.zoom(1),
            "zoom_out" => self.zoom(-1),
            "zoom_reset" => self.zoom(0),
            "devtools" => {
                if let Some(v) = self.active_view() {
                    v.open_devtools();
                }
            }
            "back" => {
                if let Some(v) = self.active_view() {
                    let _ = v.go_back();
                }
            }
            "forward" => {
                if let Some(v) = self.active_view() {
                    let _ = v.go_forward();
                }
            }
            "next_tab" => self.cycle_tab(1),
            "prev_tab" => self.cycle_tab(-1),
            "next_ws" => self.cycle_ws(1),
            "prev_ws" => self.cycle_ws(-1),
            "pin_tab" => {
                if let Some(a) = self.active_id() {
                    self.on_menu(&format!("tab.pin:{a}"));
                }
            }
            "unload_tab" => self.close_or_unload_active(),
            "tab_last" => {
                if let Some(&last) = self.display_order().last() {
                    self.show(last);
                }
            }
            "settings" => self.open_settings(),
            "toggle_ai" => self.set_ai(!self.ai_open),
            "lock" => self.lock(),
            "new_profile" => {
                self.js("__rzen.newProfile()");
                self.set_overlay(true);
            }
            other => {
                if let Some(n) = other.strip_prefix("tab_").and_then(|n| n.parse::<usize>().ok()) {
                    if let Some(&id) = self.display_order().get(n - 1) {
                        self.show(id);
                    }
                }
            }
        }
    }

    /// Opens many URLs cheaply: each becomes a tab, but only the last one gets a web
    /// process; the rest load when first selected. `eager` loads all (benchmarks).
    pub fn open_urls(&mut self, urls: impl IntoIterator<Item = String>, eager: bool) {
        let mut last = None;
        for url in urls {
            last = Some(self.open_tab(url, None, eager));
        }
        if let Some(id) = last {
            self.palette_on_ready = false;
            self.show(id);
        }
    }

    // ---------- per-turn flush ----------

    fn push_state(&self) {
        #[derive(Serialize)]
        struct UiTab<'a> {
            id: Id,
            title: &'a str,
            url: &'a str,
            icon: Option<String>,
            pinned: bool,
            essential: bool,
            live: bool,
            loading: bool,
            audible: bool,
            muted: bool,
        }
        // A locked profile shows nothing about its tabs.
        let order = if self.gate.is_some() { Vec::new() } else { self.display_order() };
        let tabs: Vec<UiTab> = order
            .iter()
            .filter_map(|&id| self.tab(id))
            .map(|t| UiTab {
                id: t.d.id,
                title: &t.d.title,
                url: &t.d.url,
                icon: t.d.favicon.as_deref().map(crate::favicons::local),
                pinned: t.d.pinned,
                essential: t.d.essential,
                live: t.view.is_some(),
                loading: t.loading,
                audible: t.audible,
                muted: t.muted,
            })
            .collect();
        let active = self.active_id();
        let cur = active.and_then(|a| self.tab(a));
        let (profile, profiles) = self.profile_json();
        let state = json!({
            "ws": self.workspaces.iter().map(|w| json!({"id": w.id, "name": w.name, "isolated": w.isolated})).collect::<Vec<_>>(),
            "activeWs": self.active_ws,
            "tabs": tabs,
            "active": active,
            "url": cur.map_or("", |t| t.d.url.as_str()),
            "canBack": cur.is_some_and(|t| t.can_back),
            "canFwd": cur.is_some_and(|t| t.can_fwd),
            "loading": cur.is_some_and(|t| t.loading),
            "sidebarOpen": self.sidebar_open,
            "peek": self.peek,
            "sw": self.sidebar_w,
            "gap": GAP,
            "find": self.finding,
            "toast": self.toast.as_ref().map(|t| t.0.as_str()),
            "gate": self.gate,
            "profile": profile,
            "profiles": profiles,
            "ai": {
                "open": self.ai_open && self.gate.is_none(), "w": self.ai_width(), "provider": self.settings.ai_provider,
                "providerName": AI_PROVIDERS.iter().find(|p| p.0 == self.settings.ai_provider).map_or("rzen AI", |p| p.1),
                "model": self.settings.ai_model,
            },
            "tab": cur.map(|t| json!({ "title": t.d.title, "url": t.d.url })),
            "media": self.media_json(),
            "agent": self.agent_seen.is_some(),
            "private": self.sh.private,
        });
        self.js(&format!("__rzen.state({state})"));
    }

    fn save_session(&mut self) {
        self.save_at = None;
        let tabs = self.tabs.iter().map(|t| TabData { url: t.current_url(), ..t.d.clone() }).collect();
        let w = &self.sh.window;
        let scale = w.scale_factor();
        let window = w.outer_position().ok().filter(|_| !w.is_minimized()).map(|p| {
            let p = p.to_logical::<f64>(scale);
            let s = w.inner_size().to_logical::<f64>(scale);
            WindowGeom { x: p.x, y: p.y, w: s.width, h: s.height }
        });
        let session = Session {
            workspaces: self.workspaces.clone(),
            tabs,
            active_ws: self.active_ws,
            closed: self.closed.clone(),
            next_id: self.sh.next_id.get(),
            sidebar_open: self.sidebar_open && !self.sidebar_forced,
            sidebar_w: self.sidebar_w,
            window,
            ai_open: self.ai_open,
            ai_w: self.ai_w,
        };
        model::save_json(&self.dir.join("session.json"), &session, false);
    }

    /// Mark window geometry as changed.
    pub fn touch(&mut self) {
        self.save_at.get_or_insert(Instant::now() + SAVE_DELAY);
    }

    /// Under macOS memory pressure, drop every background page at once (they reload on
    /// return) and keep a tighter cap until pressure clears.
    fn react_to_pressure(&mut self) {
        let level = perf::pressure();
        if level >= perf::PRESSURE_WARN && self.pressure < perf::PRESSURE_WARN {
            let ids: Vec<Id> = self.tabs.iter().filter(|t| t.view.is_some() && Some(t.d.id) != self.visible).map(|t| t.d.id).collect();
            ids.iter().for_each(|&id| self.request_unload(id));
            if !ids.is_empty() {
                self.set_toast(format!("Memory is tight, unloading {} background tabs", ids.len()), 4);
            }
        }
        self.pressure = level;
    }

    fn settings_visible(&self) -> bool {
        self.visible.and_then(|v| self.tab(v)).is_some_and(Tab::is_settings)
    }

    /// Samples memory/CPU of the browser and its WebKit processes; feeds the Settings
    /// task manager and the CSV log.
    fn perf_tick(&mut self) {
        let now = Instant::now();
        let wall = now.duration_since(self.perf_at).as_nanos().max(1) as f64;
        self.perf_at = now;
        let mut procs = perf::processes();
        if !perf::exact() {
            // Launched from a terminal: helpers are attributed to it, so only count page
            // processes that back our own tabs.
            let ours: std::collections::HashSet<i32> = self.tabs.iter().filter_map(|t| platform::web_process_pid(t.view.as_ref()?)).collect();
            procs.retain(|p| p.kind != Kind::Page || ours.contains(&p.pid));
        }
        let cpu = |p: &perf::Proc| self.perf_prev.get(&p.pid).map_or(0.0, |&c| p.cpu_ns.saturating_sub(c) as f64 / wall * 100.0);
        let mb = |b: u64| b as f64 / 1_048_576.0;
        let mut sum = [0u64; 4];
        let mut cpu_total = 0.0;
        let mut pages = 0;
        for p in &procs {
            let k = match p.kind {
                Kind::Browser => 0,
                Kind::Page => {
                    pages += 1;
                    1
                }
                Kind::Gpu => 2,
                Kind::Network => 3,
            };
            sum[k] += p.footprint;
            cpu_total += cpu(p);
        }
        let by_pid: HashMap<i32, &perf::Proc> = procs.iter().map(|p| (p.pid, p)).collect();
        let mut tabs: Vec<(f64, Value)> = self
            .tabs
            .iter()
            .filter_map(|t| {
                let p = by_pid.get(&platform::web_process_pid(t.view.as_ref()?)?)?;
                let title = if t.d.title.is_empty() { util::short_host(&t.d.url).to_string() } else { t.d.title.clone() };
                let m = mb(p.footprint);
                Some((m, json!({ "id": t.d.id, "title": title, "mb": m, "cpu": cpu(p), "visible": self.visible == Some(t.d.id) })))
            })
            .collect();
        tabs.sort_by(|a, b| b.0.total_cmp(&a.0));
        let total: u64 = sum.iter().sum();
        let live = self.tabs.iter().filter(|t| t.view.is_some()).count();
        self.perf_prev = procs.iter().map(|p| (p.pid, p.cpu_ns)).collect();
        if let Some(f) = &mut self.memlog {
            let ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let _ = writeln!(
                f,
                "{ts},{:.1},{:.1},{:.1},{:.1},{:.1},{pages},{live},{},{cpu_total:.1},{}",
                mb(total),
                mb(sum[0]),
                mb(sum[1]),
                mb(sum[2]),
                mb(sum[3]),
                self.tabs.len(),
                self.pressure
            );
        }
        if self.settings_visible() {
            let state = json!({
                "total": mb(total), "browser": mb(sum[0]), "pages": mb(sum[1]), "gpu": mb(sum[2]), "network": mb(sum[3]),
                "pageProcs": pages, "cpu": cpu_total, "live": live, "count": self.tabs.len(), "pressure": self.pressure,
                "tabs": tabs.into_iter().map(|t| t.1).collect::<Vec<_>>(),
            });
            if let Some(v) = self.active_view() {
                let _ = v.evaluate_script(&format!("window.__settings&&__settings.perf&&__settings.perf({state})"));
            }
        }
    }

    fn maintenance(&mut self) {
        self.react_to_pressure();
        self.enforce_memory_limit();
        let minutes = self.settings.unload_after_minutes;
        if minutes > 0 {
            let limit = Duration::from_secs(minutes * 60);
            let idle: Vec<Id> = self
                .tabs
                .iter()
                .filter(|t| t.view.is_some() && Some(t.d.id) != self.visible && !t.d.essential && t.last_active.elapsed() >= limit)
                .map(|t| t.d.id)
                .collect();
            idle.into_iter().for_each(|id| self.request_unload(id));
        }
        self.history.save();
    }

    pub fn flush(&mut self, cf: &mut ControlFlow) {
        if self.quit {
            self.shutdown();
            *cf = ControlFlow::ExitWithCode(0);
            return;
        }
        if self.exiting {
            return;
        }
        let now = Instant::now();
        if self.agent_seen.is_some_and(|t| now.duration_since(t) >= AGENT_BADGE) {
            self.agent_seen = None;
            self.ui_dirty = true;
        }
        let any_live = self.tabs.iter().any(|t| t.view.is_some());
        if any_live && now >= self.media_next {
            self.media_next = now + MEDIA_POLL;
            self.poll_media();
        }
        self.check_waiters(now);
        if self.toast.as_ref().is_some_and(|t| t.1 <= now) {
            self.toast = None;
            self.ui_dirty = true;
        }
        if self.ui_dirty && self.chrome_ready {
            self.ui_dirty = false;
            self.push_state();
        }
        if now >= self.next_tick {
            self.next_tick = now + TICK;
            self.maintenance();
        }
        if self.save_at.is_some_and(|t| now >= t) {
            self.save_session();
        }
        let sampling = self.memlog.is_some() || self.settings_visible();
        if sampling && now >= self.perf_next {
            self.perf_next = now + PERF_EVERY;
            self.perf_tick();
        }
        let perf_wake = sampling.then_some(self.perf_next);
        let media_wake = any_live.then_some(self.media_next);
        let waiter_wake = (!self.waiters.is_empty()).then(|| now + Duration::from_millis(100));
        let badge_wake = self.agent_seen.map(|t| t + AGENT_BADGE);
        let wake = [Some(self.next_tick), self.save_at, self.toast.as_ref().map(|t| t.1), perf_wake, media_wake, waiter_wake, badge_wake]
            .into_iter()
            .flatten()
            .min();
        *cf = ControlFlow::WaitUntil(wake.unwrap_or(now + TICK));
    }

    pub fn shutdown(&mut self) {
        if self.exiting {
            return;
        }
        self.exiting = true;
        self.save_session();
        self.history.save();
        if self.profile.id == PRIVATE_ID {
            // A private window leaves nothing behind: its tabs, history and settings copy go too.
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}
