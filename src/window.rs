//! A window to draw in, the keyboard and mouse, the clock and random numbers,
//! for programs run by the interpreter. (`yui build` has its own version of
//! all of this, written in YuiLang: see `jit/runtime.yui`.)
//!
//! The window shows a grid of pixels that the program fills in; each pixel
//! is drawn `scale` times as large. Only Windows has windows so far.

use std::cell::{Cell, RefCell};

/// What `window_open` says where there are no windows.
pub const UNSUPPORTED: &str = "windows are only available on Windows so far";

#[cfg_attr(not(windows), allow(dead_code))]
struct Window {
    #[cfg(windows)]
    handle: isize,
    width: i64,
    height: i64,
    scale: i64,
    /// One `0x00RRGGBB` per pixel, row by row from the top.
    pixels: Vec<u32>,
}

thread_local! {
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
    static RANDOM: Cell<u64> = const { Cell::new(0) };
    static START: std::time::Instant = std::time::Instant::now();
}

/// A color from its red, green and blue parts, each cut to 0..=255.
pub fn rgb(r: i64, g: i64, b: i64) -> i64 {
    r.clamp(0, 255) * 65536 + g.clamp(0, 255) * 256 + b.clamp(0, 255)
}

pub fn clear(color: i64) {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            window.pixels.fill(color as u32);
        }
    });
}

/// Pixels outside the window are ignored.
pub fn pixel(x: i64, y: i64, color: i64) {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            if x >= 0 && y >= 0 && x < window.width && y < window.height {
                window.pixels[(y * window.width + x) as usize] = color as u32;
            }
        }
    });
}

/// The part of the rectangle that is inside the window.
pub fn rect(x: i64, y: i64, width: i64, height: i64, color: i64) {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            let (left, top) = (x.max(0), y.max(0));
            let right = x.saturating_add(width).min(window.width);
            let bottom = y.saturating_add(height).min(window.height);
            for row in top..bottom {
                for column in left..right {
                    window.pixels[(row * window.width + column) as usize] = color as u32;
                }
            }
        }
    });
}

/// Milliseconds since the program started.
pub fn time_ms() -> i64 {
    START.with(|start| start.elapsed().as_millis() as i64)
}

pub fn sleep_ms(ms: i64) {
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
    }
}

/// The same sequence as `random_next` in `runtime.yui` for the same seed.
const MIX: u64 = 88172645463325252;

pub fn random_seed(seed: i64) {
    let state = seed as u64 ^ MIX;
    RANDOM.with(|r| r.set(if state == 0 { MIX } else { state }));
}

/// A number from 0 up to, but not including, `limit` (0 if `limit` is not positive).
pub fn random(limit: i64) -> i64 {
    let mut x = RANDOM.with(|r| r.get());
    if x == 0 {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        x = now ^ MIX;
        if x == 0 {
            x = MIX;
        }
    }
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    RANDOM.with(|r| r.set(x));
    let value = (x.wrapping_mul(2685821657736338717) >> 1) as i64;
    if limit <= 0 {
        0
    } else {
        value % limit
    }
}

/// The virtual-key code of a key by its name, or 0.
#[cfg_attr(not(windows), allow(dead_code))]
fn key_code(name: &str) -> i32 {
    let bytes = name.as_bytes();
    if let [byte] = bytes {
        return match byte {
            b'a'..=b'z' => (*byte - 32) as i32,
            b'A'..=b'Z' | b'0'..=b'9' | b' ' => *byte as i32,
            _ => 0,
        };
    }
    match name {
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "space" => 0x20,
        "enter" => 0x0D,
        "escape" => 0x1B,
        "shift" => 0x10,
        "ctrl" => 0x11,
        "tab" => 0x09,
        _ => 0,
    }
}

