//! Windows (WebView2) and other platforms: portable fallbacks.

use std::rc::Rc;

use muda::Menu;
use tao::{event_loop::EventLoopProxy, window::Window};
use wry::{NewWindowFeatures, NewWindowResponse, WebView, WebViewBuilder};

use crate::{
    browser::{Shared, UserEvent},
    model::Id,
};

/// Native child windows can't alpha-blend over each other here, so the command bar
/// renders inside the sidebar instead of floating over the page.
pub const OVERLAY: bool = false;
pub const SHARE: bool = false;

pub fn user_agent() -> Option<String> {
    None
}

pub fn tab_builder_ext<'a>(b: WebViewBuilder<'a>, _: &EventLoopProxy<UserEvent>, _: Id, store: Option<[u8; 16]>, _: bool) -> WebViewBuilder<'a> {
    let _ = store;
    b
}

pub fn style_content(_view: &WebView) {}
pub fn set_blocking(_enabled: bool, _rules_json: impl FnOnce() -> String, _on_ready: impl Fn() + 'static) {}
pub fn apply_blocker(_view: &WebView) {}
pub fn blocker_status() -> crate::filters::FilterStatus {
    crate::filters::status()
}
pub fn refresh_filters(_force: bool) {}
pub fn set_chrome_on_top(_chrome: &WebView, _on_top: bool) {}
pub fn extend_wry(_chrome: &WebView) {}
pub fn set_ui_typing(_on: bool) {}
pub fn notifications_supported() -> bool {
    false
}
pub fn init_notifications(_proxy: &tao::event_loop::EventLoopProxy<crate::browser::UserEvent>) {}
pub fn authorize_notifications(_proxy: &tao::event_loop::EventLoopProxy<crate::browser::UserEvent>) {}
pub fn show_notification(_n: &crate::browser::WebNotification, _icon: Option<&std::path::Path>) {}
pub fn close_notification(_tab: crate::model::Id, _key: &str) {}
pub fn activate_app() {}
pub fn ask_notifications(_window: &Window, _site: &str, done: impl Fn(bool) + 'static) {
    done(false)
}
pub const NOTIFY_JS: &str = "";
pub fn set_traffic_lights(_window: &Window, _visible: bool) {}
pub fn match_page_background(_view: &WebView) {}
pub fn place_traffic_lights(_window: &Window) {}
pub fn copy_text(_text: &str) {}
pub fn share(_window: &Window, _url: &str, _rect: (f64, f64, f64, f64)) {}
pub fn install_key_fixes() {}
pub fn plain_text_input(_view: &WebView) {}
pub fn preconnect(_view: &WebView, _url: &str) {}
pub fn release(_view: &WebView) {}
pub fn clear_proxy(_store: Option<[u8; 16]>) {}
pub fn clear_cache(_store: Option<[u8; 16]>, done: impl Fn() + 'static) {
    done()
}
/// Without a system keychain, secrets go in an owner-only file in the profile.
pub fn get_secret(account: &str) -> Option<String> {
    std::fs::read_to_string(crate::profiles::dir(account).join(".ai-key")).ok().filter(|s| !s.is_empty())
}
pub fn set_secret(account: &str, value: &str) -> bool {
    std::fs::write(crate::profiles::dir(account).join(".ai-key"), value).is_ok()
}
pub fn set_window_background(_window: &Window) {}
pub fn snapshot(_view: &WebView, done: impl Fn(Option<Vec<u8>>) + 'static) {
    done(None)
}
pub fn is_playing_audio(_view: &WebView) -> bool {
    false
}
pub fn set_muted(_view: &WebView, _muted: bool) -> bool {
    false
}
/// WebView2 uses Windows Hello for passkeys.
pub fn passkeys_available() -> bool {
    true
}
pub fn is_busy(_view: &WebView) -> bool {
    false
}
/// Boosts aren't wired up for WebView2 yet.
pub fn set_page_scripts(_scripts: Vec<String>) {}
pub fn apply_page_scripts(_view: &WebView) {}
pub fn view_url(view: &WebView) -> Option<String> {
    view.url().ok()
}
pub fn web_process_pid(_view: &WebView) -> Option<i32> {
    None
}
pub fn attached(_view: &WebView, _window: &Window) -> bool {
    true
}
pub fn is_default_browser() -> Option<bool> {
    None
}
pub fn make_default_browser(_on_done: impl Fn() + 'static) -> bool {
    false
}

pub fn show_menu(window: &Window, menu: &Menu, x: f64, y: f64) {
    #[cfg(target_os = "windows")]
    {
        use muda::{ContextMenu, dpi::LogicalPosition};
        use tao::platform::windows::WindowExtWindows;
        unsafe { menu.show_context_menu_for_hwnd(window.hwnd() as _, Some(LogicalPosition::new(x, y).into())) };
    }
    #[cfg(not(target_os = "windows"))]
    let _ = (window, menu, x, y);
}

pub fn new_window(sh: &Rc<Shared>, _opener: Id, url: String, _: NewWindowFeatures) -> NewWindowResponse {
    let _ = sh.proxy.send_event(UserEvent::OpenTab(url));
    NewWindowResponse::Deny
}

/// WebView2 can drop most of a hidden tab's working set on request.
pub fn background(view: &WebView) {
    #[cfg(target_os = "windows")]
    {
        use wry::{MemoryUsageLevel, WebViewExtWindows};
        let _ = view.set_memory_usage_level(MemoryUsageLevel::Low);
    }
    #[cfg(not(target_os = "windows"))]
    let _ = view;
}

pub fn foreground(view: &WebView) {
    #[cfg(target_os = "windows")]
    {
        use wry::{MemoryUsageLevel, WebViewExtWindows};
        let _ = view.set_memory_usage_level(MemoryUsageLevel::Normal);
    }
    #[cfg(not(target_os = "windows"))]
    let _ = view;
}
