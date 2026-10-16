//! macOS: WKWebView (Safari's engine) + AppKit niceties.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::c_void,
    ptr::NonNull,
    rc::Rc,
    sync::OnceLock,
};

use block2::RcBlock;
use muda::{ContextMenu, Menu, dpi::LogicalPosition};
use objc2::{AnyThread, ClassType, MainThreadMarker, msg_send, rc::Retained, runtime::NSObjectProtocol, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication, NSBitmapImageFileType, NSBitmapImageRep, NSColor,
    NSSharingServicePicker,
    NSEvent, NSEventMask, NSEventModifierFlags, NSImage, NSPasteboard, NSPasteboardTypeString, NSView, NSWindow, NSWindowButton,
    NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSBundle, NSComparisonResult, NSDictionary, NSError, NSNumber, NSObject, NSPoint, NSProcessInfo, NSRect, NSRectEdge, NSSize,
    NSString, NSURL, NSUUID,
};
use objc2_web_kit::{
    WKContentRuleList, WKContentRuleListStore, WKMediaCaptureState, WKPreferences, WKUserScript, WKUserScriptInjectionTime, WKWebView,
    WKWebViewConfiguration, WKWebsiteDataStore,
};
use tao::{event_loop::EventLoopProxy, platform::macos::WindowExtMacOS, window::Window};
use wry::{
    NewWindowFeatures, NewWindowResponse, WebView, WebViewBuilder, WebViewBuilderExtDarwin, WebViewBuilderExtMacos,
    WebViewExtMacOS,
};

mod web;
pub use web::*;

use crate::{
    browser::{Popup, Shared, UserEvent, build_tab_view},
    model::Id,
};

/// The chrome web view can be raised above page content (command bar, compact-mode peek).
pub const OVERLAY: bool = true;
/// The system share sheet (AirDrop, Messages, Mail, Notes, …) is available.
pub const SHARE: bool = true;
const CONTENT_RADIUS: f64 = 10.0;

thread_local! {
    /// Compiled rule lists in use: the filter lists, or the small built-in list until they're ready.
    static BLOCKER: RefCell<Vec<Retained<WKContentRuleList>>> = const { RefCell::new(Vec::new()) };
    static BLOCKING: Cell<bool> = const { Cell::new(false) };
    /// Filter-list version BLOCKER was built from (None = the built-in list).
    static BLOCKER_VERSION: RefCell<Option<String>> = const { RefCell::new(None) };
    static BLOCKER_READY: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
    static BLOCKER_BUSY: Cell<bool> = const { Cell::new(false) };
    /// Newest compile started; older ones finishing late are dropped.
    static BLOCKER_GEN: Cell<u32> = const { Cell::new(0) };
}

/// Safari's exact UA string, so sites serve their full Safari experience instead of a
/// degraded "unknown WebKit" page.
pub fn user_agent() -> Option<String> {
    let major = NSProcessInfo::processInfo().operatingSystemVersion().majorVersion;
    let safari = if major >= 26 { major } else { major + 3 };
    Some(format!(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/{safari}.0 Safari/605.1.15"
    ))
}

pub fn tab_builder_ext<'a>(
    b: WebViewBuilder<'a>,
    proxy: &EventLoopProxy<UserEvent>,
    id: Id,
    store: Option<[u8; 16]>,
    private: bool,
) -> WebViewBuilder<'a> {
    let b = match tab_config(store, private) {
        Some(config) => {
            add_notify_handler(&*unsafe { config.userContentController() }, id, proxy);
            b.with_webview_configuration(config)
        }
        None => b,
    };
    let p = proxy.clone();
    b.with_on_web_content_process_terminate_handler(move || {
        let _ = p.send_event(UserEvent::Crashed(id));
    })
}

fn os_major() -> isize {
    NSProcessInfo::processInfo().operatingSystemVersion().majorVersion
}

/// Our own WebKit configuration for every page: the profile's data store (or a
/// non-persistent one for private mode) and full display-rate rendering.
fn tab_config(store: Option<[u8; 16]>, private: bool) -> Option<Retained<WKWebViewConfiguration>> {
    let mtm = MainThreadMarker::new()?;
    let config = unsafe { WKWebViewConfiguration::new(mtm) };
    let data = unsafe {
        match (private, store) {
            // One in-memory store for the whole private window, so a login carries across its tabs.
            (true, _) => PRIVATE_STORE.with(|s| s.get_or_init(|| WKWebsiteDataStore::nonPersistentDataStore(mtm)).clone()),
            (false, Some(id)) if os_major() >= 14 => WKWebsiteDataStore::dataStoreForIdentifier(&NSUUID::from_bytes(id), mtm),
            _ => WKWebsiteDataStore::defaultDataStore(mtm),
        }
    };
    unsafe { config.setWebsiteDataStore(&data) };
    let prefs = unsafe { config.preferences() };
    unlock_frame_rate(&prefs);
    Some(config)
}

