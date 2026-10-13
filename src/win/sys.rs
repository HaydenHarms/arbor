//! Thin, safe-ish wrappers over the Win32 calls Lattice needs. All `unsafe`
//! lives here; the rest of the app deals in plain integers and `Rect`s.

use crate::layout::Rect;
use std::ffi::c_void;
use std::sync::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, BOOL, COLORREF, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW, MonitorFromPoint,
    MonitorFromWindow, ReleaseDC, SelectObject, SetStretchBltMode, StretchBlt, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, SRCCOPY,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, VK_LBUTTON,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging as wm;

pub type Hwnd = isize;

pub use wm::{
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, WM_CLOSE, WM_ENDSESSION, WM_HOTKEY, WM_LBUTTONUP,
    WM_RBUTTONUP, WM_TIMER,
};
pub const WM_TRAY: u32 = wm::WM_APP + 1;
/// Posted to ourselves to take over dragging a maximized window.
pub const WM_RESTORE_DRAG: u32 = wm::WM_APP + 2;

pub const VK_SPACE: u32 = 0x20;
pub const VK_RETURN: u32 = 0x0D;
pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;
pub const VK_OEM_4: u32 = 0xDB; // [
pub const VK_OEM_6: u32 = 0xDD; // ]
pub const VK_F: u32 = 0x46;
pub const VK_Q: u32 = 0x51;

#[derive(Clone, Copy)]
pub enum Mods {
    CtrlAlt,
    CtrlAltShift,
}

/// Parked windows sit here: far off every display, but still a normal,
/// restored window that Windows itself can bring back if Lattice dies.
const PARK: i32 = -32000;

fn hwnd(h: Hwnd) -> HWND {
    HWND(h as *mut c_void)
}

fn mon(m: isize) -> HMONITOR {
    HMONITOR(m as *mut c_void)
}

// ---------------------------------------------------------------------------
// Setup

pub fn set_dpi_aware() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Claims the per-session "Lattice is running" mutex. Returns false if another
/// copy already holds it. The handle is kept open until the process exits.
pub fn claim_single_instance() -> bool {
    unsafe {
        let made = CreateMutexW(None, true, w!("Local\\LatticeWindowManager"));
        made.is_ok() && GetLastError() != ERROR_ALREADY_EXISTS
    }
}

type WndProc = fn(u32, usize, isize) -> Option<isize>;
static WNDPROC: Mutex<Option<WndProc>> = Mutex::new(None);

unsafe extern "system" fn raw_wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let f = *WNDPROC.lock().unwrap();
    match f.and_then(|f| f(msg, wp.0, lp.0)) {
        Some(r) => LRESULT(r),
        None => wm::DefWindowProcW(h, msg, wp, lp),
    }
}

/// A hidden top-level window that receives hotkeys, timers and tray clicks.
pub fn create_message_window(proc_: WndProc) -> Hwnd {
    *WNDPROC.lock().unwrap() = Some(proc_);
    unsafe {
        let inst = GetModuleHandleW(None).expect("module handle");
        let class = w!("LatticeWindowManager");
        let wc = wm::WNDCLASSW {
            lpfnWndProc: Some(raw_wndproc),
            hInstance: inst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        wm::RegisterClassW(&wc);
        let h = wm::CreateWindowExW(
            wm::WINDOW_EX_STYLE(0),
            class,
            w!("Lattice"),
            wm::WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            inst,
            None,
        )
        .expect("create window");
        h.0 as isize
    }
}

/// Registers each hotkey with id = its index; returns the ones that failed.
pub fn register_hotkeys<T>(h: Hwnd, keys: &[(T, Mods, u32, &str)]) -> Vec<String> {
    let mut failed = Vec::new();
    for (i, (_, mods, vk, label)) in keys.iter().enumerate() {
        let m = match mods {
            Mods::CtrlAlt => MOD_CONTROL | MOD_ALT,
            Mods::CtrlAltShift => MOD_CONTROL | MOD_ALT | MOD_SHIFT,
        } | MOD_NOREPEAT;
        let ok = unsafe { RegisterHotKey(hwnd(h), i as i32, HOT_KEY_MODIFIERS(m.0), *vk) };
        if ok.is_err() {
            failed.push(label.to_string());
        }
    }
    failed
}

type EventFn = fn(u32, Hwnd);
static EVENT_FN: Mutex<Option<EventFn>> = Mutex::new(None);

unsafe extern "system" fn raw_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    h: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // Only whole top-level windows, not their caret, scroll bars, etc.
    if id_object != wm::OBJID_WINDOW.0 || id_child != 0 || h.0.is_null() {
        return;
    }
    let f = *EVENT_FN.lock().unwrap();
    if let Some(f) = f {
        f(event, h.0 as isize);
    }
}

