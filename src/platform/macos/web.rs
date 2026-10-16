//! Web-platform pieces WKWebView leaves to the host app: ⌘-shortcuts for pages, JavaScript
//! dialogs (`alert`/`confirm`/`prompt`) and web notifications.

use std::{cell::Cell, ffi::c_char, path::Path, sync::OnceLock};

use block2::{Block, DynBlock, RcBlock};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, Bool, Imp, NSObject, NSObjectProtocol, ProtocolObject, Sel},
    sel,
};
use objc2_app_kit::{NSAlert, NSApplication, NSBitmapImageFileType, NSBitmapImageRep, NSEvent, NSEventModifierFlags, NSImage, NSMenu, NSTextField, NSView, NSWindow};
use objc2_foundation::{NSArray, NSBundle, NSDictionary, NSNumber, NSPoint, NSRect, NSSize, NSString, NSURL, NSUUID, ns_string};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAttachment, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use objc2_web_kit::{WKFrameInfo, WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKWebView};
use tao::{event_loop::EventLoopProxy, platform::macos::WindowExtMacOS, window::Window};
use wry::{WebView, WebViewExtMacOS};

use crate::{
    browser::{UserEvent, WebNotification},
    model::Id,
};

// ---------- ⌘-shortcuts ----------

static WEBKIT_KEY_EQUIVALENT: OnceLock<usize> = OnceLock::new();
static WEBKIT_BECOME_FIRST_RESPONDER: OnceLock<usize> = OnceLock::new();

thread_local! {
    static CHROME: Cell<usize> = const { Cell::new(0) };
    /// One of the browser UI's text fields has the keyboard (command bar, find, rename, AI).
    static UI_TYPING: Cell<bool> = const { Cell::new(false) };
}

/// Extends wry's web view classes (key routing, JavaScript dialogs). objc2 names classes
/// after their module path and crate version, so they're found through the chrome's own
/// objects rather than by name. The chrome itself keeps wry's key handling.
pub fn extend_wry(chrome: &WebView) {
    let wk = chrome.webview();
    CHROME.with(|c| c.set(Retained::as_ptr(&wk) as usize));
    route_key_equivalents(wk.class());
    guard_keyboard(wk.class());
    if let Some(delegate) = unsafe { wk.UIDelegate() } {
        let delegate: &AnyObject = delegate.as_ref();
        install_js_dialogs(delegate.class());
    }
}

/// wry answers NO to `performKeyEquivalent:` for every child web view, so pages never see
/// ⌘-shortcuts (⌘B/⌘I in editors, ⌘K pickers, ⌘↩ to send). The focused page now gets WebKit's
/// own handling for every ⌘-shortcut the menu bar doesn't use. Menu shortcuts (⌘C/⌘V/⌘Z, ⌘T,
/// ⌘L…) stay with the menu bar: WebKit hands back keys a page doesn't take by re-sending them,
/// and in a tao window that re-send never reaches the menu, so ⌘C would stop copying.
fn route_key_equivalents(wry: &AnyClass) {
    let Some(wk) = AnyClass::get(c"WKWebView") else { return };
    let (Some(ours), Some(webkit)) = (wry.instance_method(sel!(performKeyEquivalent:)), wk.instance_method(sel!(performKeyEquivalent:))) else {
        return;
    };
    if WEBKIT_KEY_EQUIVALENT.set(webkit.implementation() as usize).is_err() {
        return;
    }
    let imp: unsafe extern "C-unwind" fn(&NSView, Sel, &NSEvent) -> Bool = key_equivalent;
    unsafe { ours.set_implementation(std::mem::transmute::<_, Imp>(imp)) };
}

unsafe extern "C-unwind" fn key_equivalent(this: &NSView, cmd: Sel, event: &NSEvent) -> Bool {
    let me = this as *const NSView as usize;
    let focused = this.window().and_then(|w| w.firstResponder()).is_some_and(|r| Retained::as_ptr(&r) as usize == me);
    if !focused || CHROME.with(Cell::get) == me || !event.modifierFlags().contains(NSEventModifierFlags::Command) || menu_handles(event) {
        return Bool::NO;
    }
    let Some(&imp) = WEBKIT_KEY_EQUIVALENT.get() else { return Bool::NO };
    let webkit: unsafe extern "C-unwind" fn(&NSView, Sel, &NSEvent) -> Bool = unsafe { std::mem::transmute(imp) };
    unsafe { webkit(this, cmd, event) }
}