/// WebKit caps page rendering (requestAnimationFrame, CSS/JS animation, canvas) near 60 fps
/// even on 120 Hz ProMotion displays. Lift the cap through WebKit's feature-flag API — the
/// same switch Safari's Develop → Feature Flags menu flips.
fn unlock_frame_rate(prefs: &WKPreferences) {
    let cls = WKPreferences::class();
    unsafe {
        let has_features: bool = msg_send![cls, respondsToSelector: sel!(_features)];
        if !has_features || !prefs.respondsToSelector(sel!(_setEnabled:forFeature:)) {
            return;
        }
        let features: Option<Retained<NSArray<NSObject>>> = msg_send![cls, _features];
        for feature in features.iter().flat_map(|a| a.iter()) {
            let key: Option<Retained<NSString>> = msg_send![&*feature, key];
            if key.is_some_and(|k| k.to_string() == "PreferPageRenderingUpdatesNear60FPSEnabled") {
                let _: () = msg_send![prefs, _setEnabled: false, forFeature: &*feature];
            }
        }
    }
}

/// Rounded page corners (Zen look) and the native content blocker.
pub fn style_content(view: &WebView) {
    plain_text_input(view);
    let wk = view.webview();
    wk.setWantsLayer(true);
    if let Some(layer) = wk.layer() {
        layer.setCornerRadius(CONTENT_RADIUS);
        layer.setMasksToBounds(true);
    }
    apply_blocker(view);
}

/// While a page grows (sidebar closing, window resize) WebKit shows the web view's own grey
/// background where it hasn't painted yet. Fill that with the page's background instead, as
/// Safari does, so a resize reads as the page reflowing rather than a strip opening up.
pub fn match_page_background(view: &WebView) {
    let wk = view.webview();
    if !wk.respondsToSelector(sel!(_setBackgroundColor:)) {
        return;
    }
    let page: Option<Retained<NSColor>> = unsafe { msg_send![&*wk, underPageBackgroundColor] };
    if let Some(color) = page {
        let _: () = unsafe { msg_send![&*wk, _setBackgroundColor: &*color] };
    }
}

/// Turns ad and tracker blocking on/off. Rules run in WebKit's content-rule engine (the
/// native matcher Safari content blockers use): compiled once to bytecode that every page
/// memory-maps, so no JavaScript runs per request. Lists compiled on an earlier run are
/// looked up instead of recompiled; until filter lists exist, `rules_json` (the built-in
/// list) is used. `on_ready` fires whenever a new set of lists can be applied to pages.
pub fn set_blocking(enabled: bool, rules_json: impl FnOnce() -> String, on_ready: impl Fn() + 'static) {
    BLOCKING.with(|b| b.set(enabled));
    crate::filters::update(|s| s.enabled = enabled);
    if !enabled || BLOCKER_READY.with(|r| r.borrow().is_some()) {
        return;
    }
    BLOCKER_READY.with(|r| *r.borrow_mut() = Some(Rc::new(on_ready)));
    let builtin = rules_json();
    match crate::filters::Manifest::load().filter(|m| !m.ids.is_empty()) {
        Some(m) => lookup_lists(m, builtin),
        None => {
            compile_lists(vec![("rzen-shields".into(), builtin)], None, crate::filters::Manifest::default());
            refresh_filters(false);
        }
    }
}

/// Current blocker state for the settings page.
pub fn blocker_status() -> crate::filters::FilterStatus {
    crate::filters::status()
}

/// Downloads stale filter lists (all when `force`) and recompiles if they changed. Downloads
/// and conversion run on a background thread; WebKit compiles off the main thread too.
pub fn refresh_filters(force: bool) {
    if BLOCKER_BUSY.with(|b| b.replace(true)) {
        return;
    }
    if force || crate::filters::needs_download() {
        crate::filters::update(|s| s.state = "downloading");
    }
    std::thread::spawn(move || {
        crate::filters::download(force);
        let cached = crate::filters::load_cached();
        on_main(move || after_download(cached));
    });
    start_refresh_timer();
}

