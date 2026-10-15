//! Native menu bar. On macOS the Edit menu is also what makes ⌘C/⌘V work inside web views.

use muda::{
    AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu,
    accelerator::Accelerator,
};

fn item(id: &str, text: &str, accel: Option<&str>) -> MenuItem {
    let accel = accel.map(|a| a.parse::<Accelerator>().unwrap_or_else(|e| panic!("bad accelerator {a}: {e}")));
    MenuItem::with_id(id, text, true, accel)
}

pub fn build() -> Menu {
    let menu = Menu::new();
    let sep = PredefinedMenuItem::separator;

    #[cfg(target_os = "macos")]
    {
        let about = AboutMetadata {
            name: Some("rzen".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            ..Default::default()
        };
        let app = Submenu::with_items(
            "rzen",
            true,
            &[
                &PredefinedMenuItem::about(None, Some(about)),
                &sep(),
                &item("settings", "Settings…", Some("CmdOrCtrl+,")),
                &sep(),
                &PredefinedMenuItem::services(None),
                &sep(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &sep(),
                &PredefinedMenuItem::quit(None),
            ],
        )
        .unwrap();
        menu.append(&app).unwrap();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = AboutMetadata::default();

    let file = Submenu::with_items(
        "File",
        true,
        &[
            &item("new_tab", "New Tab", Some("CmdOrCtrl+T")),
            &item("new_ws", "New Space", Some("CmdOrCtrl+Shift+N")),
            &item("private_window", "New Private Window", Some("CmdOrCtrl+Shift+P")),
            &item("new_profile", "New Profile…", None),
            &item("lock", "Lock Profile", Some("CmdOrCtrl+Ctrl+L")),
            &item("open_location", "Open Location…", Some("CmdOrCtrl+L")),
            &item("reopen", "Reopen Closed Tab", Some("CmdOrCtrl+Shift+T")),
            &sep(),
            &item("close_tab", "Close Tab", Some("CmdOrCtrl+W")),
            &sep(),
            &item("print", "Print…", Some("CmdOrCtrl+P")),
        ],
    )
    .unwrap();

    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &sep(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
            &sep(),
            &item("copy_url", "Copy Page Link", Some("CmdOrCtrl+Shift+C")),
            &sep(),
            &item("find", "Find…", Some("CmdOrCtrl+F")),
            &item("find_next", "Find Next", Some("CmdOrCtrl+G")),
            &item("find_prev", "Find Previous", Some("CmdOrCtrl+Shift+G")),
        ],
    )
    .unwrap();

    let view = Submenu::with_items(
        "View",
        true,
        &[
            &item("toggle_sidebar", "Toggle Sidebar", Some("CmdOrCtrl+S")),
            &item("toggle_ai", "Toggle AI Panel", Some("CmdOrCtrl+Shift+A")),
            &sep(),
            &item("tab_switcher", "Go to Tab…", Some("Ctrl+P")),
            &item("command_palette", "Command Palette…", Some("Ctrl+Shift+P")),
            &sep(),
            &item("reload", "Reload Page", Some("CmdOrCtrl+R")),
            &item("stop", "Stop Loading", Some("CmdOrCtrl+.")),
            &sep(),
            &item("zoom_in", "Zoom In", Some("CmdOrCtrl+=")),
            &item("zoom_out", "Zoom Out", Some("CmdOrCtrl+-")),
            &item("zoom_reset", "Actual Size", Some("CmdOrCtrl+0")),
            &sep(),
            &item("devtools", "Web Inspector", Some("CmdOrCtrl+Alt+I")),
            &sep(),
            &PredefinedMenuItem::fullscreen(None),
        ],
    )
    .unwrap();

    let history = Submenu::with_items(
        "History",
        true,
        &[&item("back", "Back", Some("CmdOrCtrl+[")), &item("forward", "Forward", Some("CmdOrCtrl+]"))],
    )
    .unwrap();

    let tabs = Submenu::new("Tabs", true);
    tabs.append_items(&[
        &item("next_tab", "Next Tab", Some("Ctrl+Tab")),
        &item("prev_tab", "Previous Tab", Some("Ctrl+Shift+Tab")),
        &sep(),
        &item("next_ws", "Next Workspace", Some("Ctrl+Alt+Right")),
        &item("prev_ws", "Previous Workspace", Some("Ctrl+Alt+Left")),
        &sep(),
        &item("pin_tab", "Pin / Unpin Tab", None),
        &item("unload_tab", "Unload Tab (free memory)", None),
        &sep(),
    ])
    .unwrap();
    for n in 1..=8 {
        tabs.append(&item(&format!("tab_{n}"), &format!("Tab {n}"), Some(&format!("CmdOrCtrl+{n}")))).unwrap();
    }
    tabs.append(&item("tab_last", "Last Tab", Some("CmdOrCtrl+9"))).unwrap();

    let window = Submenu::with_items(
        "Window",
        true,
        &[&PredefinedMenuItem::minimize(None), &PredefinedMenuItem::maximize(None)],
    )
    .unwrap();

    menu.append_items(&[&file, &edit, &view, &history, &tabs, &window]).unwrap();
    #[cfg(target_os = "macos")]
    window.set_as_windows_menu_for_nsapp();
    menu
}
