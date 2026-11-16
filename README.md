# Window Switcher

A Windows 11-style switcher built with Rust and Tauri. It replaces the two AutoHotkey scripts with one native process and its own overlay - no temporary taskbar edits, no Alt+Tab list manipulation, and no window-style changes.

## Shortcuts

- `` Alt+` `` (and the matching French `Alt+²` key): cycle windows belonging to the current application.
- `Shift` reverses the direction.
- `Win+Tab`: cycle application groups, one card per application identity.
- While the switcher is open, `←` / `→` move to the previous / next item (wrapping around), and `↑` / `↓` move between rows of the grid. In the vertical layout it is the other way round: `↑` / `↓` step through the list, and `←` / `→` move between its columns.
- Release `Alt` / `Win` to activate the selected item; press `Esc` to cancel. Clicking an item activates it right away; hovering only highlights it.

Shortcuts are matched by **physical key position** (virtual key plus scan code), so `` Alt+` `` works on both QWERTY (backtick) and AZERTY (`²`) layouts.

## Grouping: one card per app identity

Cards are grouped by **AppUserModelID (AUMID)** whenever a window exposes one, and by executable path otherwise:

- Packaged/UWP apps and Start-menu apps are grouped by their AUMID.
- Browser PWAs (for example a Chrome "install app" shortcut) have their own AUMID, so each PWA appears as its own card instead of being merged into Chrome.
- Regular Chrome windows share Chrome's identity and group together.
- Classic Win32 programs without an AUMID group by full executable path (so two copies of the same app installed in different folders are separate cards).

This mirrors how the Windows taskbar groups windows and is why elevated, packaged, and PWA windows can be switched without touching their window styles.

## Tray icon and quitting

Click the tray icon for **Quit Window Switcher**. The overlay itself has no decorations and no taskbar entry, and it is configured as not closable (`closable: false`), so the tray menu is the supported way to exit.

## Single instance

Only one copy runs. Launching `Window Switcher.exe` again while it is already running exits immediately instead of installing a second tray icon and a duplicate keyboard hook.

## Non-activating overlay

The overlay is intentionally non-activating (`focusable: false`). The original target window stays focused until selection, which makes modifier-release switching reliable. The window is also transparent, always on top, hidden from the taskbar, and uses `noRedirectionBitmap` to avoid a white flash before the webview paints. Its `center: true` / fixed width only affect the very first frame; the Rust side repositions and resizes it per monitor on every show.

## Why this avoids the AHK failure mode

The AutoHotkey window switcher temporarily removed unrelated entries from Windows' own Alt+Tab/taskbar model. That cannot handle elevated and packaged windows consistently. This project instead:

1. Enumerates eligible top-level windows using the Win32 API.
2. Uses one eligibility policy for both switchers.
3. Displays its own Tauri overlay.
4. Activates the selected `HWND` directly.

It never hides another app, edits its taskbar registration, or changes its window styles.

## Run (development)

```powershell
git clone https://github.com/Wadjinny/window-switcher.git
cd window-switcher
npm install
npm run tauri dev
```

Run this from a **Developer PowerShell for VS 2022** (or after calling
`vcvars64.bat`). A `link.exe` from another toolchain on your `PATH` (for
example coreutils) otherwise shadows the MSVC linker and the build fails.

Building a Tauri Windows application requires the **Desktop development with C++** workload and a Windows SDK from Visual Studio Build Tools. The Windows SDK must include its `Lib` directory (for example, `kernel32.lib`).

Run the app elevated if you also need to enumerate and activate elevated windows such as Task Manager.

## Release build (portable)

```powershell
npm run tauri build   # or: npm run release
```

The executable is written to `src-tauri\target\release\window-switcher.exe`. It is a single portable file (bundling is disabled), so it can be copied anywhere and run directly.

To keep a stable copy that does not change when you rebuild:

```powershell
npm run install:local
```

That stops any running `Window Switcher` process, copies the release exe to `%LOCALAPPDATA%\Programs\Window Switcher\Window Switcher.exe`, and prints the resulting path. It does not create shortcuts or startup entries.

Notes:

- The exe is **unsigned**, so Windows SmartScreen may warn on first launch ("Windows protected your PC" → *More info* → *Run anyway*).
- It requires the **WebView2 runtime**, which is present by default on Windows 11. On older Windows 10 machines you may need to install the Evergreen runtime.
- `npm run install:local` fails with a clear message if the release build is missing; run the build first.

## Download

Pre-built portable executables are attached to each [GitHub release](../../releases). Download `Window-Switcher-<version>-x64.exe` and run it; the `.sha256` file next to it lets you verify the download.

## CI and releases

- **CI** (`.github/workflows/ci.yml`) runs on every push and pull request: type check, Vitest, `cargo clippy` and `cargo test` on Windows.
- **Release** (`.github/workflows/release.yml`) runs when a `v*` tag is pushed, or manually from the Actions tab. It runs the tests, builds the portable exe and publishes a GitHub release with the exe and its SHA-256.

To cut a release:

```powershell
git tag v0.1.0
git push origin v0.1.0
```

## Configuration

On first launch it creates:

`%APPDATA%\Window Switcher\config.json`

```json
{
  "window_hotkey": "Alt+Backtick",
  "app_hotkey": "Win+Tab"
}
```

Both values accept a modifier and a key joined by `+`. Matching is case-insensitive and spaces are ignored.

| Part | Accepted values |
| --- | --- |
| Modifier | `Alt`, `Win` (also `Windows`) |
| Key | `Tab`, `Backtick`, `` ` ``, `Oem3`, `²`, `Oem8` |

`Backtick`, `` ` ``, `Oem3`, `²`, and `Oem8` are aliases for the same physical key (scan code `0x29`), so the setting keeps working when the keyboard layout changes. Anything unrecognised falls back to the defaults shown above.

The easiest way to change them is the tray menu's **Settings…** window: click a shortcut, then hold Alt or Win and press Tab or the backtick key (Esc cancels). It rejects combinations the hook cannot use (Ctrl, Shift, other keys, the same shortcut twice), applies the new hotkeys immediately and writes them here. **Reset to defaults** restores the values above.

If you edit the file by hand instead, restart the application - the file is read once at startup.

## License

[MIT](LICENSE)