const KEY_MODS: NSEventModifierFlags = NSEventModifierFlags::Command
    .union(NSEventModifierFlags::Shift)
    .union(NSEventModifierFlags::Option)
    .union(NSEventModifierFlags::Control);

/// Whether a menu bar item uses this shortcut (read from the live menu, so it never drifts).
fn menu_handles(e: &NSEvent) -> bool {
    fn walk(menu: &NSMenu, key: &str, mods: NSEventModifierFlags) -> bool {
        menu.itemArray().iter().any(|item| {
            if item.submenu().is_some_and(|sub| walk(&sub, key, mods)) {
                return true;
            }
            let k = item.keyEquivalent().to_string();
            // An upper-case key equivalent implies Shift.
            let shift = if k.chars().any(char::is_uppercase) { NSEventModifierFlags::Shift } else { NSEventModifierFlags::empty() };
            !k.is_empty() && k.to_lowercase() == key && ((item.keyEquivalentModifierMask() | shift) & KEY_MODS) == mods
        })
    }
    let Some(mtm) = MainThreadMarker::new() else { return true };
    let Some(menu) = NSApplication::sharedApplication(mtm).mainMenu() else { return false };
    let key = e.charactersIgnoringModifiers().map(|s| s.to_string().to_lowercase()).unwrap_or_default();
    walk(&menu, &key, e.modifierFlags() & KEY_MODS)
}

// ---------- keyboard ownership ----------

/// The UI tells rzen when one of its text fields gains or loses focus.
pub fn set_ui_typing(on: bool) {
    UI_TYPING.with(|c| c.set(on));
}

/// A page's script can focus one of its own fields (a search box that focuses itself as the
/// page loads) and WebKit then takes the window's keyboard, out from under the command bar or
/// find field you're typing in. While a UI field has focus, only a real click moves the
/// keyboard to a page; rzen hands it over itself when it means to (`set_ui_typing(false)` first).
fn guard_keyboard(wry: &AnyClass) {
    let Some(wk) = AnyClass::get(c"WKWebView") else { return };
    let Some(webkit) = wk.instance_method(sel!(becomeFirstResponder)) else { return };
    if WEBKIT_BECOME_FIRST_RESPONDER.set(webkit.implementation() as usize).is_err() {
        return;
    }
    let imp: unsafe extern "C-unwind" fn(&NSView, Sel) -> Bool = become_first_responder;
    let cls = wry as *const AnyClass as *mut AnyClass;
    unsafe { objc2::ffi::class_addMethod(cls, sel!(becomeFirstResponder), std::mem::transmute::<_, Imp>(imp), c"B@:".as_ptr()) };
}

unsafe extern "C-unwind" fn become_first_responder(this: &NSView, cmd: Sel) -> Bool {
    let me = this as *const NSView as usize;
    if UI_TYPING.with(Cell::get) && CHROME.with(Cell::get) != me && !clicked() {
        return Bool::NO;
    }
    let Some(&imp) = WEBKIT_BECOME_FIRST_RESPONDER.get() else { return Bool::YES };
    let webkit: unsafe extern "C-unwind" fn(&NSView, Sel) -> Bool = unsafe { std::mem::transmute(imp) };
    unsafe { webkit(this, cmd) }
}

