//! The windows the switcher offers: enumerating and filtering them, grouping
//! them into apps, and bringing the chosen one to the front.

pub(crate) mod activate;
pub(crate) mod enumerate;
pub(crate) mod grouping;
pub(crate) mod identity;

use std::path::PathBuf;

use windows::Win32::Foundation::HWND;

#[derive(Clone)]
pub(crate) struct Target {
    pub(crate) hwnd: HWND,
    pub(crate) process_path: PathBuf,
    /// Explicit AppUserModelID for packaged apps and browser PWAs, when the
    /// window exposes one. It is what the taskbar groups by.
    pub(crate) app_id: Option<String>,
    pub(crate) title: String,
    /// Number of windows behind this card (always 1 for window mode).
    pub(crate) window_count: usize,
}

// `HWND` is an opaque, process-wide handle value. The switcher only stores it
// and hands it back to Win32 calls, so moving it between our threads is safe.
unsafe impl Send for Target {}
