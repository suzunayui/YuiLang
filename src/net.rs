//! Requests over HTTP and HTTPS for programs run by the interpreter.
//! (`yui build` has its own version, written in YuiLang: see `jit/runtime.yui`.)
//!
//! A request runs in the background: `start` gives a number for it, `done`
//! says whether it has finished, and `take` hands over what came back. So a
//! game keeps drawing while it waits. Only Windows so far, where the system
//! does the work (WinHTTP); there are no external crates.

use std::cell::RefCell;
use std::sync::mpsc::{channel, Receiver, TryRecvError};

/// How many requests can be under way at once.
const SLOTS: usize = 8;

enum Slot {
    Free,
    Waiting(Receiver<Outcome>),
    Finished(Outcome),
}

/// The status and the body, or the system's error code.
type Outcome = Result<(u32, Vec<u8>), u32>;

thread_local! {
    static REQUESTS: RefCell<Vec<Slot>> = RefCell::new((0..SLOTS).map(|_| Slot::Free).collect());
}

/// Starts a request and gives its number (1 and up), or 0 if too many are
/// under way already.
pub fn start(method: &str, url: &str, headers: &[String], body: &str) -> i64 {
    REQUESTS.with(|r| {
        let mut slots = r.borrow_mut();
        let Some(index) = slots.iter().position(|slot| matches!(slot, Slot::Free)) else { return 0 };
        let (sender, receiver) = channel();
        let (method, url, headers, body) = (method.to_string(), url.to_string(), headers.join("\r\n"), body.as_bytes().to_vec());
        std::thread::spawn(move || {
            let _ = sender.send(os::request(&method, &url, &headers, &body));
        });
        slots[index] = Slot::Waiting(receiver);
        index as i64 + 1
    })
}

/// Whether the request has finished. (True for a number that names none.)
pub fn done(id: i64) -> bool {
    REQUESTS.with(|r| {
        let mut slots = r.borrow_mut();
        let Some(slot) = usize::try_from(id - 1).ok().and_then(|index| slots.get_mut(index)) else { return true };
        if let Slot::Waiting(receiver) = slot {
            match receiver.try_recv() {
                Ok(outcome) => *slot = Slot::Finished(outcome),
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => *slot = Slot::Finished(Err(0)),
            }
        }
        true
    })
}

/// What came back, once: the body if the server said 2xx, a message otherwise.
/// The same texts as `http_take` in `runtime.yui`.
pub fn take(id: i64) -> Result<String, String> {
    if !done(id) {
        return Err("the request has not finished".to_string());
    }
    REQUESTS.with(|r| {
        let mut slots = r.borrow_mut();
        let Some(slot) = usize::try_from(id - 1).ok().and_then(|index| slots.get_mut(index)) else {
            return Err("no such request".to_string());
        };
        match std::mem::replace(slot, Slot::Free) {
            Slot::Finished(Ok((status, body))) => match String::from_utf8(body) {
                Err(_) => Err("the response was not valid UTF-8".to_string()),
                Ok(text) if (200..300).contains(&status) => Ok(text),
                Ok(text) => Err(format!("HTTP {status}: {text}")),
            },
            Slot::Finished(Err(code)) => Err(format!("the request failed (os error {code})")),
            _ => Err("no such request".to_string()),
        }
    })
}

#[cfg(not(windows))]
mod os {
    /// 50 is what Windows calls "not supported".
    pub fn request(_method: &str, _url: &str, _headers: &str, _body: &[u8]) -> super::Outcome {
        Err(50)
    }
}

#[cfg(windows)]
mod os {
    use std::ffi::c_void;

    #[repr(C)]
    struct UrlParts {
        size: u32,
        scheme: *const u16,
        scheme_length: u32,
        scheme_kind: i32,
        host: *const u16,
        host_length: u32,
        port: u16,
        user: *const u16,
        user_length: u32,
        password: *const u16,
        password_length: u32,
        path: *const u16,
        path_length: u32,
        extra: *const u16,
        extra_length: u32,
    }

