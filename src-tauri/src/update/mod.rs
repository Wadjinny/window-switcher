//! Updates from GitHub Releases, for the portable exe.
//!
//! Checks the latest release shortly after startup and every 6 hours (unless
//! turned off in Settings), or when asked. A newer one is announced once per
//! version with a Windows notification, an "Update to …" tray item and the
//! Settings window's Updates section.
//!
//! Installing downloads the release's exe and its `.sha256` (both published
//! by the release workflow), checks the digest, then swaps the files: the
//! running exe is renamed to `<exe>.old` (Windows allows renaming a running
//! program, not deleting it), the new one takes its path, and it is started
//! with `--replaces <pid>` while this instance exits, the same hand-over
//! "Run as administrator" uses. The next start deletes the `.old` file. The
//! path stays the same, so shortcuts and the startup entry keep working.

mod http;
mod toast;

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use windows::{
    core::{w, PCWSTR},
    Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
    Win32::{System::Threading::GetCurrentProcessId, UI::Shell::ShellExecuteW},
};

use crate::{elevation::relaunch_parameters, show_error_message, tray::refresh_tray_menu};

const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/Wadjinny/window-switcher/releases/latest";
/// Release pages must be on the project's own repository.
const RELEASES_PAGE_PREFIX: &str = "https://github.com/Wadjinny/window-switcher/releases/";
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(20);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Sent to the Settings window whenever the status changes.
pub(crate) const STATUS_EVENT: &str = "update:status";

/// `true` while automatic checks are on (`check_for_updates` in config.json).
pub(crate) static AUTO_CHECK: AtomicBool = AtomicBool::new(true);

/// A version as `(major, minor, patch)`.
type Version = (u64, u64, u64);

