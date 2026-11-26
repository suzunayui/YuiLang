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
    /// For a window the graphics card draws in: what it draws on, and its
    /// OpenGL context.
    #[cfg(windows)]
    gpu: Option<(isize, isize)>,
    /// What was typed into the window since `text_input` was last called.
    #[cfg(windows)]
    typed: Vec<u16>,
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
    pub fn gpu_open(_title: &str, _width: i64, _height: i64) -> Result<(), &'static str> {
        Err(super::UNSUPPORTED)
    }
    pub fn gpu_clear(_color: i64) {}
    pub fn gpu_camera(_x: f64, _y: f64, _z: f64, _turn: f64, _tilt: f64) {}
    pub fn gpu_fog(_color: i64, _start: i64, _end: i64) {}
    pub fn gpu_mesh(_points: &[f64], _colors: &[i64]) -> i64 {
        0
    }
    pub fn gpu_draw(_mesh: i64, _x: f64, _y: f64, _z: f64) {}
    pub fn gpu_free(_mesh: i64) {}
    pub fn gpu_rect(_x: i64, _y: i64, _width: i64, _height: i64, _color: i64) {}
    pub fn gpu_text(_x: i64, _y: i64, _size: i64, _color: i64, _text: &str) {}
    pub fn text_input() -> String {
        String::new()
    }
    pub fn text_cursor(_x: i64, _y: i64, _size: i64) {}
    pub fn gpu_text_width(_size: i64, _text: &str) -> i64 {
        0
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
        fn SetProcessDPIAware() -> i32;
        fn CreateCaret(window: isize, bitmap: isize, width: i32, height: i32) -> i32;
        fn SetCaretPos(x: i32, y: i32) -> i32;
    }

    /// Where the text being composed is shown (COMPOSITIONFORM).
    #[repr(C)]
    struct Composition {
        style: u32,
        position: Point,
        area: Rect,
    }

    /// The font it is shown in (LOGFONTW).
    #[repr(C)]
    struct LogFont {
        height: i32,
        other_sizes: [i32; 4],
        flags: [u8; 8],
        face: [u16; 32],
    }

    // The part of Windows that turns keystrokes into Japanese and the like.
    #[link(name = "imm32")]
    extern "system" {
        fn ImmGetContext(window: isize) -> isize;
        fn ImmSetCompositionWindow(context: isize, form: *const Composition) -> i32;
        fn ImmSetCompositionFontW(context: isize, font: *const LogFont) -> i32;
        fn ImmReleaseContext(window: isize, context: isize) -> i32;
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
        fn ChoosePixelFormat(dc: isize, format: *const PixelFormat) -> i32;
        fn SetPixelFormat(dc: isize, index: i32, format: *const PixelFormat) -> i32;
        fn SwapBuffers(dc: isize) -> i32;
        #[allow(clippy::too_many_arguments)]
        fn CreateFontW(
            height: i32,
            width: i32,
            escapement: i32,
            orientation: i32,
            weight: i32,
            italic: u32,
            underline: u32,
            strike_out: u32,
            char_set: u32,
            out_precision: u32,
            clip_precision: u32,
            quality: u32,
            pitch_and_family: u32,
            face: *const u16,
        ) -> isize;
        fn SelectObject(dc: isize, object: isize) -> isize;
        fn DeleteObject(object: isize) -> i32;
        fn GetTextMetricsW(dc: isize, metrics: *mut [i32; 16]) -> i32;
        fn GetTextExtentPoint32W(dc: isize, text: *const u16, units: i32, size: *mut Point) -> i32;
    }

    /// What kind of surface OpenGL is to draw on (PIXELFORMATDESCRIPTOR).
    #[repr(C)]
    struct PixelFormat {
        size: u16,
        version: u16,
        flags: u32,
        pixel_type: u8,
        color_bits: u8,
        unused: [u8; 13],
        depth_bits: u8,
        rest: [u8; 16],
    }

    // OpenGL as it was in 1997: in every Windows, and every call is a plain
    // function, which is what `runtime.yui` can call too.
    #[link(name = "opengl32")]
    extern "system" {
        fn wglCreateContext(dc: isize) -> isize;
        fn wglMakeCurrent(dc: isize, context: isize) -> i32;
        fn wglDeleteContext(context: isize) -> i32;
        fn glClearColor(r: f32, g: f32, b: f32, a: f32);
        fn glClear(mask: u32);
        fn glEnable(what: u32);
        fn glDisable(what: u32);
        fn glMatrixMode(mode: u32);
        fn glLoadIdentity();
        fn glFrustum(left: f64, right: f64, bottom: f64, top: f64, near: f64, far: f64);
        fn glOrtho(left: f64, right: f64, bottom: f64, top: f64, near: f64, far: f64);
        fn glRotated(angle: f64, x: f64, y: f64, z: f64);
        fn glTranslated(x: f64, y: f64, z: f64);
        fn glScaled(x: f64, y: f64, z: f64);
        fn glViewport(x: i32, y: i32, width: i32, height: i32);
        fn glBegin(mode: u32);
        fn glEnd();
        fn glColor3ub(r: u8, g: u8, b: u8);
        fn glVertex3dv(point: *const f64);
        fn glVertex2i(x: i32, y: i32);
        fn glGenLists(count: i32) -> u32;
        fn glNewList(list: u32, mode: u32);
        fn glEndList();
        fn glCallList(list: u32);
        fn glDeleteLists(list: u32, count: i32);
        fn glPushMatrix();
        fn glPopMatrix();
        fn glPushAttrib(mask: u32);
        fn glPopAttrib();
        fn glFogi(name: u32, value: i32);
        fn glFogfv(name: u32, values: *const f32);
        fn glRasterPos2i(x: i32, y: i32);
        fn wglUseFontBitmapsW(dc: isize, first: u32, count: u32, list_base: u32) -> i32;
    }

    /// Letters of one size, as pictures the graphics card keeps: the font,
    /// the number of the first of 65,536 lists (one per character), which of
    /// them have been made, and how far the top and the next line are from
    /// the line the letters stand on.
    struct Font {
        size: i64,
        handle: isize,
        base: u32,
        made: Vec<bool>,
        ascent: i32,
        height: i32,
    }

    thread_local! {
        static FONTS: std::cell::RefCell<Vec<Font>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Forgets the letters (their pictures went with the window they were made for).
    fn forget_fonts() {
        FONTS.with(|f| {
            for font in f.borrow_mut().drain(..) {
                // Safety: a font this module created.
                unsafe {
                    DeleteObject(font.handle);
                }
            }
        });
    }

    const DEPTH_TEST: u32 = 0x0B71;
    const FOG: u32 = 0x0B60;
    const PROJECTION: u32 = 0x1701;
    const MODELVIEW: u32 = 0x1700;

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
        let handle = create(title, width * scale, height * scale)?;
        let pixels = vec![0u32; (width * height) as usize];
        WINDOW.with(|w| *w.borrow_mut() = Some(Window { handle, width, height, scale, pixels, gpu: None, typed: Vec::new() }));
        Ok(())
    }

    /// A window whose inside is `width` by `height` pixels of the screen.
    fn create(title: &str, width: i64, height: i64) -> Result<isize, &'static str> {
        let class_name = wide("YuiLangWindow");
        let title = wide(title);
        // Safety: the structures are laid out as Windows expects, and the
        // pointers given stay valid for the calls. The window procedure is
        // the system's default one.
        let handle = unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class = WndClass {
                size: std::mem::size_of::<WndClass>() as u32,
                // The window keeps what it is drawn on for itself (OpenGL wants that).
                style: 0x20,
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
            let mut rect = Rect { left: 0, top: 0, right: width as i32, bottom: height as i32 };
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
        Ok(handle)
    }

    /// A window the graphics card draws in, `width` by `height` pixels.
    pub fn gpu_open(title: &str, width: i64, height: i64) -> Result<(), &'static str> {
        close();
        let (width, height) = (width.clamp(1, 4096), height.clamp(1, 4096));
        // One pixel of the screen per pixel of the picture: the system is
        // not to stretch the window.
        // Safety: a plain setting of the process.
        unsafe {
            SetProcessDPIAware();
        }
        let handle = create(title, width, height)?;
        // Safety: `PixelFormat` is laid out as Windows expects; the handles
        // are the ones just made.
        let gpu = unsafe {
            let dc = GetDC(handle);
            // Drawn in a window, by OpenGL, shown a whole picture at a time;
            // 32 bits of color and 24 of depth per pixel.
            let format = PixelFormat {
                size: 40,
                version: 1,
                flags: 0x25,
                pixel_type: 0,
                color_bits: 32,
                unused: [0; 13],
                depth_bits: 24,
                rest: [0; 16],
            };
            let index = ChoosePixelFormat(dc, &format);
            SetPixelFormat(dc, index, &format);
            let context = wglCreateContext(dc);
            if context == 0 || wglMakeCurrent(dc, context) == 0 {
                DestroyWindow(handle);
                return Err("the graphics card could not be used (no OpenGL)");
            }
            glViewport(0, 0, width as i32, height as i32);
            glEnable(DEPTH_TEST);
            // What is seen: 70 degrees from top to bottom, from 0.1 to 1000
            // away. (Whole numbers scaled back, as `runtime.yui` does it.)
            let near = height * 10 / 7;
            let unit = (near * 10) as f64;
            glMatrixMode(PROJECTION);
            glLoadIdentity();
            glFrustum(-(width as f64), width as f64, -(height as f64), height as f64, near as f64, (near * 10000) as f64);
            glScaled(unit, unit, unit);
            glMatrixMode(MODELVIEW);
            glLoadIdentity();
            (dc, context)
        };
        let window = Window { handle, width, height, scale: 1, pixels: Vec::new(), gpu: Some(gpu), typed: Vec::new() };
        WINDOW.with(|w| *w.borrow_mut() = Some(window));
        Ok(())
    }

    /// Runs `draw` if the window is one the graphics card draws in.
    fn with_gpu<T: Default>(draw: impl FnOnce(&Window) -> T) -> T {
        WINDOW.with(|w| match w.borrow().as_ref() {
            Some(window) if window.gpu.is_some() => draw(window),
            _ => T::default(),
        })
    }

    fn parts(color: i64) -> [u8; 3] {
        [(color >> 16) as u8, (color >> 8) as u8, color as u8]
    }

    // Safety, for all of the below: plain OpenGL calls, made only while the
    // window's context is the current one; pointers outlive the calls.

    pub fn gpu_clear(color: i64) {
        with_gpu(|_| unsafe {
            let [r, g, b] = parts(color).map(|part| part as f32 / 255.0);
            glClearColor(r, g, b, 0.0);
            glClear(0x4100);
        })
    }

    /// Looks from (x, y, z). `turn` 0 looks along +z and grows towards +x;
    /// `tilt` grows upwards. Both in degrees.
    pub fn gpu_camera(x: f64, y: f64, z: f64, turn: f64, tilt: f64) {
        with_gpu(|_| unsafe {
            glMatrixMode(MODELVIEW);
            glLoadIdentity();
            glScaled(1.0, 1.0, -1.0);
            glRotated(tilt, 1.0, 0.0, 0.0);
            glRotated(-turn, 0.0, 1.0, 0.0);
            glTranslated(-x, -y, -z);
        })
    }

    /// Things fade into `color` from `start` to `end` away; no fog if `end <= start`.
    pub fn gpu_fog(color: i64, start: i64, end: i64) {
        with_gpu(|_| unsafe {
            if end <= start {
                glDisable(FOG);
                return;
            }
            let [r, g, b] = parts(color).map(|part| part as f32 / 255.0);
            glEnable(FOG);
            glFogi(0x0B65, 0x2601);
            glFogi(0x0B63, start.clamp(0, 1 << 20) as i32);
            glFogi(0x0B64, end.clamp(0, 1 << 20) as i32);
            glFogfv(0x0B66, [r, g, b, 0.0].as_ptr());
        })
    }

    /// Triangles for the card to keep: nine numbers and one color each.
    pub fn gpu_mesh(points: &[f64], colors: &[i64]) -> i64 {
        with_gpu(|_| unsafe {
            let list = glGenLists(1);
            if list == 0 {
                return 0;
            }
            glNewList(list, 0x1300);
            glBegin(4);
            for (corners, color) in points.chunks_exact(9).zip(colors) {
                let [r, g, b] = parts(*color);
                glColor3ub(r, g, b);
                for corner in corners.chunks_exact(3) {
                    glVertex3dv(corner.as_ptr());
                }
            }
            glEnd();
            glEndList();
            list as i64
        })
    }

    pub fn gpu_draw(mesh: i64, x: f64, y: f64, z: f64) {
        with_gpu(|_| unsafe {
            if mesh > 0 {
                glPushMatrix();
                glTranslated(x, y, z);
                glCallList(mesh as u32);
                glPopMatrix();
            }
        })
    }

    pub fn gpu_free(mesh: i64) {
        with_gpu(|_| unsafe {
            if mesh > 0 {
                glDeleteLists(mesh as u32, 1);
            }
        })
    }

    /// Sets things up to draw flat, on top of the picture, in pixels from the top left.
    unsafe fn overlay_begin(window: &Window) {
        glPushAttrib(0x2000);
        glDisable(DEPTH_TEST);
        glDisable(FOG);
        glMatrixMode(PROJECTION);
        glPushMatrix();
        glLoadIdentity();
        glOrtho(0.0, window.width as f64, window.height as f64, 0.0, -1.0, 1.0);
        glMatrixMode(MODELVIEW);
        glPushMatrix();
        glLoadIdentity();
    }

    unsafe fn overlay_end() {
        glPopMatrix();
        glMatrixMode(PROJECTION);
        glPopMatrix();
        glMatrixMode(MODELVIEW);
        glPopAttrib();
    }

    fn limit(n: i64) -> i32 {
        n.clamp(-(1 << 20), 1 << 20) as i32
    }

    /// A rectangle on top of the picture, in pixels from the top left.
    pub fn gpu_rect(x: i64, y: i64, width: i64, height: i64, color: i64) {
        with_gpu(|window| unsafe {
            let (left, top, right, bottom) = (limit(x), limit(y), limit(x) + limit(width), limit(y) + limit(height));
            let [r, g, b] = parts(color);
            overlay_begin(window);
            glColor3ub(r, g, b);
            glBegin(7);
            glVertex2i(left, top);
            glVertex2i(right, top);
            glVertex2i(right, bottom);
            glVertex2i(left, bottom);
            glEnd();
            overlay_end();
        })
    }

    /// The letters of `size` (made if there are none yet): their place in `fonts`.
    unsafe fn font_index(fonts: &mut Vec<Font>, dc: isize, size: i64) -> usize {
        if let Some(index) = fonts.iter().position(|font| font.size == size) {
            return index;
        }
        // Four sizes are kept; a fifth replaces the first.
        if fonts.len() == 4 {
            let old = fonts.remove(0);
            glDeleteLists(old.base, 65536);
            DeleteObject(old.handle);
        }
        let face = wide("Yu Gothic UI");
        let handle = CreateFontW(-(size as i32), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 0, 0, face.as_ptr());
        SelectObject(dc, handle);
        let mut metrics = [0i32; 16];
        GetTextMetricsW(dc, &mut metrics);
        let base = glGenLists(65536);
        fonts.push(Font { size, handle, base, made: vec![false; 65536], ascent: metrics[1], height: metrics[0] });
        fonts.len() - 1
    }

    /// How many pixels wide `text` is when drawn with `gpu_text` at `size`.
    pub fn gpu_text_width(size: i64, text: &str) -> i64 {
        with_gpu(|window| unsafe {
            let Some((dc, _)) = window.gpu else { return 0 };
            FONTS.with(|f| {
                let mut fonts = f.borrow_mut();
                let index = font_index(&mut fonts, dc, size.clamp(6, 200));
                SelectObject(dc, fonts[index].handle);
                let units: Vec<u16> = text.encode_utf16().collect();
                let mut extent = Point::default();
                GetTextExtentPoint32W(dc, units.as_ptr(), units.len() as i32, &mut extent);
                extent.x as i64
            })
        })
    }

    /// Tells the system where the text being typed appears, so that what is
    /// being composed (Japanese and the like) shows up there, `size` high.
    pub fn text_cursor(x: i64, y: i64, size: i64) {
        WINDOW.with(|w| {
            let slot = w.borrow();
            let Some(window) = slot.as_ref() else { return };
            let (x, y, size) = (limit(x), limit(y), size.clamp(6, 200) as i32);
            // Safety: the structures are laid out as Windows expects.
            unsafe {
                // Newer input methods go by the caret, older ones by the composition window.
                CreateCaret(window.handle, 0, 1, size);
                SetCaretPos(x, y);
                let context = ImmGetContext(window.handle);
                if context != 0 {
                    // 2 = at this point
                    let form = Composition { style: 2, position: Point { x, y }, area: Rect { left: 0, top: 0, right: 0, bottom: 0 } };
                    ImmSetCompositionWindow(context, &form);
                    let mut font = LogFont { height: -size, other_sizes: [0, 0, 0, 400], flags: [0, 0, 0, 1, 0, 0, 0, 0], face: [0; 32] };
                    for (to, from) in font.face.iter_mut().zip("Yu Gothic UI".encode_utf16()) {
                        *to = from;
                    }
                    ImmSetCompositionFontW(context, &font);
                    ImmReleaseContext(window.handle, context);
                }
            }
        })
    }

    /// Text on top of the picture: its top left at (x, y), letters `size`
    /// pixels high. A line feed starts a new line.
    pub fn gpu_text(x: i64, y: i64, size: i64, color: i64, text: &str) {
        with_gpu(|window| unsafe {
            let Some((dc, _)) = window.gpu else { return };
            let size = size.clamp(6, 200);
            FONTS.with(|f| {
                let mut fonts = f.borrow_mut();
                let index = font_index(&mut fonts, dc, size);
                let font = &mut fonts[index];
                if font.base == 0 {
                    return;
                }
                let [r, g, b] = parts(color);
                overlay_begin(window);
                glColor3ub(r, g, b);
                let (left, mut line) = (limit(x), limit(y) + font.ascent);
                glRasterPos2i(left, line);
                let mut selected = false;
                for ch in text.chars() {
                    let code = ch as u32;
                    if code == 10 {
                        line += font.height;
                        glRasterPos2i(left, line);
                    } else if (32..65536).contains(&code) {
                        if !font.made[code as usize] {
                            if !selected {
                                SelectObject(dc, font.handle);
                                selected = true;
                            }
                            // The first call is known to fail now and then.
                            if wglUseFontBitmapsW(dc, code, 1, font.base + code) == 0 {
                                wglUseFontBitmapsW(dc, code, 1, font.base + code);
                            }
                            font.made[code as usize] = true;
                        }
                        glCallList(font.base + code);
                    }
                }
                overlay_end();
            });
        })
    }

    /// What was typed into the window since the last call. Enter is a line feed.
    pub fn text_input() -> String {
        WINDOW.with(|w| match w.borrow_mut().as_mut() {
            Some(window) => String::from_utf16_lossy(&std::mem::take(&mut window.typed)),
            None => String::new(),
        })
    }

    /// Lets the window react to what happened to it, and shows the pixels.
    /// False once the window has been closed.
    pub fn update() -> bool {
        WINDOW.with(|w| {
            let mut slot = w.borrow_mut();
            let Some(window) = slot.as_mut() else { return false };
            // Safety: `Message` and `BitmapInfo` are laid out as Windows
            // expects; the pixels outlive the call that reads them.
            unsafe {
                let mut message: Message = std::mem::zeroed();
                while PeekMessageW(&mut message, 0, 0, 0, 1) != 0 {
                    // 0x0102 = a character was typed (Enter arrives as 13).
                    if message.message == 0x0102 && window.typed.len() < 256 {
                        let unit = message.w as u16;
                        window.typed.push(if unit == 13 { 10 } else { unit });
                    }
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                if IsWindow(window.handle) == 0 {
                    forget_fonts();
                    *slot = None;
                    return false;
                }
                if let Some((dc, _)) = window.gpu {
                    SwapBuffers(dc);
                    return true;
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
                    if let Some((_, context)) = window.gpu {
                        forget_fonts();
                        wglMakeCurrent(0, 0);
                        wglDeleteContext(context);
                    }
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
pub use os::{gpu_camera, gpu_clear, gpu_draw, gpu_fog, gpu_free, gpu_mesh, gpu_open, gpu_rect, gpu_text, text_input};
pub use os::{gpu_text_width, text_cursor};