fn after_download(cached: Option<(Vec<String>, String)>) {
    let Some((texts, version)) = cached else {
        BLOCKER_BUSY.with(|b| b.set(false));
        return crate::filters::update(|s| s.error = Some("Couldn't download the filter lists. Using the built-in list.".into()));
    };
    if BLOCKER_VERSION.with(|v| v.borrow().as_deref() == Some(version.as_str())) {
        BLOCKER_BUSY.with(|b| b.set(false));
        return crate::filters::update(|s| {
            s.state = "ready";
            s.error = None;
        });
    }
    crate::filters::update(|s| s.state = "converting");
    std::thread::spawn(move || {
        let c = crate::filters::convert(&texts);
        drop(texts);
        let manifest = crate::filters::Manifest {
            version: version.clone(),
            ids: c.lists.iter().map(|l| l.0.clone()).collect(),
            rules: c.rules,
            network: c.network,
            cosmetic: c.cosmetic,
            skipped: c.skipped,
        };
        on_main(move || compile_lists(c.lists, Some(version), manifest));
    });
}

/// Rechecks the lists every six hours while blocking is on (a sleeping thread costs nothing).
fn start_refresh_timer() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(6 * 3600));
            on_main(|| {
                if BLOCKING.with(Cell::get) {
                    refresh_filters(false);
                }
            });
        }
    });
}

/// Runs `f` on the main thread (AppKit's run loop drains the main dispatch queue).
fn on_main(f: impl FnOnce() + Send + 'static) {
    #[repr(C)]
    struct Queue([u8; 0]);
    unsafe extern "C" {
        static _dispatch_main_q: Queue;
        fn dispatch_async_f(queue: *const Queue, context: *mut c_void, work: extern "C" fn(*mut c_void));
    }
    extern "C" fn run(ctx: *mut c_void) {
        let f = unsafe { Box::from_raw(ctx.cast::<Box<dyn FnOnce() + Send>>()) };
        f();
    }
    let ctx: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(f));
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, Box::into_raw(ctx).cast(), run) };
}

fn rule_store() -> Option<Retained<WKContentRuleListStore>> {
    MainThreadMarker::new().and_then(|mtm| unsafe { WKContentRuleListStore::defaultStore(mtm) })
}

/// Startup: memory-maps the lists compiled on an earlier run (no recompiling), then checks
/// for list updates. Falls back to the built-in list if WebKit's store lost them.
fn lookup_lists(m: crate::filters::Manifest, builtin: String) {
    let Some(store) = rule_store() else { return };
    struct Lookup {
        found: Vec<Option<Retained<WKContentRuleList>>>,
        left: usize,
        builtin: Option<String>,
        manifest: crate::filters::Manifest,
    }
    let state = Rc::new(RefCell::new(Lookup { found: vec![None; m.ids.len()], left: m.ids.len(), builtin: Some(builtin), manifest: m.clone() }));
    for (i, id) in m.ids.iter().enumerate() {
        let state = state.clone();
        let done = RcBlock::new(move |list: *mut WKContentRuleList, _err: *mut NSError| {
            let mut st = state.borrow_mut();
            st.found[i] = unsafe { Retained::retain(list) };
            st.left -= 1;
            if st.left > 0 {
                return;
            }
            let lists: Option<Vec<_>> = st.found.drain(..).collect();
            let (builtin, manifest) = (st.builtin.take().unwrap_or_default(), st.manifest.clone());
            drop(st);
            match lists {
                Some(lists) => install(lists, Some(manifest.version.clone()), &manifest),
                None => compile_lists(vec![("rzen-shields".into(), builtin)], None, crate::filters::Manifest::default()),
            }
            refresh_filters(false);
        });
        unsafe { store.lookUpContentRuleListForIdentifier_completionHandler(Some(&NSString::from_str(id)), Some(&done)) };
    }
}

