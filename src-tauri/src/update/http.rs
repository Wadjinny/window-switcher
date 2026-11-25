//! HTTPS downloads through WinHTTP, Windows' own HTTP stack: it uses the
//! system proxy and the Windows certificate store, so it works behind the
//! same corporate proxies and TLS inspection the browser does. And SHA-256
//! through Windows' CNG, to check what was downloaded.

use std::{ffi::c_void, ptr};

use windows::{
    core::{w, PCWSTR},
    Win32::{
        Networking::WinHttp::{
            WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
            WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData,
            WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
            INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
            WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
        },
        Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE},
    },
};

/// Nothing this app downloads comes near this; a bigger body is refused
/// rather than held in memory.
const MAX_BODY: usize = 128 * 1024 * 1024;

/// A WinHTTP handle, closed when dropped.
struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void) -> Result<Self, String> {
        if raw.is_null() {
            Err(windows::core::Error::from_thread().message())
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from WinHTTP and is closed only here.
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

/// `https://host/path?query` split into its host and the rest (at least
/// `/`). Only HTTPS on the default port: every URL this app fetches is one.
pub(crate) fn split_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = match rest.find('/') {
        Some(slash) => rest.split_at(slash),
        None => (rest, "/"),
    };
    (!host.is_empty() && !host.contains([':', '@', '?', '#'])).then_some((host, path))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// GETs `url` and returns its body; any status but 200 is an error.
/// Redirects (GitHub's downloads go through one) are followed.
pub(crate) fn get(url: &str, accept: Option<&str>) -> Result<Vec<u8>, String> {
    let (host, path) = split_url(url).ok_or_else(|| format!("not an https URL: {url}"))?;
    let agent = wide(concat!("WindowSwitcher/", env!("CARGO_PKG_VERSION")));
    let host = wide(host);
    let path = wide(path);
    // SAFETY: every string outlives the calls using it, and every handle is
    // closed by `Handle` (the request, then the connection, then the session).
    unsafe {
        let session = Handle::new(WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))?;
        let _ = WinHttpSetTimeouts(session.0, 15_000, 15_000, 30_000, 60_000);
        let connection = Handle::new(WinHttpConnect(
            session.0,
            PCWSTR(host.as_ptr()),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        ))?;
        let request = Handle::new(WinHttpOpenRequest(
            connection.0,
            w!("GET"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            ptr::null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        let headers: Option<Vec<u16>> =
            accept.map(|accept| format!("Accept: {accept}\r\n").encode_utf16().collect());
        WinHttpSendRequest(request.0, headers.as_deref(), None, 0, 0, 0)
            .map_err(|error| error.message())?;
        WinHttpReceiveResponse(request.0, ptr::null_mut()).map_err(|error| error.message())?;

        let mut status = 0u32;
        let mut size = size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&mut status as *mut u32).cast()),
            &mut size,
            ptr::null_mut(),
        )
        .map_err(|error| error.message())?;
        if status != 200 {
            return Err(format!("the server answered HTTP {status}"));
        }

        let mut body = Vec::new();
        loop {
            let mut available = 0u32;
            WinHttpQueryDataAvailable(request.0, &mut available)
                .map_err(|error| error.message())?;
            if available == 0 {
                break;
            }
            let start = body.len();
            if start + available as usize > MAX_BODY {
                return Err("the download is unexpectedly large".into());
            }
            body.resize(start + available as usize, 0);
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                body[start..].as_mut_ptr().cast(),
                available,
                &mut read,
            )
            .map_err(|error| error.message())?;
            body.truncate(start + read as usize);
            if read == 0 {
                break;
            }
        }
        Ok(body)
    }
}

/// The SHA-256 digest of `data`.
pub(crate) fn sha256(data: &[u8]) -> Result<[u8; 32], String> {
    let mut digest = [0u8; 32];
    // SAFETY: the pseudo handle needs no opening or closing (Windows 10+).
    unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut digest) }
        .ok()
        .map_err(|error| format!("unable to hash the download: {}", error.message()))?;
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_into_host_and_path() {
        assert_eq!(
            split_url("https://api.github.com/repos/a/b/releases/latest"),
            Some(("api.github.com", "/repos/a/b/releases/latest"))
        );
        assert_eq!(split_url("https://example.com"), Some(("example.com", "/")));
        assert_eq!(split_url("http://example.com/"), None);
        assert_eq!(split_url("https://example.com:8443/"), None);
        assert_eq!(split_url("https:///path"), None);
    }

    #[test]
    fn sha256_matches_a_known_digest() {
        let digest = sha256(b"abc").unwrap();
        assert_eq!(
            digest[..4],
            [0xba, 0x78, 0x16, 0xbf],
            "SHA-256(\"abc\") starts with ba7816bf"
        );
    }
}