pub fn install_event_hooks(f: EventFn) {
    *EVENT_FN.lock().unwrap() = Some(f);
    let flags = wm::WINEVENT_OUTOFCONTEXT | wm::WINEVENT_SKIPOWNPROCESS;
    let ranges = [
        (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND),
        (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
        (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE),
    ];
    for (lo, hi) in ranges {
        unsafe {
            SetWinEventHook(lo, hi, None, Some(raw_event), 0, 0, flags);
        }
    }
}

/// Mouse buttons and moves seen by the low-level mouse hook.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseEvent {
    LeftDown,
    LeftUp,
    Move,
}

type MouseFn = fn(MouseEvent, (i32, i32));
static MOUSE_FN: Mutex<Option<MouseFn>> = Mutex::new(None);

unsafe extern "system" fn raw_mouse(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let ev = match wp.0 as u32 {
            wm::WM_LBUTTONDOWN => Some(MouseEvent::LeftDown),
            wm::WM_LBUTTONUP => Some(MouseEvent::LeftUp),
            wm::WM_MOUSEMOVE => Some(MouseEvent::Move),
            _ => None,
        };
        let f = *MOUSE_FN.lock().unwrap();
        if let (Some(ev), Some(f)) = (ev, f) {
            let info = &*(lp.0 as *const wm::MSLLHOOKSTRUCT);
            f(ev, (info.pt.x, info.pt.y));
        }
    }
    wm::CallNextHookEx(None, code, wp, lp)
}

/// Watch the mouse system-wide. `f` must return quickly: Windows skips
/// hooks that hold up input.
pub fn install_mouse_hook(f: MouseFn) {
    *MOUSE_FN.lock().unwrap() = Some(f);
    unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        let _ = wm::SetWindowsHookExW(wm::WH_MOUSE_LL, Some(raw_mouse), inst, 0);
    }
}

pub fn post_to_self(h: Hwnd, msg: u32) {
    unsafe {
        let _ = wm::PostMessageW(hwnd(h), msg, WPARAM(0), LPARAM(0));
    }
}

/// Whether Windows Snap is on. With it on, Windows itself lets you drag
/// maximized windows; with it off, it doesn't.
pub fn snap_enabled() -> bool {
    let mut on = BOOL(0);
    unsafe {
        let _ = wm::SystemParametersInfoW(
            wm::SPI_GETWINARRANGING,
            0,
            Some(&mut on as *mut BOOL as *mut c_void),
            wm::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    on.as_bool()
}

pub fn is_maximized(h: Hwnd) -> bool {
    unsafe { wm::IsZoomed(hwnd(h)).as_bool() }
}

pub fn left_button_down() -> bool {
    unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000 != 0 }
}

/// Whether (x, y) is on the window's title bar, by asking the window.
/// Gives up (false) if it doesn't answer quickly.
pub fn on_title_bar(h: Hwnd, (x, y): (i32, i32)) -> bool {
    let mut hit = 0usize;
    let lp = LPARAM(((y as u16 as isize) << 16) | x as u16 as isize);
    unsafe {
        wm::SendMessageTimeoutW(hwnd(h), wm::WM_NCHITTEST, WPARAM(0), lp, wm::SMTO_ABORTIFHUNG, 100, Some(&mut hit));
    }
    hit == wm::HTCAPTION as usize
}

/// Do what Windows does when you drag a maximized window with Snap on:
/// restore it to its normal size under the pointer, keeping the grabbed
/// spot on the title bar under it, and carry on the drag as a move.
pub fn restore_and_drag(h: Hwnd, grab: (i32, i32), pointer: (i32, i32)) {
    let Some(max) = visual_rect(h) else { return };
    unsafe {
        let mut wp = wm::WINDOWPLACEMENT { length: std::mem::size_of::<wm::WINDOWPLACEMENT>() as u32, ..Default::default() };
        if wm::GetWindowPlacement(hwnd(h), &mut wp).is_err() {
            return;
        }
        let n = to_rect(wp.rcNormalPosition);
        let along = (grab.0 - max.x) as f32 / max.w.max(1) as f32;
        let x = pointer.0 - (along * n.w as f32) as i32;
        let y = pointer.1 - (grab.1 - max.y);
        let _ = wm::ShowWindowAsync(hwnd(h), wm::SW_RESTORE);
        let _ = wm::SetWindowPos(
            hwnd(h),
            None,
            x,
            y,
            0,
            0,
            wm::SWP_NOSIZE | wm::SWP_NOZORDER | wm::SWP_NOACTIVATE | wm::SWP_ASYNCWINDOWPOS,
        );
        // SC_MOVE | HTCAPTION: start the system move loop, following the
        // button that's still held down.
        let _ = wm::PostMessageW(hwnd(h), wm::WM_SYSCOMMAND, WPARAM((wm::SC_MOVE | wm::HTCAPTION) as usize), LPARAM(0));
    }
}

pub fn message_loop() {
    unsafe {
        let mut msg = wm::MSG::default();
        while wm::GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = wm::TranslateMessage(&msg);
            wm::DispatchMessageW(&msg);
        }
    }
}