/// Whether the event being handled is a mouse press (the user clicked into a view).
fn clicked() -> bool {
    use objc2_app_kit::NSEventType;
    let Some(mtm) = MainThreadMarker::new() else { return false };
    NSApplication::sharedApplication(mtm)
        .currentEvent()
        .is_some_and(|e| matches!(e.r#type(), NSEventType::LeftMouseDown | NSEventType::RightMouseDown | NSEventType::OtherMouseDown))
}

// ---------- alert / confirm / prompt ----------

/// A sheet on `window`; `done` gets the index of the button pressed and the text field's value.
pub fn sheet(window: &NSWindow, title: &str, message: &str, buttons: &[&str], field: Option<&str>, done: impl Fn(usize, String) + 'static) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    for b in buttons {
        alert.addButtonWithTitle(&NSString::from_str(b));
    }
    let input = field.map(|text| {
        let f = NSTextField::initWithFrame(NSTextField::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(260.0, 24.0)));
        f.setStringValue(&NSString::from_str(text));
        alert.setAccessoryView(Some(&f));
        f
    });
    if let Some(f) = &input {
        alert.layout();
        alert.window().setInitialFirstResponder(Some(f));
    }
    let handler = RcBlock::new(move |response: isize| {
        let text = input.as_ref().map(|f| f.stringValue().to_string()).unwrap_or_default();
        done((response - 1000).max(0) as usize, text);
    });
    alert.beginSheetModalForWindow_completionHandler(window, Some(&handler));
}

fn frame_host(frame: &WKFrameInfo) -> String {
    let host = unsafe { frame.securityOrigin().host() }.to_string();
    if host.is_empty() { "This page".into() } else { host }
}

/// Dialogs only show for a page that's on screen; a background tab gets the "dismissed" answer.
fn dialog_window(web: &WKWebView) -> Option<Retained<NSWindow>> {
    web.window().filter(|_| !web.isHiddenOrHasHiddenAncestor())
}

unsafe extern "C-unwind" fn js_alert(_this: &AnyObject, _cmd: Sel, web: &WKWebView, message: &NSString, frame: &WKFrameInfo, done: &Block<dyn Fn()>) {
    let done = done.copy();
    match dialog_window(web) {
        Some(w) => sheet(&w, &frame_host(frame), &message.to_string(), &["OK"], None, move |_, _| done.call(())),
        None => done.call(()),
    }
}

unsafe extern "C-unwind" fn js_confirm(_this: &AnyObject, _cmd: Sel, web: &WKWebView, message: &NSString, frame: &WKFrameInfo, done: &Block<dyn Fn(Bool)>) {
    let done = done.copy();
    match dialog_window(web) {
        Some(w) => sheet(&w, &frame_host(frame), &message.to_string(), &["OK", "Cancel"], None, move |i, _| done.call((Bool::new(i == 0),))),
        None => done.call((Bool::NO,)),
    }
}

unsafe extern "C-unwind" fn js_prompt(
    _this: &AnyObject,
    _cmd: Sel,
    web: &WKWebView,
    prompt: &NSString,
    default: Option<&NSString>,
    frame: &WKFrameInfo,
    done: &Block<dyn Fn(*mut NSString)>,
) {
    let done = done.copy();
    let default = default.map(|d| d.to_string()).unwrap_or_default();
    match dialog_window(web) {
        Some(w) => sheet(&w, &frame_host(frame), &prompt.to_string(), &["OK", "Cancel"], Some(&default), move |i, text| {
            let value = (i == 0).then(|| NSString::from_str(&text));
            done.call((value.as_ref().map_or(std::ptr::null_mut(), |v| Retained::as_ptr(v) as *mut NSString),));
        }),
        None => done.call((std::ptr::null_mut(),)),
    }
}