/// Compiles lists one after another (keeps the compile-time memory peak to one list), then
/// swaps them in. A failed cosmetic list is left out; a failed network list keeps the old set.
fn compile_lists(lists: Vec<(String, String)>, version: Option<String>, manifest: crate::filters::Manifest) {
    struct Job {
        pending: Vec<(String, String)>,
        done: Vec<Retained<WKContentRuleList>>,
        ids: Vec<String>,
        version: Option<String>,
        manifest: crate::filters::Manifest,
        error: Option<String>,
        generation: u32,
    }
    fn next(job: Rc<RefCell<Job>>) {
        let Some(store) = rule_store() else { return };
        let item = job.borrow_mut().pending.pop();
        let Some((id, json)) = item else { return finish(job) };
        let j = job.clone();
        let list_id = id.clone();
        let done = RcBlock::new(move |list: *mut WKContentRuleList, err: *mut NSError| {
            match unsafe { Retained::retain(list) } {
                Some(list) => {
                    let mut jb = j.borrow_mut();
                    jb.done.push(list);
                    jb.ids.push(list_id.clone());
                }
                None => {
                    let msg = unsafe { err.as_ref() }.map(|e| e.localizedDescription().to_string()).unwrap_or_default();
                    log!("rzen: content blocker {list_id} failed to compile: {msg}");
                    if !list_id.starts_with("rzen-cosmetic") {
                        j.borrow_mut().error = Some(format!("Filter list failed to compile: {msg}"));
                    }
                }
            }
            next(j.clone());
        });
        let json = NSString::from_str(&json);
        unsafe {
            store.compileContentRuleListForIdentifier_encodedContentRuleList_completionHandler(Some(&NSString::from_str(&id)), Some(&json), Some(&done));
        }
    }
    fn finish(job: Rc<RefCell<Job>>) {
        let Ok(job) = Rc::try_unwrap(job).map(RefCell::into_inner) else { return };
        if job.version.is_some() {
            BLOCKER_BUSY.with(|b| b.set(false));
        }
        if let Some(err) = job.error {
            return crate::filters::update(|s| {
                s.state = if BLOCKER_VERSION.with(|v| v.borrow().is_some()) { "ready" } else { "builtin" };
                s.error = Some(err);
            });
        }
        if job.generation < BLOCKER_GEN.with(Cell::get) {
            return;
        }
        let mut manifest = job.manifest;
        if job.version.is_some() {
            // Drop lists from older runs (and the built-in one) from WebKit's store.
            let old = crate::filters::Manifest::load().map(|m| m.ids).unwrap_or_default();
            if let Some(store) = rule_store() {
                let noop = RcBlock::new(|_: *mut NSError| {});
                for id in old.iter().map(String::as_str).chain(["rzen-shields"]).filter(|id| !job.ids.iter().any(|n| n == id)) {
                    unsafe { store.removeContentRuleListForIdentifier_completionHandler(Some(&NSString::from_str(id)), Some(&noop)) };
                }
            }
            manifest.ids = job.ids;
            manifest.save();
        }
        install(job.done, job.version, &manifest);
    }
    let generation = BLOCKER_GEN.with(|g| {
        g.set(g.get() + 1);
        g.get()
    });
    if version.is_some() {
        crate::filters::update(|s| s.state = "compiling");
    }
    let mut pending = lists;
    pending.reverse();
    next(Rc::new(RefCell::new(Job { pending, done: Vec::new(), ids: Vec::new(), version, manifest, error: None, generation })));
}

fn install(lists: Vec<Retained<WKContentRuleList>>, version: Option<String>, m: &crate::filters::Manifest) {
    let count = lists.len();
    let filters = version.is_some();
    BLOCKER.with(|b| *b.borrow_mut() = lists);
    BLOCKER_VERSION.with(|v| *v.borrow_mut() = version);
    crate::filters::update(|s| {
        s.state = if filters { "ready" } else { "builtin" };
        s.lists = count;
        if filters {
            (s.rules, s.network, s.cosmetic, s.skipped, s.error) = (m.rules, m.network, m.cosmetic, m.skipped, None);
        }
    });
    if let Some(f) = BLOCKER_READY.with(|r| r.borrow().clone()) {
        f();
    }
}

/// Puts the current rule lists on a page (or takes them all off when blocking is off).
pub fn apply_blocker(view: &WebView) {
    let controller = view.manager();
    unsafe { controller.removeAllContentRuleLists() };
    if BLOCKING.with(Cell::get) {
        BLOCKER.with(|b| b.borrow().iter().for_each(|l| unsafe { controller.addContentRuleList(l) }));
    }
}

struct Order {
    chrome: *const c_void,
    chrome_on_top: bool,
}

unsafe extern "C-unwind" fn by_layer(a: NonNull<NSView>, b: NonNull<NSView>, ctx: *mut c_void) -> NSComparisonResult {
    let order = unsafe { &*(ctx as *const Order) };
    let rank = |v: NonNull<NSView>| -> u8 {
        match (v.as_ptr() as *const c_void == order.chrome, order.chrome_on_top) {
            (true, true) => 2,
            (true, false) => 0,
            _ => 1,
        }
    };
    match rank(a).cmp(&rank(b)) {
        std::cmp::Ordering::Less => NSComparisonResult::Ascending,
        std::cmp::Ordering::Equal => NSComparisonResult::Same,
        std::cmp::Ordering::Greater => NSComparisonResult::Descending,
    }
}