pub fn post_quit() {
    unsafe { wm::PostQuitMessage(0) }
}

pub fn set_timer(h: Hwnd, id: usize, ms: u32) {
    unsafe {
        wm::SetTimer(hwnd(h), id, ms, None);
    }
}

pub fn kill_timer(h: Hwnd, id: usize) {
    unsafe {
        let _ = wm::KillTimer(hwnd(h), id);
    }
}

pub fn message_box(title: &str, text: &str) {
    let t = wide(title);
    let b = wide(text);
    unsafe {
        wm::MessageBoxW(None, PCWSTR(b.as_ptr()), PCWSTR(t.as_ptr()), wm::MB_OK | wm::MB_ICONINFORMATION);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// ---------------------------------------------------------------------------
// Tray icon

fn tray_data(h: Hwnd) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd(h),
        uID: 1,
        ..Default::default()
    }
}

pub fn add_tray_icon(h: Hwnd, tip: &str) {
    let mut d = tray_data(h);
    d.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    d.uCallbackMessage = WM_TRAY;
    d.hIcon = unsafe { wm::LoadIconW(None, wm::IDI_APPLICATION).unwrap_or_default() };
    for (dst, src) in d.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *dst = src;
    }
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &d);
    }
}

pub fn remove_tray_icon(h: Hwnd) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &tray_data(h));
    }
}

/// Shows a popup menu at the pointer; returns the chosen id, or 0.
/// An item with id 0 is drawn as a separator.
pub fn show_tray_menu(h: Hwnd, items: &[(usize, &str)]) -> usize {
    unsafe {
        let Ok(menu) = wm::CreatePopupMenu() else { return 0 };
        for (id, label) in items {
            let text = wide(label);
            let flags = if *id == 0 { wm::MF_SEPARATOR } else { wm::MF_STRING };
            let _ = wm::AppendMenuW(menu, flags, *id, PCWSTR(text.as_ptr()));
        }
        let (x, y) = cursor_pos();
        // Without this the menu won't close when the user clicks elsewhere.
        let _ = wm::SetForegroundWindow(hwnd(h));
        let pick = wm::TrackPopupMenu(
            menu,
            wm::TPM_RETURNCMD | wm::TPM_RIGHTBUTTON | wm::TPM_BOTTOMALIGN,
            x,
            y,
            0,
            hwnd(h),
            None,
        );
        let _ = wm::DestroyMenu(menu);
        pick.0 as usize
    }
}

// ---------------------------------------------------------------------------
// Windows and displays

pub fn cursor_pos() -> (i32, i32) {
    let mut p = POINT::default();
    unsafe {
        let _ = wm::GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

pub fn foreground_window() -> Hwnd {
    unsafe { wm::GetForegroundWindow().0 as isize }
}

pub fn root_window_at((x, y): (i32, i32)) -> Hwnd {
    unsafe {
        let h = wm::WindowFromPoint(POINT { x, y });
        wm::GetAncestor(h, wm::GA_ROOT).0 as isize
    }
}

pub fn monitor_at((x, y): (i32, i32)) -> isize {
    unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST).0 as isize }
}

pub fn monitor_of_window(h: Hwnd) -> isize {
    unsafe { MonitorFromWindow(hwnd(h), MONITOR_DEFAULTTONEAREST).0 as isize }
}

/// The display's area minus the taskbar.
pub fn work_area(m: isize) -> Rect {
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        let _ = GetMonitorInfoW(mon(m), &mut mi);
    }
    to_rect(mi.rcWork)
}

fn to_rect(r: RECT) -> Rect {
    Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top)
}

pub fn is_window(h: Hwnd) -> bool {
    unsafe { wm::IsWindow(hwnd(h)).as_bool() }
}