/// wry's UI delegate has no JavaScript panels, so WebKit silently skips `alert()` and answers
/// `confirm()`/`prompt()` with "cancel". Teach the class (shared by every web view) to show them.
fn install_js_dialogs(cls: &AnyClass) {
    let cls = cls as *const AnyClass as *mut AnyClass;
    let alert: unsafe extern "C-unwind" fn(_, _, _, _, _, _) = js_alert;
    let confirm: unsafe extern "C-unwind" fn(_, _, _, _, _, _) = js_confirm;
    let prompt: unsafe extern "C-unwind" fn(_, _, _, _, _, _, _) = js_prompt;
    let methods: [(Sel, Imp, &[u8]); 3] = unsafe {
        [
            (sel!(webView:runJavaScriptAlertPanelWithMessage:initiatedByFrame:completionHandler:), std::mem::transmute(alert), b"v@:@@@@?\0"),
            (sel!(webView:runJavaScriptConfirmPanelWithMessage:initiatedByFrame:completionHandler:), std::mem::transmute(confirm), b"v@:@@@@?\0"),
            (
                sel!(webView:runJavaScriptTextInputPanelWithPrompt:defaultText:initiatedByFrame:completionHandler:),
                std::mem::transmute(prompt),
                b"v@:@@@@@?\0",
            ),
        ]
    };
    for (sel, imp, types) in methods {
        unsafe { objc2::ffi::class_addMethod(cls, sel, imp, types.as_ptr() as *const c_char) };
    }
}

// ---------- notifications ----------

/// Notification Center only works for a real app bundle (not a bare `cargo run` binary).
pub fn notifications_supported() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let b = NSBundle::mainBundle();
        b.bundleIdentifier().is_some() && b.bundlePath().to_string().ends_with(".app")
    })
}

struct NotifyIvars {
    tab: Id,
    proxy: EventLoopProxy<UserEvent>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RzenNotifyHandler"]
    #[ivars = NotifyIvars]
    struct NotifyHandler;

    unsafe impl NSObjectProtocol for NotifyHandler {}

    unsafe impl WKScriptMessageHandler for NotifyHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            // Only top-level pages may notify, attributed to the origin WebKit reports, never
            // to anything the page claims about itself.
            let frame = unsafe { message.frameInfo() };
            if !unsafe { frame.isMainFrame() } {
                return;
            }
            let Ok(body) = unsafe { message.body() }.downcast::<NSString>() else { return };
            let origin = unsafe { frame.securityOrigin() };
            let (scheme, host, port) = unsafe { (origin.protocol().to_string(), origin.host().to_string(), origin.port()) };
            if host.is_empty() {
                return;
            }
            let origin = if port == 0 { format!("{scheme}://{host}") } else { format!("{scheme}://{host}:{port}") };
            let iv = self.ivars();
            let _ = iv.proxy.send_event(UserEvent::Notify(iv.tab, origin, body.to_string()));
        }
    }
);

/// Lets this page's notification shim reach the browser (see `NOTIFY_JS`).
pub fn add_notify_handler(controller: &WKUserContentController, tab: Id, proxy: &EventLoopProxy<UserEvent>) {
    let Some(mtm) = MainThreadMarker::new().filter(|_| notifications_supported()) else { return };
    let handler: Retained<NotifyHandler> = {
        let this = NotifyHandler::alloc(mtm).set_ivars(NotifyIvars { tab, proxy: proxy.clone() });
        unsafe { msg_send![super(this), init] }
    };
    unsafe { controller.addScriptMessageHandler_name(ProtocolObject::from_ref(&*handler), ns_string!("rzenNotify")) };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "RzenNotifyDelegate"]
    #[ivars = EventLoopProxy<UserEvent>]
    struct NotifyDelegate;

    unsafe impl NSObjectProtocol for NotifyDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotifyDelegate {
        // Banners show even while rzen is in front (another tab may be the one notifying).
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(&self, _c: &UNUserNotificationCenter, _n: &UNNotification, done: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>) {
            done.call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List | UNNotificationPresentationOptions::Sound,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(&self, _c: &UNUserNotificationCenter, response: &UNNotificationResponse, done: &DynBlock<dyn Fn()>) {
            let info = response.notification().request().content().userInfo();
            let tab: Option<Retained<AnyObject>> = unsafe { msg_send![&*info, objectForKey: ns_string!("tab")] };
            let id: Option<Retained<AnyObject>> = unsafe { msg_send![&*info, objectForKey: ns_string!("id")] };
            let tab = tab.and_then(|t| t.downcast::<NSNumber>().ok()).map(|n| n.unsignedLongLongValue());
            let id = id.and_then(|i| i.downcast::<NSString>().ok()).map(|s| s.to_string());
            if let (Some(tab), Some(id)) = (tab, id) {
                let _ = self.ivars().send_event(UserEvent::NotifyClick(tab as Id, id));
            }
            done.call(());
        }
    }
);