/// Re-stacks sibling views in place (no remove/re-add, so nothing reloads or loses state):
/// the chrome either below the pages or above them.
pub fn set_chrome_on_top(chrome: &WebView, on_top: bool) {
    let wk = chrome.webview();
    let Some(parent) = (unsafe { wk.superview() }) else { return };
    let order = Order { chrome: Retained::as_ptr(&wk) as *const c_void, chrome_on_top: on_top };
    unsafe { parent.sortSubviewsUsingFunction_context(by_layer, &order as *const Order as *mut c_void) };
}

pub fn set_traffic_lights(window: &Window, visible: bool) {
    let ns_window = unsafe { &*(window.ns_window() as *const NSWindow) };
    for kind in [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton] {
        if let Some(button) = ns_window.standardWindowButton(kind) {
            button.setHidden(!visible);
        }
    }
    place_traffic_lights(window);
}

/// Close button centered on the sidebar's icon column (x 26) and on the toolbar row (y 22):
/// the button's center lands at (x + 7, y - 2).
pub const TRAFFIC_LIGHT_INSET: (f64, f64) = (19.0, 24.0);

/// AppKit moves the window buttons back to its default corner on events tao doesn't cover
/// (showing them again, resizes, focus changes), so rzen puts them back itself. Same
/// placement as tao's inset; idempotent.
pub fn place_traffic_lights(window: &Window) {
    place_window_buttons(unsafe { &*(window.ns_window() as *const NSWindow) });
}

fn place_window_buttons(ns_window: &NSWindow) {
    let button = |kind| ns_window.standardWindowButton(kind);
    let (Some(close), Some(mini), Some(zoom)) =
        (button(NSWindowButton::CloseButton), button(NSWindowButton::MiniaturizeButton), button(NSWindowButton::ZoomButton))
    else {
        return;
    };
    let Some(bar) = (unsafe { close.superview().and_then(|v| v.superview()) }) else { return };
    let (x, y) = TRAFFIC_LIGHT_INSET;
    let close_frame = close.frame();
    let mut frame = bar.frame();
    frame.size.height = close_frame.size.height + y;
    frame.origin.y = ns_window.frame().size.height - frame.size.height;
    bar.setFrame(frame);
    let step = mini.frame().origin.x - close_frame.origin.x;
    for (i, b) in [close, mini, zoom].iter().enumerate() {
        let mut origin = b.frame().origin;
        origin.x = x + i as f64 * step;
        b.setFrameOrigin(origin);
    }
}

/// Opens the macOS share sheet for `url`, anchored under the given chrome rect.
pub fn share(window: &Window, url: &str, (x, y, w, h): (f64, f64, f64, f64)) {
    let Some(link) = NSURL::URLWithString(&NSString::from_str(url)) else { return };
    let item: &objc2::runtime::AnyObject = &link;
    let items = NSArray::from_slice(&[item]);
    let picker = unsafe { NSSharingServicePicker::initWithItems(NSSharingServicePicker::alloc(), &items) };
    let view = unsafe { &*(window.ns_view() as *const NSView) };
    let (top, edge) = if view.isFlipped() { (y, NSRectEdge::MaxY) } else { (view.frame().size.height - y - h, NSRectEdge::MinY) };
    picker.showRelativeToRect_ofView_preferredEdge(NSRect::new(NSPoint::new(x, top), NSSize::new(w, h)), view, edge);
}

pub fn copy_text(text: &str) {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    pb.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString });
}

pub fn show_menu(window: &Window, menu: &Menu, x: f64, y: f64) {
    unsafe { menu.show_context_menu_for_nsview(window.ns_view() as _, Some(LogicalPosition::new(x, y).into())) };
}

/// `window.open` / `target=_blank`: build the new tab from WebKit's own configuration so
/// `window.opener` keeps working (OAuth "Sign in with…" popups depend on it).
pub fn new_window(sh: &Rc<Shared>, opener: Id, url: String, features: NewWindowFeatures) -> NewWindowResponse {
    let id = sh.alloc_id();
    let config = features.opener.target_configuration;
    match build_tab_view(sh, id, None, None, move |b| b.with_webview_configuration(config)) {
        Ok(view) => {
            let wk: Retained<WKWebView> = Retained::into_super(view.webview());
            sh.popups.borrow_mut().push(Popup { id, opener, url, view });
            let _ = sh.proxy.send_event(UserEvent::AdoptPopups);
            NewWindowResponse::Create { webview: wk }
        }
        Err(e) => {
            log!("rzen: popup failed, opening as tab: {e}");
            let _ = sh.proxy.send_event(UserEvent::OpenTab(url));
            NewWindowResponse::Deny
        }
    }
}

