//! "Run as administrator": whether this process is elevated, and restarting
//! it elevated through the UAC prompt.
//!
//! A non-elevated switcher cannot list, activate or send keys to the windows
//! of elevated programs (User Interface Privilege Isolation), and its keyboard
//! hook gets no keys while one of them is in front. An elevated one can.
//!
//! The elevated copy is started with `--replaces <pid>`: it waits for the
//! instance that launched it to exit before starting, so the single-instance
//! guard (a named mutex held by the running copy) lets it through.

use std::{env, ffi::OsString, os::windows::ffi::OsStrExt, sync::OnceLock};

use windows::{
    core::{HRESULT, PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, LocalFree, ERROR_CANCELLED, HANDLE, HLOCAL, HWND},
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenElevation, TokenUser,
            TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
        },
        System::{
            Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED},
            Threading::{
                GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
                WaitForSingleObject, PROCESS_SYNCHRONIZE,
            },
        },
        UI::{
            Shell::{
                ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS,
                SHELLEXECUTEINFOW,
            },
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
};

/// Given to the elevated copy: the process id of the instance it replaces.
const REPLACES_FLAG: &str = "--replaces";
/// Given to the elevated copy when the switch was made from Settings, so the
/// new instance opens Settings again where the user left it.
const OPEN_SETTINGS_FLAG: &str = "--open-settings";
/// How long the elevated copy waits for the old instance to exit.
const REPLACE_WAIT_MS: u32 = 10_000;

/// What this process was started with, as far as elevation is concerned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LaunchArgs {
    /// Started by an instance that asked for elevation and is now exiting.
    pub(crate) replaces: Option<u32>,
    /// Open the Settings window once started.
    pub(crate) open_settings: bool,
}

impl LaunchArgs {
    pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Self {
        let mut parsed = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg == REPLACES_FLAG {
                parsed.replaces = args
                    .next()
                    .and_then(|pid| pid.to_str()?.parse().ok())
                    .filter(|&pid| pid != 0);
            } else if arg == OPEN_SETTINGS_FLAG {
                parsed.open_settings = true;
            }
        }
        parsed
    }
}

/// The arguments given to the elevated copy.
fn relaunch_parameters(pid: u32, open_settings: bool) -> String {
    let mut parameters = format!("{REPLACES_FLAG} {pid}");
    if open_settings {
        parameters.push(' ');
        parameters.push_str(OPEN_SETTINGS_FLAG);
    }
    parameters
}

static ELEVATED: OnceLock<bool> = OnceLock::new();

/// True when this process has administrator rights (an elevated token). A
/// process's elevation never changes, so it is read once.
pub(crate) fn is_elevated() -> bool {
    *ELEVATED.get_or_init(|| {
        let mut elevation = TOKEN_ELEVATION::default();
        let read = with_process_token(|token| {
            let mut size = 0;
            // SAFETY: the buffer is a TOKEN_ELEVATION of the size given.
            unsafe {
                GetTokenInformation(
                    token,
                    TokenElevation,
                    Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
                    size_of::<TOKEN_ELEVATION>() as u32,
                    &mut size,
                )
            }
        });
        match read {
            Ok(()) => elevation.TokenIsElevated != 0,
            Err(error) => {
                log::warn!("unable to read this process's elevation: {error}");
                false
            }
        }
    })
}

/// The signed-in user's security identifier, e.g. `S-1-5-21-...`.
pub(crate) fn user_sid() -> windows::core::Result<String> {
    with_process_token(|token| {
        let mut size = 0;
        // SAFETY: the first call only reports the size needed; the second
        // writes at most `size` bytes into a buffer of that many bytes, which
        // is u64-aligned, as TOKEN_USER needs.
        unsafe {
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
            let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                size,
                &mut size,
            )?;
            let user = &*(buffer.as_ptr() as *const TOKEN_USER);
            let mut text = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut text)?;
            let sid = text.to_string();
            LocalFree(Some(HLOCAL(text.0.cast())));
            sid.map_err(|_| windows::core::Error::from(HRESULT(0x8007_000D_u32 as i32)))
        }
    })
}

fn with_process_token<T>(
    read: impl FnOnce(HANDLE) -> windows::core::Result<T>,
) -> windows::core::Result<T> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo handle needs no closing; the token handle is closed
    // below.
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let result = read(token);
        let _ = CloseHandle(token);
        result
    }
}