pub fn init_notifications(proxy: &EventLoopProxy<UserEvent>) {
    if !notifications_supported() {
        return;
    }
    let delegate: Retained<NotifyDelegate> = {
        let this = NotifyDelegate::alloc().set_ivars(proxy.clone());
        unsafe { msg_send![super(this), init] }
    };
    UNUserNotificationCenter::currentNotificationCenter().setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // The center holds its delegate weakly; this one lives for the whole app.
    std::mem::forget(delegate);
}

/// Asks macOS once for permission to post; `UserEvent::NotifyAuth` carries the answer.
pub fn authorize_notifications(proxy: &EventLoopProxy<UserEvent>) {
    let p = proxy.clone();
    let done = RcBlock::new(move |granted: Bool, _err: *mut objc2_foundation::NSError| {
        let _ = p.send_event(UserEvent::NotifyAuth(granted.as_bool()));
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound, &done);
}

/// Posts a page's notification, with the site's icon beside it when there is one. The same
/// `key` (the page's tag, else its id) replaces an earlier one.
pub fn show_notification(n: &WebNotification, icon: Option<&Path>) {
    let (tab, key) = (n.tab, n.key.as_str());
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&n.title));
    content.setSubtitle(&NSString::from_str(&n.site));
    content.setBody(&NSString::from_str(&n.body));
    if !n.silent {
        content.setSound(Some(&UNNotificationSound::defaultSound()));
    }
    if let Some(attachment) = icon.and_then(icon_attachment) {
        content.setAttachments(&NSArray::from_retained_slice(&[attachment]));
    }
    let tab_n = NSNumber::new_u64(tab as u64);
    let id_s = NSString::from_str(&n.id);
    let info = NSDictionary::<NSString, AnyObject>::from_slices(&[ns_string!("tab"), ns_string!("id")], &[tab_n.as_ref(), id_s.as_ref()]);
    unsafe { content.setUserInfo(&*(Retained::as_ptr(&info) as *const NSDictionary)) };
    let ident = NSString::from_str(&format!("{tab}:{key}"));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&ident, &content, None);
    UNUserNotificationCenter::currentNotificationCenter().addNotificationRequest_withCompletionHandler(&request, None);
}

/// Notification Center takes PNG, JPEG or GIF attachments and moves the file into its own store,
/// so the cached icon (often .ico or SVG) is redrawn as a PNG in a temporary file of its own.
fn icon_attachment(path: &Path) -> Option<Retained<UNNotificationAttachment>> {
    let image = NSImage::initWithContentsOfFile(NSImage::alloc(), &NSString::from_str(path.to_str()?))?;
    let tiff = image.TIFFRepresentation()?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
    let file = std::env::temp_dir().join(format!("rzen-notification-{}.png", NSUUID::new().UUIDString()));
    std::fs::write(&file, png.to_vec()).ok()?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(file.to_str()?));
    match unsafe { UNNotificationAttachment::attachmentWithIdentifier_URL_options_error(ns_string!("icon"), &url, None) } {
        Ok(a) => Some(a),
        Err(e) => {
            log!("rzen: notification icon not attached: {}", e.localizedDescription());
            None
        }
    }
}

pub fn close_notification(tab: Id, key: &str) {
    let ident = NSString::from_str(&format!("{tab}:{key}"));
    UNUserNotificationCenter::currentNotificationCenter().removeDeliveredNotificationsWithIdentifiers(&NSArray::from_slice(&[&*ident]));
}

/// Brings rzen forward (a notification click).
pub fn activate_app() {
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).activate();
    }
}

/// Permission prompt for a site, as a sheet on the browser window.
pub fn ask_notifications(window: &Window, site: &str, done: impl Fn(bool) + 'static) {
    let ns_window = unsafe { &*(window.ns_window() as *const NSWindow) };
    sheet(
        ns_window,
        &format!("Allow “{site}” to send notifications?"),
        "They appear in Notification Center, even when this tab isn't open in front.",
        &["Allow", "Don't Allow"],
        None,
        move |i, _| done(i == 0),
    );
}

