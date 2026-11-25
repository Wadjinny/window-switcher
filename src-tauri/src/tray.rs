//! The notification-area icon and its menu.

use tauri::{
    menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager, Runtime,
};

use crate::{
    hotkey::{app_shortcut, window_shortcut},
    settings_window::open_settings,
    update,
};

const TRAY_ID: &str = "switcher-tray";

fn tray_tooltip() -> String {
    format!(
        "Window Switcher {} — {} for windows, {} for apps",
        env!("CARGO_PKG_VERSION"),
        window_shortcut().hint(),
        app_shortcut().hint()
    )
}

pub(crate) fn refresh_tray_tooltip(app: &AppHandle) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Err(error) = tray.set_tooltip(Some(tray_tooltip())) {
            log::warn!("unable to update the tray tooltip: {error}");
        }
    }
}

/// The menu, with "Update to …" on top while a newer release is available.
fn tray_menu<R: Runtime, M: Manager<R>>(manager: &M) -> tauri::Result<Menu<R>> {
    let update = update::available_version()
        .map(|version| {
            MenuItem::with_id(
                manager,
                "update",
                format!("Update to {version} and restart"),
                true,
                None::<&str>,
            )
        })
        .transpose()?;
    let update_separator = PredefinedMenuItem::separator(manager)?;
    let settings = MenuItem::with_id(manager, "settings", "Settings…", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(manager)?;
    let quit = MenuItem::with_id(manager, "quit", "Quit Window Switcher", true, None::<&str>)?;
    let mut items: Vec<&dyn IsMenuItem<R>> = Vec::new();
    if let Some(update) = &update {
        items.push(update);
        items.push(&update_separator);
    }
    items.extend([&settings as &dyn IsMenuItem<R>, &separator, &quit]);
    Menu::with_items(manager, &items)
}

/// Rebuilds the menu after the update status changed.
pub(crate) fn refresh_tray_menu(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    match tray_menu(app) {
        Ok(menu) => {
            if let Err(error) = tray.set_menu(Some(menu)) {
                log::warn!("unable to update the tray menu: {error}");
            }
        }
        Err(error) => log::warn!("unable to build the tray menu: {error}"),
    }
}

pub(crate) fn install_tray(app: &tauri::App) -> tauri::Result<()> {
    let menu = tray_menu(app)?;
    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(tray_tooltip())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "update" => update::install_in_background(app, false),
            "settings" => open_settings(app),
            "quit" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}