    #[link(name = "winhttp")]
    extern "system" {
        fn WinHttpOpen(agent: *const u16, access: u32, proxy: *const u16, bypass: *const u16, flags: u32) -> isize;
        fn WinHttpSetTimeouts(handle: isize, resolve: i32, connect: i32, send: i32, receive: i32) -> i32;
        fn WinHttpCrackUrl(url: *const u16, length: u32, flags: u32, parts: *mut UrlParts) -> i32;
        fn WinHttpConnect(session: isize, host: *const u16, port: u16, reserved: u32) -> isize;
        fn WinHttpOpenRequest(
            connection: isize,
            verb: *const u16,
            path: *const u16,
            version: *const u16,
            referrer: *const u16,
            accept: *const *const u16,
            flags: u32,
        ) -> isize;
        fn WinHttpAddRequestHeaders(request: isize, headers: *const u16, length: u32, modifiers: u32) -> i32;
        fn WinHttpSendRequest(
            request: isize,
            headers: *const u16,
            headers_length: u32,
            body: *const c_void,
            body_length: u32,
            total_length: u32,
            context: usize,
        ) -> i32;
        fn WinHttpReceiveResponse(request: isize, reserved: *mut c_void) -> i32;
        fn WinHttpQueryHeaders(
            request: isize,
            info: u32,
            name: *const u16,
            buffer: *mut c_void,
            length: *mut u32,
            index: *mut u32,
        ) -> i32;
        fn WinHttpQueryDataAvailable(request: isize, available: *mut u32) -> i32;
        fn WinHttpReadData(request: isize, buffer: *mut c_void, length: u32, read: *mut u32) -> i32;
        fn WinHttpCloseHandle(handle: isize) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLastError() -> u32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Closes the handles it was given when it goes out of scope.
    struct Handles(Vec<isize>);

    impl Drop for Handles {
        fn drop(&mut self) {
            for handle in self.0.iter().rev() {
                // Safety: handles this module opened, closed once.
                unsafe {
                    WinHttpCloseHandle(*handle);
                }
            }
        }
    }

    pub fn request(method: &str, url: &str, headers: &str, body: &[u8]) -> super::Outcome {
        let (agent, wide_url, verb, wide_headers) = (wide("YuiLang"), wide(url), wide(method), wide(headers));
        // Safety: the structures are laid out as Windows expects, and every
        // pointer stays valid for the call it is given to.
        unsafe {
            let failed = || Err(GetLastError());
            let mut parts: UrlParts = std::mem::zeroed();
            parts.size = std::mem::size_of::<UrlParts>() as u32;
            (parts.scheme_length, parts.host_length, parts.path_length, parts.extra_length) = (u32::MAX, u32::MAX, u32::MAX, u32::MAX);
            if WinHttpCrackUrl(wide_url.as_ptr(), 0, 0, &mut parts) == 0 {
                return failed();
            }
            let mut host = std::slice::from_raw_parts(parts.host, parts.host_length as usize).to_vec();
            host.push(0);
            let mut handles = Handles(Vec::new());
            // 4 = the proxy the system is set up with.
            let session = WinHttpOpen(agent.as_ptr(), 4, std::ptr::null(), std::ptr::null(), 0);
            if session == 0 {
                return failed();
            }
            handles.0.push(session);
            // An answer may take minutes to start arriving (a language model thinking).
            WinHttpSetTimeouts(session, 0, 60_000, 60_000, 600_000);
            let connection = WinHttpConnect(session, host.as_ptr(), parts.port, 0);
            if connection == 0 {
                return failed();
            }
            handles.0.push(connection);
            let path = if *parts.path == 0 { std::ptr::null() } else { parts.path };
            let secure = if parts.scheme_kind == 2 { 0x0080_0000 } else { 0 };
            let request = WinHttpOpenRequest(connection, verb.as_ptr(), path, std::ptr::null(), std::ptr::null(), std::ptr::null(), secure);
            if request == 0 {
                return failed();
            }
            handles.0.push(request);
            if !headers.is_empty() && WinHttpAddRequestHeaders(request, wide_headers.as_ptr(), u32::MAX, 0x2000_0000) == 0 {
                return failed();
            }
            let length = body.len() as u32;
            if WinHttpSendRequest(request, std::ptr::null(), 0, body.as_ptr() as *const c_void, length, length, 0) == 0 {
                return failed();
            }
            if WinHttpReceiveResponse(request, std::ptr::null_mut()) == 0 {
                return failed();
            }
            let (mut status, mut size) = (0u32, 4u32);
            // 0x20000013 = the status code, as a number.
            WinHttpQueryHeaders(request, 0x2000_0013, std::ptr::null(), &mut status as *mut u32 as *mut c_void, &mut size, std::ptr::null_mut());
            let mut text = Vec::new();
            loop {
                let mut available = 0u32;
                if WinHttpQueryDataAvailable(request, &mut available) == 0 {
                    return failed();
                }
                if available == 0 {
                    break;
                }
                let start = text.len();
                text.resize(start + available as usize, 0);
                let mut read = 0u32;
                if WinHttpReadData(request, text.as_mut_ptr().add(start) as *mut c_void, available, &mut read) == 0 {
                    return failed();
                }
                text.truncate(start + read as usize);
                if read == 0 {
                    break;
                }
            }
            Ok((status, text))
        }
    }
}
