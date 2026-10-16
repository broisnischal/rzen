//! The status bubble at the page's bottom-left corner: a hovered link's address, or what's
//! loading while the sidebar is collapsed. It's a native view above the pages (the page views
//! sit above the chrome, so the chrome can't draw there) that never takes part in hit-testing,
//! so every click still reaches the page under it.

use std::cell::{Cell, RefCell};

use objc2::{
    MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, Imp, Sel},
    sel,
};
use objc2_app_kit::{NSAppearanceCustomization, NSColor, NSEvent, NSFont, NSLineBreakMode, NSTextField, NSView, NSWindow};
use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString, NSURL};
use tao::{event_loop::EventLoopProxy, platform::macos::WindowExtMacOS, window::Window};
use wry::{WebView, WebViewExtMacOS};

use crate::browser::UserEvent;

const HEIGHT: f64 = 22.0;
/// From the page's own edges.
const INSET: f64 = 6.0;
const PAD_X: f64 = 8.0;

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RzenStatusBubble"]
    struct Bubble;

    impl Bubble {
        /// Clicks go straight through to the page.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> *mut NSView {
            std::ptr::null_mut()
        }
    }
);

struct Parts {
    bubble: Retained<Bubble>,
    label: Retained<NSTextField>,
    bar: Retained<NSView>,
    shown: Cell<bool>,
}

thread_local! {
    static PARTS: RefCell<Option<Parts>> = const { RefCell::new(None) };
    static LINKS: RefCell<Option<EventLoopProxy<UserEvent>>> = const { RefCell::new(None) };
    /// The last link reported, per page, so mouse moves within one link stay quiet.
    static LAST_LINK: RefCell<(usize, String)> = const { RefCell::new((0, String::new())) };
}

/// Shows `text` (with a load-progress line when `progress` is set) at the bottom-left of
/// `page` (window points, top-left origin), or fades the bubble out for `None`.
pub fn set_status(window: &Window, status: Option<(&str, Option<f64>)>, page: (f64, f64, f64, f64)) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let ns_window = unsafe { &*(window.ns_window() as *const NSWindow) };
    let Some(content) = ns_window.contentView() else { return };
    PARTS.with(|cell| {
        let mut cell = cell.borrow_mut();
        let Some((text, progress)) = status else {
            if let Some(p) = cell.as_ref().filter(|p| p.shown.get()) {
                p.shown.set(false);
                fade(&p.bubble, 0.0, 0.12);
            }
            return;
        };
        let p = cell.get_or_insert_with(|| build(mtm));
        // New page views are added above it; keep it on top.
        let top = content.subviews().lastObject().is_some_and(|v| std::ptr::eq(&*v, &**p.bubble as &NSView));
        if !top {
            p.bubble.removeFromSuperview();
            content.addSubview(&p.bubble);
        }
        paint(p, ns_window);
        p.label.setStringValue(&NSString::from_str(text));
        let (x, y, w, h) = page;
        p.label.sizeToFit();
        let width = (p.label.frame().size.width.ceil() + 2.0 * PAD_X).clamp(60.0, (w * 0.6).max(60.0));
        let bottom = y + h - INSET - HEIGHT;
        let oy = if content.isFlipped() { bottom } else { content.frame().size.height - bottom - HEIGHT };
        p.bubble.setFrame(NSRect::new(NSPoint::new(x + INSET, oy), NSSize::new(width, HEIGHT)));
        p.label.setFrame(NSRect::new(NSPoint::new(PAD_X, 3.0), NSSize::new(width - 2.0 * PAD_X, 16.0)));
        match progress {
            Some(f) => {
                p.bar.setHidden(false);
                p.bar.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new((width * f.clamp(0.05, 1.0)).round(), 2.0)));
            }
            None => p.bar.setHidden(true),
        }
        if !p.shown.get() {
            p.shown.set(true);
            fade(&p.bubble, 1.0, 0.08);
        }
    });
}

fn build(mtm: MainThreadMarker) -> Parts {
    let zero = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(HEIGHT, HEIGHT));
    let bubble: Retained<Bubble> = unsafe { msg_send![Bubble::alloc(mtm), initWithFrame: zero] };
    bubble.setWantsLayer(true);
    bubble.setAlphaValue(0.0);
    if let Some(layer) = bubble.layer() {
        layer.setCornerRadius(6.0);
        layer.setMasksToBounds(true);
        layer.setBorderWidth(1.0);
    }
    let label = NSTextField::labelWithString(&NSString::new(), mtm);
    label.setFont(Some(&NSFont::systemFontOfSize(11.5)));
    label.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
    label.setMaximumNumberOfLines(1);
    bubble.addSubview(&label);
    let bar = NSView::initWithFrame(NSView::alloc(mtm), zero);
    bar.setWantsLayer(true);
    bubble.addSubview(&bar);
    Parts { bubble, label, bar, shown: Cell::new(false) }
}