fn bundle_url() -> Option<Retained<NSURL>> {
    let bundle = NSBundle::mainBundle();
    bundle.bundleIdentifier()?;
    Some(bundle.bundleURL())
}

/// `None` when not running from an .app bundle (LaunchServices can't register a bare binary).
pub fn is_default_browser() -> Option<bool> {
    let ours = bundle_url()?;
    let probe = NSURL::URLWithString(&NSString::from_str("https://example.com"))?;
    let current = NSWorkspace::sharedWorkspace().URLForApplicationToOpenURL(&probe)?;
    Some(current.path() == ours.path())
}

/// Asks macOS to make rzen the handler for http/https (the OS shows its own confirmation).
pub fn make_default_browser(on_done: impl Fn() + 'static) -> bool {
    let Some(app) = bundle_url() else { return false };
    let on_done = Rc::new(on_done);
    let ws = NSWorkspace::sharedWorkspace();
    for scheme in ["http", "https"] {
        let cb = on_done.clone();
        let done = RcBlock::new(move |_err: *mut NSError| cb());
        ws.setDefaultApplicationAtURL_toOpenURLsWithScheme_completionHandler(&app, &NSString::from_str(scheme), Some(&done));
    }
    true
}

/// Ctrl+⌫ / Ctrl+⌦ delete a word, as on Windows and Linux. (macOS maps Ctrl+⌫ to
/// "delete by decomposing", which reads as broken.) ⌥⌫ keeps working as usual.
pub fn install_key_fixes() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        let e = unsafe { event.as_ref() };
        let mods = e.modifierFlags()
            & (NSEventModifierFlags::Command | NSEventModifierFlags::Option | NSEventModifierFlags::Control | NSEventModifierFlags::Shift);
        if mods == NSEventModifierFlags::Control {
            let action = match e.keyCode() {
                51 => Some(sel!(deleteWordBackward:)),
                117 => Some(sel!(deleteWordForward:)),
                _ => None,
            };
            if let Some(action) = action {
                if unsafe { NSApplication::sharedApplication(mtm).sendAction_to_from(action, None, None) } {
                    return std::ptr::null_mut();
                }
            }
        }
        event.as_ptr()
    });
    if let Some(monitor) = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &handler) } {
        // Lives for the whole app.
        std::mem::forget(monitor);
    }
}

/// False while WebKit has moved the page into its own element-fullscreen window.
pub fn attached(view: &WebView, window: &Window) -> bool {
    let ours = window.ns_window() as *const NSWindow;
    view.webview().window().is_some_and(|w| Retained::as_ptr(&w) == ours)
}

/// Opens the connection (DNS, TCP, TLS) to `url`'s server before it's needed.
pub fn preconnect(view: &WebView, url: &str) {
    let wk = view.webview();
    if !wk.respondsToSelector(sel!(_preconnectToServer:)) {
        return;
    }
    if let Some(u) = NSURL::URLWithString(&NSString::from_str(url)) {
        let _: () = unsafe { msg_send![&*wk, _preconnectToServer: &*u] };
    }
}

/// Web forms get exactly what was typed: no smart quotes/dashes or text replacement
/// (they corrupt code, passwords and rich editors). Matches Chrome's behaviour.
pub fn plain_text_input(view: &WebView) {
    let wk = view.webview();
    unsafe {
        if wk.respondsToSelector(sel!(setAutomaticTextReplacementEnabled:)) {
            let _: () = msg_send![&*wk, setAutomaticTextReplacementEnabled: false];
        }
        if wk.respondsToSelector(sel!(setAutomaticQuoteSubstitutionEnabled:)) {
            let _: () = msg_send![&*wk, setAutomaticQuoteSubstitutionEnabled: false];
        }
        if wk.respondsToSelector(sel!(setAutomaticDashSubstitutionEnabled:)) {
            let _: () = msg_send![&*wk, setAutomaticDashSubstitutionEnabled: false];
        }
    }
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecTaskCreateFromSelf(allocator: *const c_void) -> *mut c_void;
    fn SecTaskCopyValueForEntitlement(task: *mut c_void, entitlement: *const c_void, error: *mut *mut c_void) -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

/// WebKit only runs WebAuthn/passkeys in a WKWebView app that Apple has entitled as a web
/// browser (`com.apple.developer.web-browser.public-key-credential`).
pub fn passkeys_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| unsafe {
        let task = SecTaskCreateFromSelf(std::ptr::null());
        if task.is_null() {
            return false;
        }
        let key = NSString::from_str("com.apple.developer.web-browser.public-key-credential");
        let value = SecTaskCopyValueForEntitlement(task, Retained::as_ptr(&key).cast(), std::ptr::null_mut());
        CFRelease(task);
        if value.is_null() {
            return false;
        }
        // CFBoolean is toll-free bridged to NSNumber.
        let enabled = (*(value as *const NSNumber)).boolValue();
        CFRelease(value);
        enabled
    })
}