#[cfg(not(windows))]
mod os {
    pub fn open(_title: &str, _width: i64, _height: i64, _scale: i64) -> Result<(), &'static str> {
        Err(super::UNSUPPORTED)
    }
    pub fn update() -> bool {
        false
    }
    pub fn close() {}
    pub fn key_down(_name: &str) -> bool {
        false
    }
    pub fn mouse(_axis: usize) -> i64 {
        -1
    }
    pub fn mouse_down(_button: i64) -> bool {
        false
    }
}

#[cfg(windows)]
mod os {
    use super::{Window, WINDOW};
    use std::ffi::c_void;

    #[repr(C)]
    struct WndClass {
        size: u32,
        style: u32,
        procedure: Option<unsafe extern "system" fn(isize, u32, usize, isize) -> isize>,
        class_extra: i32,
        window_extra: i32,
        instance: isize,
        icon: isize,
        cursor: isize,
        background: isize,
        menu_name: *const u16,
        class_name: *const u16,
        small_icon: isize,
    }

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct Message {
        window: isize,
        message: u32,
        w: usize,
        l: isize,
        time: u32,
        point: Point,
        private: u32,
    }

    #[repr(C)]
    struct BitmapInfo {
        size: u32,
        width: i32,
        height: i32,
        planes: u16,
        bits: u16,
        compression: u32,
        rest: [u32; 5],
        colors: [u32; 1],
    }

    #[link(name = "user32")]
    extern "system" {
        fn RegisterClassExW(class: *const WndClass) -> u16;
        #[allow(clippy::too_many_arguments)]
        fn CreateWindowExW(
            ex_style: u32,
            class: *const u16,
            title: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: isize,
            menu: isize,
            instance: isize,
            param: *const c_void,
        ) -> isize;
        fn DefWindowProcW(window: isize, message: u32, w: usize, l: isize) -> isize;
        fn PeekMessageW(message: *mut Message, window: isize, min: u32, max: u32, remove: u32) -> i32;
        fn TranslateMessage(message: *const Message) -> i32;
        fn DispatchMessageW(message: *const Message) -> isize;
        fn IsWindow(window: isize) -> i32;
        fn DestroyWindow(window: isize) -> i32;
        fn AdjustWindowRect(rect: *mut Rect, style: u32, menu: i32) -> i32;
        fn GetDC(window: isize) -> isize;
        fn ReleaseDC(window: isize, dc: isize) -> i32;
        fn GetAsyncKeyState(key: i32) -> i16;
        fn GetCursorPos(point: *mut Point) -> i32;
        fn ScreenToClient(window: isize, point: *mut Point) -> i32;
        fn LoadCursorW(instance: isize, name: *const u16) -> isize;
        fn GetForegroundWindow() -> isize;
    }