/// `0.3.1` or `v0.3.1`. Pre-releases (`v0.4.0-beta`) and anything else are
/// `None`: only plain releases are offered.
fn parse_version(text: &str) -> Option<Version> {
    let mut parts = text.strip_prefix('v').unwrap_or(text).split('.');
    let mut next = || parts.next()?.parse::<u64>().ok();
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

fn current_version() -> Version {
    parse_version(env!("CARGO_PKG_VERSION")).unwrap_or_default()
}

/// The parts of a GitHub release this app uses.
#[derive(Clone, Debug, PartialEq)]
struct Release {
    version: Version,
    /// `0.3.1`, for display.
    name: String,
    page: String,
    exe_url: String,
    sha256_url: String,
}

/// Reads the GitHub API's release JSON. The release must carry the assets
/// the release workflow publishes: `Window-Switcher-<tag>-x64.exe` and its
/// `.sha256`.
fn parse_release(json: &serde_json::Value) -> Result<Release, String> {
    let tag = json["tag_name"].as_str().ok_or("the release has no tag")?;
    let version = parse_version(tag).ok_or_else(|| format!("unexpected release tag {tag:?}"))?;
    let page = json["html_url"]
        .as_str()
        .filter(|page| page.starts_with(RELEASES_PAGE_PREFIX))
        .unwrap_or("https://github.com/Wadjinny/window-switcher/releases/latest")
        .to_string();
    let exe_name = format!("Window-Switcher-{tag}-x64.exe");
    let sha256_name = format!("{exe_name}.sha256");
    let asset = |name: &str| {
        json["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|asset| asset["name"].as_str() == Some(name))
            .and_then(|asset| asset["browser_download_url"].as_str())
            .filter(|url| http::split_url(url).is_some())
            .map(str::to_string)
            .ok_or_else(|| format!("release {tag} has no {name}"))
    };
    Ok(Release {
        version,
        name: format!("{}.{}.{}", version.0, version.1, version.2),
        page,
        exe_url: asset(&exe_name)?,
        sha256_url: asset(&sha256_name)?,
    })
}

/// The digest from a `.sha256` file: `<64 hex digits>  <file name>`.
fn parse_sha256_file(text: &str) -> Option<[u8; 32]> {
    let hex = text.split_whitespace().next()?;
    if hex.len() != 64 {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(digest)
}

/// What the Settings window shows about updates.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Status {
    /// Not checked yet since startup.
    Idle,
    Checking,
    UpToDate {
        /// Unix time of the check, in seconds.
        checked_at: u64,
    },
    Available {
        version: String,
    },
    Installing {
        version: String,
    },
    Failed {
        message: String,
    },
}

struct State {
    status: Status,
    /// The newer release found by the last check, if any.
    release: Option<Release>,
    /// The version the notification was last shown for, so each new version
    /// is announced once.
    notified: Option<Version>,
}

static STATE: Mutex<State> = Mutex::new(State {
    status: Status::Idle,
    release: None,
    notified: None,
});

#[derive(Clone, Serialize)]
pub(crate) struct UpdatePayload {
    current: &'static str,
    auto_check: bool,
    /// The newer version found, even after a failed install: it can be
    /// tried again.
    available: Option<String>,
    #[serde(flatten)]
    status: Status,
}

pub(crate) fn payload() -> UpdatePayload {
    let (status, available) = STATE
        .lock()
        .map(|state| {
            (
                state.status.clone(),
                state.release.as_ref().map(|release| release.name.clone()),
            )
        })
        .unwrap_or((Status::Idle, None));
    UpdatePayload {
        current: env!("CARGO_PKG_VERSION"),
        auto_check: AUTO_CHECK.load(Ordering::SeqCst),
        available,
        status,
    }
}

/// The version an "Update to …" item should offer, if any.
pub(crate) fn available_version() -> Option<String> {
    let state = STATE.lock().ok()?;
    match &state.status {
        Status::Installing { .. } | Status::Checking => None,
        _ => state.release.as_ref().map(|release| release.name.clone()),
    }
}

fn set_status(app: &AppHandle, status: Status) {
    if let Ok(mut state) = STATE.lock() {
        state.status = status;
    }
    let _ = app.emit(STATUS_EVENT, payload());
    refresh_tray_menu(app);
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Starts the automatic checks, and deletes what a previous update left.
pub(crate) fn start(app: &AppHandle) {
    remove_leftovers();
    let app = app.clone();
    let spawned = thread::Builder::new()
        .name("switcher-updates".into())
        .spawn(move || {
            thread::sleep(FIRST_CHECK_DELAY);
            loop {
                if AUTO_CHECK.load(Ordering::SeqCst) {
                    check(&app, false);
                }
                thread::sleep(CHECK_INTERVAL);
            }
        });
    if let Err(error) = spawned {
        log::warn!("unable to start the update checks: {error}");
    }
}

/// Asks GitHub for the latest release. A newer one is announced with a
/// notification the first time it is seen, unless `manual` (the user is
/// looking at the result already). Blocking: call it off the main thread.
pub(crate) fn check(app: &AppHandle, manual: bool) {
    let busy = STATE
        .lock()
        .is_ok_and(|state| matches!(state.status, Status::Checking | Status::Installing { .. }));
    if busy {
        return;
    }
    let previous = STATE.lock().map(|state| state.status.clone()).ok();
    set_status(app, Status::Checking);
    let latest = http::get(LATEST_RELEASE_URL, Some("application/vnd.github+json"))
        .and_then(|body| serde_json::from_slice(&body).map_err(|error| error.to_string()))
        .and_then(|json: serde_json::Value| parse_release(&json));
    match latest {
        Ok(release) if release.version > current_version() => {
            log::info!("update available: {}", release.name);
            let announce = {
                let mut state = match STATE.lock() {
                    Ok(state) => state,
                    Err(_) => return,
                };
                let announce = !manual && state.notified != Some(release.version);
                state.notified = Some(release.version);
                state.release = Some(release.clone());
                announce
            };
            set_status(
                app,
                Status::Available {
                    version: release.name.clone(),
                },
            );
            if announce {
                if let Err(error) = toast::show_update_toast(app, &release.name) {
                    log::warn!("unable to show the update notification: {error}");
                }
            }
        }
        Ok(release) => {
            log::info!("up to date (latest release {})", release.name);
            if let Ok(mut state) = STATE.lock() {
                state.release = None;
            }
            set_status(
                app,
                Status::UpToDate {
                    checked_at: unix_now(),
                },
            );
        }
        Err(error) => {
            log::warn!("unable to check for updates: {error}");
            // A failed background check keeps an update already found.
            match previous {
                Some(status @ Status::Available { .. }) if !manual => set_status(app, status),
                _ => set_status(
                    app,
                    Status::Failed {
                        message: format!("Could not check for updates: {error}"),
                    },
                ),
            }
        }
    }
}

/// Installs the available update on a worker thread, then restarts. On
/// failure the Settings window shows why; started from the tray or a
/// notification, a message box does.
pub(crate) fn install_in_background(app: &AppHandle, from_settings: bool) {
    let app = app.clone();
    let spawned = thread::Builder::new()
        .name("switcher-update-install".into())
        .spawn(move || {
            if let Err(error) = install(&app, from_settings) {
                log::error!("update failed: {error}");
                set_status(
                    &app,
                    Status::Failed {
                        message: format!("The update failed: {error}"),
                    },
                );
                if !from_settings {
                    show_error_message(&format!("Window Switcher could not update: {error}"));
                }
            }
        });
    if let Err(error) = spawned {
        log::error!("unable to start the update: {error}");
    }
}

fn install(app: &AppHandle, reopen_settings: bool) -> Result<(), String> {
    let release = {
        let state = STATE
            .lock()
            .map_err(|_| "the update state is unavailable")?;
        if matches!(state.status, Status::Installing { .. }) {
            return Ok(());
        }
        state.release.clone().ok_or("no update is available")?
    };
    set_status(
        app,
        Status::Installing {
            version: release.name.clone(),
        },
    );
    log::info!("downloading {}", release.exe_url);
    let expected = http::get(&release.sha256_url, None)
        .ok()
        .and_then(|body| parse_sha256_file(&String::from_utf8_lossy(&body)))
        .ok_or("the release's checksum could not be read")?;
    let exe = http::get(&release.exe_url, None)?;
    if http::sha256(&exe)? != expected {
        return Err("the download does not match the release's checksum".into());
    }

    let current = env::current_exe().map_err(|error| error.to_string())?;
    let (staged, old) = side_files(&current);
    fs::write(&staged, &exe)
        .map_err(|error| format!("unable to save {}: {error}", staged.display()))?;
    let _ = fs::remove_file(&old);
    if let Err(error) = fs::rename(&current, &old) {
        let _ = fs::remove_file(&staged);
        return Err(format!("unable to move {}: {error}", current.display()));
    }
    if let Err(error) = fs::rename(&staged, &current) {
        let _ = fs::rename(&old, &current);
        let _ = fs::remove_file(&staged);
        return Err(format!("unable to replace {}: {error}", current.display()));
    }

    // SAFETY: trivially safe.
    let pid = unsafe { GetCurrentProcessId() };
    let arguments = relaunch_parameters(pid, reopen_settings);
    let started = Command::new(&current)
        .args(arguments.split(' '))
        .current_dir(current.parent().unwrap_or(Path::new(".")))
        .spawn();
    if let Err(error) = started {
        // Put the running version back, so the next start is not a surprise.
        let _ = fs::remove_file(&current);
        let _ = fs::rename(&old, &current);
        return Err(format!("unable to start the new version: {error}"));
    }
    log::info!("updated to {}; restarting", release.name);
    // The new instance waits for this one to exit (see `--replaces`).
    app.exit(0);
    Ok(())
}

/// Where the download is saved before the swap, and where the replaced exe
/// goes: next to the exe, so the renames never cross volumes.
fn side_files(exe: &Path) -> (PathBuf, PathBuf) {
    let name = exe.file_name().unwrap_or_default().to_string_lossy();
    (
        exe.with_file_name(format!("{name}.new")),
        exe.with_file_name(format!("{name}.old")),
    )
}

/// Deletes the exe the last update replaced, and any unfinished download.
fn remove_leftovers() {
    let Ok(exe) = env::current_exe() else { return };
    let (staged, old) = side_files(&exe);
    for leftover in [old, staged] {
        if leftover.exists() {
            match fs::remove_file(&leftover) {
                Ok(()) => log::info!("removed {}", leftover.display()),
                Err(error) => log::warn!("unable to remove {}: {error}", leftover.display()),
            }
        }
    }
}

/// Opens the available release's page (or the latest one's) in the browser.
pub(crate) fn open_release_page() {
    let page = STATE
        .lock()
        .ok()
        .and_then(|state| state.release.as_ref().map(|release| release.page.clone()))
        .unwrap_or_else(|| format!("{RELEASES_PAGE_PREFIX}latest"));
    let page: Vec<u16> = page.encode_utf16().chain(Some(0)).collect();
    // SAFETY: the strings outlive the call.
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(page.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions_parse_and_compare() {
        assert_eq!(parse_version("v0.3.1"), Some((0, 3, 1)));
        assert_eq!(parse_version("1.10.0"), Some((1, 10, 0)));
        assert!(parse_version("v0.10.0") > parse_version("v0.9.9"));
        assert_eq!(parse_version("v0.4.0-beta"), None);
        assert_eq!(parse_version("v0.4"), None);
        assert_eq!(parse_version("v0.4.0.1"), None);
        assert_eq!(parse_version("latest"), None);
        assert_ne!(current_version(), (0, 0, 0));
    }

    fn release_json(tag: &str, assets: &[&str]) -> serde_json::Value {
        json!({
            "tag_name": tag,
            "html_url": format!("{RELEASES_PAGE_PREFIX}tag/{tag}"),
            "assets": assets.iter().map(|name| json!({
                "name": name,
                "browser_download_url": format!("https://github.com/dl/{name}"),
            })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn a_release_needs_its_exe_and_checksum() {
        let release = parse_release(&release_json(
            "v0.4.0",
            &[
                "Window-Switcher-v0.4.0-x64.exe",
                "Window-Switcher-v0.4.0-x64.exe.sha256",
            ],
        ))
        .unwrap();
        assert_eq!(release.version, (0, 4, 0));
        assert_eq!(release.name, "0.4.0");
        assert_eq!(
            release.exe_url,
            "https://github.com/dl/Window-Switcher-v0.4.0-x64.exe"
        );
        assert!(release.sha256_url.ends_with(".exe.sha256"));
        assert!(release.page.ends_with("/tag/v0.4.0"));

        assert!(
            parse_release(&release_json("v0.4.0", &["Window-Switcher-v0.4.0-x64.exe"])).is_err()
        );
        assert!(parse_release(&release_json("nightly", &[])).is_err());
    }

    #[test]
    fn checksum_files_parse() {
        let hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let digest =
            parse_sha256_file(&format!("{hex}  Window-Switcher-v0.4.0-x64.exe\r\n")).unwrap();
        assert_eq!(digest, http::sha256(b"abc").unwrap());
        assert_eq!(parse_sha256_file("abc  file"), None);
        assert_eq!(parse_sha256_file(""), None);
        assert_eq!(parse_sha256_file(&format!("{}zz  f", &hex[..62])), None);
    }

    #[test]
    fn side_files_sit_next_to_the_exe() {
        let (staged, old) = side_files(Path::new(r"C:\Apps\Window Switcher.exe"));
        assert_eq!(staged, Path::new(r"C:\Apps\Window Switcher.exe.new"));
        assert_eq!(old, Path::new(r"C:\Apps\Window Switcher.exe.old"));
    }
}