/// Pure black/white window background that follows the light/dark appearance by itself.
/// The chrome stays fully transparent, so every page area (tab, AI panel) is just a hole.
pub fn set_window_background(window: &Window) {
    let ns_window = unsafe { &*(window.ns_window() as *const NSWindow) };
    let (black, white) = (NSColor::blackColor(), NSColor::whiteColor());
    let provider = RcBlock::new(move |appearance: NonNull<NSAppearance>| -> NonNull<NSColor> {
        let names = unsafe { NSArray::from_slice(&[NSAppearanceNameAqua, NSAppearanceNameDarkAqua]) };
        let dark = unsafe { appearance.as_ref() }
            .bestMatchFromAppearancesWithNames(&names)
            .is_some_and(|n| &*n == unsafe { NSAppearanceNameDarkAqua });
        NonNull::from(if dark { &*black } else { &*white })
    });
    let color = unsafe { NSColor::colorWithName_dynamicProvider(None, &provider) };
    ns_window.setBackgroundColor(Some(&color));
}

/// PNG screenshot of what the page currently shows (for AI agents).
pub fn snapshot(view: &WebView, done: impl Fn(Option<Vec<u8>>) + 'static) {
    let wk = view.webview();
    let block = RcBlock::new(move |image: *mut NSImage, _err: *mut NSError| {
        let png = unsafe { image.as_ref() }.and_then(|img| {
            let tiff = img.TIFFRepresentation()?;
            let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
            let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
            Some(data.to_vec())
        });
        done(png);
    });
    unsafe { wk.takeSnapshotWithConfiguration_completionHandler(None, &block) };
}

/// Whether the page is producing sound right now (WebKit's own state, no JavaScript).
pub fn is_playing_audio(view: &WebView) -> bool {
    let wk = view.webview();
    wk.respondsToSelector(sel!(_isPlayingAudio)) && unsafe { msg_send![&*wk, _isPlayingAudio] }
}

/// Mutes every sound source in the page (media elements, Web Audio), like Safari's tab mute.
pub fn set_muted(view: &WebView, muted: bool) -> bool {
    let wk = view.webview();
    if !wk.respondsToSelector(sel!(_setPageMuted:)) {
        return false;
    }
    // _WKMediaAudioMuted = 1 << 0
    let state: usize = if muted { 1 } else { 0 };
    let _: () = unsafe { msg_send![&*wk, _setPageMuted: state] };
    true
}

const KEYCHAIN_SERVICE: &str = "app.rzen.browser";

/// Secrets (the AI API key) live in the macOS Keychain, per profile.
pub fn get_secret(account: &str) -> Option<String> {
    security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, account).ok().and_then(|b| String::from_utf8(b).ok())
}

pub fn set_secret(account: &str, value: &str) -> bool {
    if value.is_empty() {
        return security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, account).is_ok();
    }
    security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, account, value.as_bytes()).is_ok()
}

/// Removes a proxy from a data store (WebKit keeps it on the shared store until told otherwise).
pub fn clear_proxy(store: Option<[u8; 16]>) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    if os_major() < 14 {
        return;
    }
    let data = unsafe {
        match store {
            Some(id) => WKWebsiteDataStore::dataStoreForIdentifier(&NSUUID::from_bytes(id), mtm),
            None => WKWebsiteDataStore::defaultDataStore(mtm),
        }
    };
    let none = NSArray::<NSObject>::new();
    let obj: &NSObject = &data;
    unsafe { objc2_foundation::NSObjectNSKeyValueCoding::setValue_forKey(obj, Some(&none), objc2_foundation::ns_string!("proxyConfigurations")) };
}