    #[link(name = "gdi32")]
    extern "system" {
        #[allow(clippy::too_many_arguments)]
        fn StretchDIBits(
            dc: isize,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            from_x: i32,
            from_y: i32,
            from_width: i32,
            from_height: i32,
            bits: *const c_void,
            info: *const BitmapInfo,
            usage: u32,
            operation: u32,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(name: *const u16) -> isize;
    }

    /// A title bar with a close and a minimize button, a border that cannot
    /// be dragged to resize, visible.
    const STYLE: u32 = 0x10CA_0000;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn open(title: &str, width: i64, height: i64, scale: i64) -> Result<(), &'static str> {
        close();
        let (width, height, scale) = (width.clamp(1, 4096), height.clamp(1, 4096), scale.clamp(1, 64));
        let class_name = wide("YuiLangWindow");
        let title = wide(title);
        // Safety: the structures are laid out as Windows expects, and the
        // pointers given stay valid for the calls. The window procedure is
        // the system's default one.
        let handle = unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class = WndClass {
                size: std::mem::size_of::<WndClass>() as u32,
                style: 0,
                procedure: Some(DefWindowProcW),
                class_extra: 0,
                window_extra: 0,
                instance,
                icon: 0,
                cursor: LoadCursorW(0, 32512 as *const u16),
                background: 0,
                menu_name: std::ptr::null(),
                class_name: class_name.as_ptr(),
                small_icon: 0,
            };
            // Fails harmlessly if the class is registered already.
            RegisterClassExW(&class);
            let mut rect = Rect { left: 0, top: 0, right: (width * scale) as i32, bottom: (height * scale) as i32 };
            AdjustWindowRect(&mut rect, STYLE, 0);
            let default = i32::MIN;
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                STYLE,
                default,
                default,
                rect.right - rect.left,
                rect.bottom - rect.top,
                0,
                0,
                instance,
                std::ptr::null(),
            )
        };
        if handle == 0 {
            return Err("the window could not be opened");
        }
        let pixels = vec![0u32; (width * height) as usize];
        WINDOW.with(|w| *w.borrow_mut() = Some(Window { handle, width, height, scale, pixels }));
        Ok(())
    }

    /// Lets the window react to what happened to it, and shows the pixels.
    /// False once the window has been closed.
    pub fn update() -> bool {
        WINDOW.with(|w| {
            let mut slot = w.borrow_mut();
            let Some(window) = slot.as_ref() else { return false };
            // Safety: `Message` and `BitmapInfo` are laid out as Windows
            // expects; the pixels outlive the call that reads them.
            unsafe {
                let mut message: Message = std::mem::zeroed();
                while PeekMessageW(&mut message, 0, 0, 0, 1) != 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                if IsWindow(window.handle) == 0 {
                    *slot = None;
                    return false;
                }
                let info = BitmapInfo {
                    size: 40,
                    width: window.width as i32,
                    // Negative: the first row is the top one.
                    height: -(window.height as i32),
                    planes: 1,
                    bits: 32,
                    compression: 0,
                    rest: [0; 5],
                    colors: [0],
                };
                let dc = GetDC(window.handle);
                StretchDIBits(
                    dc,
                    0,
                    0,
                    (window.width * window.scale) as i32,
                    (window.height * window.scale) as i32,
                    0,
                    0,
                    window.width as i32,
                    window.height as i32,
                    window.pixels.as_ptr() as *const c_void,
                    &info,
                    0,
                    0x00CC_0020,
                );
                ReleaseDC(window.handle, dc);
            }
            true
        })
    }

    pub fn close() {
        WINDOW.with(|w| {
            if let Some(window) = w.borrow_mut().take() {
                // Safety: the handle is one this module created.
                unsafe {
                    DestroyWindow(window.handle);
                }
            }
        });
    }

    /// The window's handle, if it is the one the user is working in.
    fn focused() -> Option<isize> {
        WINDOW.with(|w| {
            let handle = w.borrow().as_ref()?.handle;
            // Safety: a plain query.
            (unsafe { GetForegroundWindow() } == handle).then_some(handle)
        })
    }

    pub fn key_down(name: &str) -> bool {
        let code = super::key_code(name);
        // Safety: a plain query.
        code != 0 && focused().is_some() && unsafe { GetAsyncKeyState(code) } as u16 & 0x8000 != 0
    }

    /// Where the mouse is, in pixels of the grid: `axis` 0 is x, 1 is y.
    pub fn mouse(axis: usize) -> i64 {
        WINDOW.with(|w| {
            let slot = w.borrow();
            let Some(window) = slot.as_ref() else { return -1 };
            let mut point = Point::default();
            // Safety: `Point` is laid out as Windows expects.
            unsafe {
                GetCursorPos(&mut point);
                ScreenToClient(window.handle, &mut point);
            }
            [point.x, point.y][axis.min(1)] as i64 / window.scale
        })
    }

    pub fn mouse_down(button: i64) -> bool {
        let code = match button {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => return false,
        };
        // Safety: a plain query.
        focused().is_some() && unsafe { GetAsyncKeyState(code) } as u16 & 0x8000 != 0
    }
}

pub use os::{close, key_down, mouse, mouse_down, open, update};