/// Page-side `Notification`: WebKit's own has no backend in an embedded web view, so it can
/// never be granted or shown. `__PERMS__` is the profile's per-site decisions.
pub const NOTIFY_JS: &str = r#"(() => {
  const H = window.webkit && webkit.messageHandlers && webkit.messageHandlers.rzenNotify;
  if (!H || window.__rzenNotify) return;
  const perms = __PERMS__;
  let perm = location.origin in perms ? (perms[location.origin] ? 'granted' : 'denied') : 'default';
  const post = (m) => H.postMessage(JSON.stringify(m));
  const live = new Map(), waits = new Map();
  let seq = 0;
  class Notification extends EventTarget {
    constructor(title, o = {}) {
      super();
      o = o || {};
      Object.assign(this, { title: String(title), body: o.body == null ? '' : String(o.body), tag: o.tag == null ? '' : String(o.tag),
        icon: o.icon || '', image: o.image || '', badge: o.badge || '', data: o.data === undefined ? null : o.data, dir: o.dir || 'auto',
        lang: o.lang || '', silent: !!o.silent, renotify: !!o.renotify, requireInteraction: !!o.requireInteraction,
        timestamp: o.timestamp || Date.now(), actions: [], vibrate: [], onclick: null, onshow: null, onclose: null, onerror: null });
      Object.defineProperty(this, '_id', { value: 'n' + ++seq });
      if (perm !== 'granted') { queueMicrotask(() => this._fire('error')); return; }
      for (const [id, n] of live) if (this.tag && n.tag === this.tag) live.delete(id);
      live.set(this._id, this);
      let icon = '';
      try { icon = this.icon ? new URL(this.icon, location.href).href : ''; } catch (e) {}
      post({ t: 'show', id: this._id, key: this.tag || this._id, title: this.title, body: this.body, icon, silent: this.silent });
      queueMicrotask(() => this._fire('show'));
    }
    _fire(type) {
      const e = new Event(type, { cancelable: true });
      if (typeof this['on' + type] === 'function') this['on' + type].call(this, e);
      this.dispatchEvent(e);
      return e;
    }
    close() {
      if (!live.delete(this._id)) return;
      post({ t: 'close', key: this.tag || this._id });
      this._fire('close');
    }
    static get permission() { return perm; }
    static get maxActions() { return 0; }
    static requestPermission(cb) {
      // Like Safari: a site may only ask in response to a click or key press.
      const asked = perm !== 'default' || !(navigator.userActivation && navigator.userActivation.isActive)
        ? Promise.resolve(perm)
        : new Promise((resolve) => { const id = ++seq; waits.set(id, resolve); post({ t: 'request', id }); });
      return asked.then((p) => { if (typeof cb === 'function') cb(p); return p; });
    }
  }
  Object.defineProperty(window, 'Notification', { value: Notification, writable: true, configurable: true });
  Object.defineProperty(window, '__rzenNotify', { value: {
    perm(p) { perm = p; },
    resolve(id, p) { perm = p; const r = waits.get(id); waits.delete(id); if (r) r(p); },
    click(id) { const n = live.get(id); if (n) n._fire('click'); },
  } });
  if (window.ServiceWorkerRegistration) {
    const proto = ServiceWorkerRegistration.prototype;
    proto.showNotification = function (title, o) {
      if (perm !== 'granted') return Promise.reject(new TypeError('No permission to show notifications'));
      new Notification(title, o);
      return Promise.resolve();
    };
    proto.getNotifications = function () { return Promise.resolve([...live.values()]); };
  }
  const query = navigator.permissions && navigator.permissions.query;
  if (query) {
    navigator.permissions.query = function (d) {
      if (d && d.name === 'notifications') {
        const state = perm === 'default' ? 'prompt' : perm;
        return Promise.resolve(Object.assign(new EventTarget(), { name: 'notifications', state, onchange: null }));
      }
      return query.call(navigator.permissions, d);
    };
  }
})();"#;
