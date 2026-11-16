/// Every command the webviews may invoke. Declaring them makes Tauri
/// generate an `allow-<command>` permission for each and deny any command a
/// window's capability does not grant (see `capabilities/`).
const COMMANDS: &[&str] = &[
    // The switcher overlays.
    "switcher_snapshot",
    "switcher_grid",
    "switcher_click",
    "update_thumbnails",
    "overlay_timing",
    // The Settings window.
    "get_settings",
    "set_setting",
    "reset_hotkeys",
    "set_hotkey_recording",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