/// Drops WebKit's disk and memory caches for a store (cookies and logins stay).
pub fn clear_cache(store: Option<[u8; 16]>, done: impl Fn() + 'static) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let data = unsafe {
        match store {
            Some(id) if os_major() >= 14 => WKWebsiteDataStore::dataStoreForIdentifier(&NSUUID::from_bytes(id), mtm),
            _ => WKWebsiteDataStore::defaultDataStore(mtm),
        }
    };
    let types = objc2_foundation::NSSet::from_slice(&[
        &*NSString::from_str("WKWebsiteDataTypeDiskCache"),
        &*NSString::from_str("WKWebsiteDataTypeMemoryCache"),
        &*NSString::from_str("WKWebsiteDataTypeFetchCache"),
    ]);
    let block = RcBlock::new(move || done());
    unsafe { data.removeDataOfTypes_modifiedSince_completionHandler(&types, &objc2_foundation::NSDate::distantPast(), &block) };
}

thread_local! {
    static PRIVATE_STORE: std::cell::OnceCell<Retained<WKWebsiteDataStore>> = const { std::cell::OnceCell::new() };
    /// Boosts: document-start scripts every page gets (see `boosts.rs`).
    static PAGE_SCRIPTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// The Boost scripts on each live page, so a change swaps exactly those and nothing of wry's.
    static PAGE_INSTALLED: RefCell<HashMap<usize, Vec<Retained<WKUserScript>>>> = RefCell::new(HashMap::new());
}

pub fn set_page_scripts(scripts: Vec<String>) {
    PAGE_SCRIPTS.with(|p| *p.borrow_mut() = scripts);
}

/// Puts the current Boost scripts on a page in place of the ones it had. They run as each
/// document starts, so code changes reach an open page on its next load.
pub fn apply_page_scripts(view: &WebView) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let controller = view.manager();
    let key = Retained::as_ptr(&view.webview()) as usize;
    let old = PAGE_INSTALLED.with(|m| m.borrow_mut().remove(&key)).unwrap_or_default();
    if !old.is_empty() {
        if !controller.respondsToSelector(sel!(_removeUserScript:)) {
            // Can't take single scripts back out on this WebKit: keep what's there rather than doubling up.
            PAGE_INSTALLED.with(|m| m.borrow_mut().insert(key, old));
            return;
        }
        for s in &old {
            let _: () = unsafe { msg_send![&*controller, _removeUserScript: &**s] };
        }
    }
    let scripts: Vec<Retained<WKUserScript>> = PAGE_SCRIPTS.with(|p| {
        p.borrow()
            .iter()
            .map(|src| unsafe {
                WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                    mtm.alloc(),
                    &NSString::from_str(src),
                    WKUserScriptInjectionTime::AtDocumentStart,
                    true,
                )
            })
            .collect()
    });
    for s in &scripts {
        unsafe { controller.addUserScript(s) };
    }
    if !scripts.is_empty() {
        PAGE_INSTALLED.with(|m| m.borrow_mut().insert(key, scripts));
    }
}

/// Tears the page down inside WebKit so its web-content process exits now, rather than
/// whenever the last reference to the view happens to go away.
pub fn release(view: &WebView) {
    let wk = view.webview();
    PAGE_INSTALLED.with(|m| m.borrow_mut().remove(&(Retained::as_ptr(&wk) as usize)));
    if wk.respondsToSelector(sel!(_close)) {
        let _: () = unsafe { msg_send![&*wk, _close] };
    }
}

/// Playing audio or capturing camera/microphone: such tabs are never unloaded.
pub fn is_busy(view: &WebView) -> bool {
    let wk = view.webview();
    let none = WKMediaCaptureState::None;
    let capturing = unsafe { wk.cameraCaptureState() != none || wk.microphoneCaptureState() != none };
    let audio = wk.respondsToSelector(sel!(_isPlayingAudio)) && unsafe { msg_send![&*wk, _isPlayingAudio] };
    capturing || audio
}

/// The web-content process backing a tab, for per-tab memory/CPU.
/// The page's committed URL. wry's `url()` panics while WebKit has none (before a tab's first
/// load commits, or after it failed, e.g. offline), so read it directly.
pub fn view_url(view: &WebView) -> Option<String> {
    let url = unsafe { view.webview().URL() }?;
    Some(url.absoluteString()?.to_string())
}

pub fn web_process_pid(view: &WebView) -> Option<i32> {
    let wk = view.webview();
    if !wk.respondsToSelector(sel!(_webProcessIdentifier)) {
        return None;
    }
    let pid: i32 = unsafe { msg_send![&*wk, _webProcessIdentifier] };
    (pid > 0).then_some(pid)
}

/// WebKit already throttles hidden views; nothing extra needed.
pub fn background(_view: &WebView) {}
pub fn foreground(_view: &WebView) {}
