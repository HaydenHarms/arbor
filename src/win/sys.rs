//! Thin, safe-ish wrappers over the Win32 calls Lattice needs. All `unsafe`
//! lives here; the rest of the app deals in plain integers and `Rect`s.

use crate::layout::Rect;
use std::ffi::c_void;
use std::sync::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTONULL,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
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

// Silence unused-import warnings on BOOL in some windows-rs versions.
#[allow(dead_code)]
fn _unused(_: BOOL) {}