/// Black and white, following the window's light or dark look.
fn paint(p: &Parts, window: &NSWindow) {
    let names = NSArray::from_retained_slice(&[NSString::from_str("NSAppearanceNameAqua"), NSString::from_str("NSAppearanceNameDarkAqua")]);
    let dark = window.effectiveAppearance().bestMatchFromAppearancesWithNames(&names).is_some_and(|n| n.to_string().contains("Dark"));
    let (bg, fg, line) = if dark { (0.08, 1.0, 0.16) } else { (1.0, 0.0, 0.12) };
    if let Some(layer) = p.bubble.layer() {
        layer.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(bg, 0.94).CGColor()));
        layer.setBorderColor(Some(&NSColor::colorWithWhite_alpha(fg, line).CGColor()));
    }
    p.label.setTextColor(Some(&NSColor::colorWithWhite_alpha(fg, 0.86)));
    if let Some(layer) = p.bar.layer() {
        layer.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(fg, 0.55).CGColor()));
    }
}

fn fade(view: &NSView, to: f64, secs: f64) {
    unsafe {
        let ctx: *mut AnyObject = msg_send![objc2::class!(NSAnimationContext), currentContext];
        let _: () = msg_send![objc2::class!(NSAnimationContext), beginGrouping];
        let _: () = msg_send![ctx, setDuration: secs];
        let animator: *mut AnyObject = msg_send![view, animator];
        let _: () = msg_send![animator, setAlphaValue: to];
        let _: () = msg_send![objc2::class!(NSAnimationContext), endGrouping];
    }
}

/// Reports the link under the pointer in any page (`UserEvent::LinkHover`), using WebKit's own
/// hover callback, which covers every frame.
pub fn watch_links(chrome: &WebView, proxy: EventLoopProxy<UserEvent>) {
    LINKS.with(|l| *l.borrow_mut() = Some(proxy));
    let Some(delegate) = (unsafe { chrome.webview().UIDelegate() }) else { return };
    let delegate: &AnyObject = delegate.as_ref();
    let cls = delegate.class() as *const AnyClass as *mut AnyClass;
    let imp: unsafe extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, *mut AnyObject, usize, *mut AnyObject) = mouse_over;
    unsafe {
        objc2::ffi::class_addMethod(cls, sel!(_webView:mouseDidMoveOverElement:withFlags:userInfo:), std::mem::transmute::<_, Imp>(imp), c"v@:@@Q@".as_ptr());
    }
}

unsafe extern "C-unwind" fn mouse_over(_this: &AnyObject, _cmd: Sel, web_view: &AnyObject, hit: *mut AnyObject, _flags: usize, _info: *mut AnyObject) {
    let url = if hit.is_null() {
        String::new()
    } else {
        let link: Option<Retained<NSURL>> = unsafe { msg_send![hit, absoluteLinkURL] };
        link.and_then(|u| u.absoluteString()).map(|s| s.to_string()).unwrap_or_default()
    };
    let key = web_view as *const AnyObject as usize;
    let changed = LAST_LINK.with(|l| {
        let mut l = l.borrow_mut();
        let changed = l.0 != key || l.1 != url;
        *l = (key, url.clone());
        changed
    });
    if changed {
        LINKS.with(|p| {
            if let Some(p) = p.borrow().as_ref() {
                let _ = p.send_event(UserEvent::LinkHover(key, url));
            }
        });
    }
}

/// Identifies a page in `UserEvent::LinkHover`.
pub fn view_key(view: &WebView) -> usize {
    Retained::as_ptr(&view.webview()) as usize
}

/// How far the page's current load has got, 0 to 1.
pub fn load_progress(view: &WebView) -> f64 {
    unsafe { view.webview().estimatedProgress() }
}

/// Whether the pointer is over this page (a hovered link stops counting once it leaves).
pub fn pointer_over(view: &WebView) -> bool {
    let wk = view.webview();
    let Some(window) = wk.window() else { return false };
    let p = window.convertPointFromScreen(NSEvent::mouseLocation());
    let local = wk.convertPoint_fromView(p, None);
    let b = wk.bounds();
    local.x >= 0.0 && local.y >= 0.0 && local.x <= b.size.width && local.y <= b.size.height
}