/// A normal app window: visible, top-level, captioned, resizable, not a tool
/// window, not cloaked (e.g. on another virtual desktop or a suspended UWP app).
pub fn is_manageable(h: Hwnd) -> bool {
    unsafe {
        let w = hwnd(h);
        if h == 0 || !wm::IsWindowVisible(w).as_bool() || wm::GetAncestor(w, wm::GA_ROOT) != w {
            return false;
        }
        if wm::GetWindow(w, wm::GW_OWNER).is_ok_and(|o| !o.0.is_null()) {
            return false;
        }
        let style = wm::GetWindowLongW(w, wm::GWL_STYLE) as u32;
        let ex = wm::GetWindowLongW(w, wm::GWL_EXSTYLE) as u32;
        if style & wm::WS_CAPTION.0 != wm::WS_CAPTION.0 || style & wm::WS_THICKFRAME.0 == 0 {
            return false;
        }
        if ex & (wm::WS_EX_TOOLWINDOW.0 | wm::WS_EX_NOACTIVATE.0) != 0 {
            return false;
        }
        let mut cloaked: u32 = 0;
        let _ = DwmGetWindowAttribute(
            w,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        );
        cloaked == 0
    }
}

pub fn restore_if_maximized(h: Hwnd) {
    unsafe {
        if wm::IsZoomed(hwnd(h)).as_bool() {
            let _ = wm::ShowWindow(hwnd(h), wm::SW_RESTORE);
        }
    }
}

fn window_rect(h: Hwnd) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe { wm::GetWindowRect(hwnd(h), &mut r).ok()? };
    Some(r)
}

fn frame_bounds(h: Hwnd) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd(h),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .ok()?;
    }
    Some(r)
}

/// Where the window visibly is, excluding the invisible resize borders that
/// Windows 10/11 add around most windows.
pub fn visual_rect(h: Hwnd) -> Option<Rect> {
    frame_bounds(h).or_else(|| window_rect(h)).map(to_rect)
}

/// Move and size a window so its *visible* frame lands exactly on `r`.
pub fn place_window(h: Hwnd, r: Rect) {
    // Invisible border widths: window rect minus visible frame.
    let (l, t, rr, b) = match (window_rect(h), frame_bounds(h)) {
        (Some(w), Some(f)) => (f.left - w.left, f.top - w.top, w.right - f.right, w.bottom - f.bottom),
        _ => (0, 0, 0, 0),
    };
    unsafe {
        // Async, so a hung app can never freeze Lattice.
        let _ = wm::SetWindowPos(
            hwnd(h),
            None,
            r.x - l,
            r.y - t,
            r.w + l + rr,
            r.h + t + b,
            wm::SWP_NOZORDER | wm::SWP_NOACTIVATE | wm::SWP_NOOWNERZORDER | wm::SWP_ASYNCWINDOWPOS,
        );
    }
}

pub fn park_window(h: Hwnd) {
    unsafe {
        let _ = wm::SetWindowPos(
            hwnd(h),
            None,
            PARK,
            PARK,
            0,
            0,
            wm::SWP_NOSIZE | wm::SWP_NOZORDER | wm::SWP_NOACTIVATE | wm::SWP_ASYNCWINDOWPOS,
        );
    }
}

// ---------------------------------------------------------------------------
// Drop-zone overlay: a frosted panel marking where a dragged window will
// dock. It sits just under the dragged window, above everything else, and
// never takes the mouse. Windows only blurs behind *active* windows, so the
// frost is done by hand: capture the screen under the zone (the panel is
// excluded from capture), blur it in `crate::frost`, and show the result.

unsafe extern "system" fn overlay_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    wm::DefWindowProcW(h, msg, wp, lp)
}