/// Why an elevated restart did not happen.
#[derive(Debug)]
pub(crate) enum RelaunchError {
    /// The user answered "No" to the UAC prompt (or closed it).
    Cancelled,
    Failed(String),
}

impl RelaunchError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Cancelled => {
                "Windows did not grant administrator rights, so Run as administrator stays off"
                    .into()
            }
            Self::Failed(reason) => format!("unable to restart as administrator: {reason}"),
        }
    }
}

/// Starts an elevated copy of this executable through the UAC prompt and
/// returns once it is running (or the user declined). The caller must exit
/// promptly: the new copy waits for this process before it starts.
///
/// `owner` is the window the prompt belongs to, so it comes to the front.
pub(crate) fn relaunch_elevated(
    owner: Option<HWND>,
    open_settings: bool,
) -> Result<(), RelaunchError> {
    let exe = env::current_exe().map_err(|error| RelaunchError::Failed(error.to_string()))?;
    // SAFETY: trivially safe.
    let pid = unsafe { GetCurrentProcessId() };
    let file = wide(exe.as_os_str());
    let parameters = wide(relaunch_parameters(pid, open_settings).as_ref());
    let directory = exe.parent().map(|directory| wide(directory.as_os_str()));
    let verb = wide("runas".as_ref());

    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // NOASYNC: the caller exits right after; FLAG_NO_UI: a declined
        // prompt is reported by the caller, not by an error box.
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        hwnd: owner.unwrap_or_default(),
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        lpDirectory: directory
            .as_ref()
            .map_or(PCWSTR::null(), |directory| PCWSTR(directory.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // ShellExecuteEx asks for COM on the calling thread.
    // SAFETY: balanced below when it succeeded; every string outlives the call.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let result = unsafe { ShellExecuteExW(&mut info) };
    if com {
        unsafe { CoUninitialize() };
    }
    match result {
        Ok(()) => {
            if !info.hProcess.is_invalid() {
                // SAFETY: SEE_MASK_NOCLOSEPROCESS hands the handle to us.
                let _ = unsafe { CloseHandle(info.hProcess) };
            }
            log::info!("started an elevated copy; this instance exits");
            Ok(())
        }
        Err(error) if error.code() == ERROR_CANCELLED.to_hresult() => {
            log::warn!("the UAC prompt was declined; still running without administrator rights");
            Err(RelaunchError::Cancelled)
        }
        Err(error) => {
            log::error!("unable to restart as administrator: {error}");
            Err(RelaunchError::Failed(error.message()))
        }
    }
}

/// In the elevated copy, before the single-instance guard runs: waits for
/// the instance that started it to exit and release the guard.
pub(crate) fn wait_for_replaced_instance(args: &LaunchArgs) {
    let Some(pid) = args.replaces else { return };
    // SAFETY: the handle is closed below. The old instance is still alive
    // when this runs (it only exits once ShellExecuteEx returned to it, after
    // this process was created), so the id cannot belong to another process.
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return; // Already gone.
        };
        WaitForSingleObject(process, REPLACE_WAIT_MS);
        let _ = CloseHandle(process);
    }
}

fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> LaunchArgs {
        LaunchArgs::parse(args.iter().map(OsString::from))
    }

    #[test]
    fn the_relaunch_parameters_parse_back() {
        assert_eq!(
            parse(&["exe", "--replaces", "4321"]),
            LaunchArgs {
                replaces: Some(4321),
                open_settings: false
            }
        );
        let parameters = relaunch_parameters(77, true);
        assert_eq!(parameters, "--replaces 77 --open-settings");
        let mut args = vec!["exe"];
        args.extend(parameters.split(' '));
        assert_eq!(
            parse(&args),
            LaunchArgs {
                replaces: Some(77),
                open_settings: true
            }
        );
    }

    #[test]
    fn an_ordinary_launch_has_no_relaunch_arguments() {
        assert_eq!(parse(&["exe"]), LaunchArgs::default());
        assert_eq!(parse(&["exe", "--replaces"]), LaunchArgs::default());
        assert_eq!(parse(&["exe", "--replaces", "x"]), LaunchArgs::default());
        assert_eq!(parse(&["exe", "--replaces", "0"]), LaunchArgs::default());
    }

    #[test]
    fn the_current_user_has_a_sid() {
        // S-1-5-21-... for local and domain accounts, S-1-12-1-... for Entra ID.
        assert!(user_sid().unwrap().starts_with("S-1-"));
    }
}