pub fn create_overlay() -> Hwnd {
    unsafe {
        let Ok(inst) = GetModuleHandleW(None) else { return 0 };
        let class = w!("LatticeDropZone");
        let wc = wm::WNDCLASSW {
            lpfnWndProc: Some(overlay_proc),
            hInstance: inst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        wm::RegisterClassW(&wc);
        // Layered + transparent: per-pixel alpha, and clicks pass through.
        let ex = wm::WS_EX_LAYERED | wm::WS_EX_TRANSPARENT | wm::WS_EX_TOOLWINDOW | wm::WS_EX_NOACTIVATE;
        let Ok(h) = wm::CreateWindowExW(ex, class, w!(""), wm::WS_POPUP, 0, 0, 0, 0, None, None, inst, None) else {
            return 0;
        };
        // Keep it out of screen captures, including our own, so capturing
        // what's under it never sees it.
        let _ = wm::SetWindowDisplayAffinity(h, wm::WDA_EXCLUDEFROMCAPTURE);
        h.0 as isize
    }
}

/// The screen inside `r`, shrunk to sw×sh with averaging, as 0xAARRGGBB.
pub fn capture_scaled(r: Rect, sw: usize, sh: usize) -> Vec<u32> {
    let mut out = vec![0u32; sw * sh];
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(screen);
        let (bmp, bits) = dib(screen, sw, sh);
        if let Some(bmp) = bmp {
            let old = SelectObject(mem, bmp);
            SetStretchBltMode(mem, HALFTONE);
            let _ = StretchBlt(mem, 0, 0, sw as i32, sh as i32, screen, r.x, r.y, r.w, r.h, SRCCOPY);
            std::ptr::copy_nonoverlapping(bits as *const u32, out.as_mut_ptr(), sw * sh);
            SelectObject(mem, old);
            let _ = DeleteObject(bmp);
        }
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }
    out
}

/// A top-down 32-bit DIB section of w×h, and a pointer to its pixels.
unsafe fn dib(dc: HDC, w: usize, h: usize) -> (Option<HBITMAP>, *mut c_void) {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bmp = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok();
    (bmp.filter(|_| !bits.is_null()), bits)
}

/// Show premultiplied 0xAARRGGBB `pixels` (r.w×r.h) at `r`, just beneath
/// window `below`.
pub fn show_overlay(h: Hwnd, r: Rect, pixels: &[u32], below: Hwnd) {
    let (w, ht) = (r.w.max(1) as usize, r.h.max(1) as usize);
    if pixels.len() != w * ht {
        return;
    }
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(screen);
        let (bmp, bits) = dib(screen, w, ht);
        if let Some(bmp) = bmp {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u32, w * ht);
            let old = SelectObject(mem, bmp);
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
                ..Default::default()
            };
            let _ = wm::UpdateLayeredWindow(
                hwnd(h),
                screen,
                Some(&POINT { x: r.x, y: r.y }),
                Some(&SIZE { cx: r.w, cy: r.h }),
                mem,
                Some(&POINT::default()),
                COLORREF(0),
                Some(&blend),
                wm::ULW_ALPHA,
            );
            SelectObject(mem, old);
            let _ = DeleteObject(bmp);
        }
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        let _ = wm::SetWindowPos(
            hwnd(h),
            hwnd(below),
            0,
            0,
            0,
            0,
            wm::SWP_NOMOVE | wm::SWP_NOSIZE | wm::SWP_NOACTIVATE | wm::SWP_SHOWWINDOW,
        );
    }
}

pub fn hide_overlay(h: Hwnd) {
    unsafe {
        let _ = wm::ShowWindow(hwnd(h), wm::SW_HIDE);
    }
}


// ---------------------------------------------------------------------------
// Rescue: if Lattice ever crashes, no window is left parked off-screen.

static RESCUE: Mutex<Vec<Hwnd>> = Mutex::new(Vec::new());

pub fn rescue_list_add(h: Hwnd) {
    let mut v = RESCUE.lock().unwrap();
    if !v.contains(&h) {
        v.push(h);
    }
}

pub fn rescue_list_remove(h: Hwnd) {
    RESCUE.lock().unwrap().retain(|&x| x != h);
}

pub fn rescue_list_clear() {
    RESCUE.lock().unwrap().clear();
}

/// Bring any off-screen managed window back onto the nearest display.
pub fn rescue_all() {
    let list = match RESCUE.try_lock() {
        Ok(v) => v.clone(),
        Err(_) => return,
    };
    for (i, h) in list.into_iter().enumerate() {
        unsafe {
            if !wm::IsWindow(hwnd(h)).as_bool() {
                continue;
            }
            let on_screen = !MonitorFromWindow(hwnd(h), MONITOR_DEFAULTTONULL).0.is_null();
            if !on_screen {
                let off = 60 + 32 * (i as i32 % 10);
                let _ = wm::SetWindowPos(
                    hwnd(h),
                    None,
                    off,
                    off,
                    0,
                    0,
                    wm::SWP_NOSIZE | wm::SWP_NOZORDER | wm::SWP_NOACTIVATE | wm::SWP_ASYNCWINDOWPOS,
                );
            }
        }
    }
}

pub fn install_rescue_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        rescue_all();
        prev(info);
    }));
}
