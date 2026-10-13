//! Thin, safe-ish wrappers over the Win32 calls Lattice needs. All `unsafe`
//! lives here; the rest of the app deals in plain integers and `Rect`s.

use crate::layout::Rect;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, BOOL, COLORREF, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmRegisterThumbnail, DwmSetWindowAttribute, DwmUnregisterThumbnail,
    DwmUpdateThumbnailProperties, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE, DWM_TNP_VISIBLE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW,
    CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC, GetDIBits,
    GetMonitorInfoW, GetObjectW, GetStockObject, UpdateWindow, BITMAP, NULL_BRUSH, PS_SOLID,
    InvalidateRect, MonitorFromPoint, MonitorFromWindow, ReleaseDC, RoundRect, ScreenToClient, SelectObject, SetBkMode,
    SetStretchBltMode, SetTextColor, StretchBlt, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    BLENDFUNCTION, DIB_RGB_COLORS, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HALFTONE, HBITMAP,
    HDC, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, NULL_PEN, PAINTSTRUCT, SRCCOPY,
    TRANSPARENT,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetCapture, RegisterHotKey, ReleaseCapture, SetCapture, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL,
    MOD_NOREPEAT, MOD_SHIFT, VK_LBUTTON,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging as wm;

pub type Hwnd = isize;

pub use wm::{
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_NAMECHANGE, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, WM_CLOSE, WM_ENDSESSION, WM_HOTKEY, WM_LBUTTONUP,
    WM_RBUTTONUP, WM_TIMER,
};
pub const WM_TRAY: u32 = wm::WM_APP + 1;
/// Mouse events from the hook thread.
pub const WM_MOUSE_HOOK: u32 = wm::WM_APP + 3;

pub const VK_SPACE: u32 = 0x20;
pub const VK_RETURN: u32 = 0x0D;
pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;
pub const VK_OEM_4: u32 = 0xDB; // [
pub const VK_OEM_6: u32 = 0xDD; // ]
pub const VK_PRIOR: u32 = 0x21; // Page Up
pub const VK_NEXT: u32 = 0x22; // Page Down
pub const VK_ESCAPE: u32 = 0x1B;
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
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
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

/// Where the hook thread posts mouse events: (window, message).
static MOUSE_TARGET: Mutex<(Hwnd, u32)> = Mutex::new((0, 0));
/// Whether the left button is down; moves only matter (and are only posted)
/// while it is.
static LEFT_DOWN: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn raw_mouse(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let ev = match wp.0 as u32 {
            wm::WM_LBUTTONDOWN => {
                LEFT_DOWN.store(true, Ordering::Relaxed);
                Some(MouseEvent::LeftDown)
            }
            wm::WM_LBUTTONUP => {
                LEFT_DOWN.store(false, Ordering::Relaxed);
                Some(MouseEvent::LeftUp)
            }
            wm::WM_MOUSEMOVE if LEFT_DOWN.load(Ordering::Relaxed) => Some(MouseEvent::Move),
            _ => None,
        };
        if let (Some(ev), Ok(target)) = (ev, MOUSE_TARGET.try_lock()) {
            let info = &*(lp.0 as *const wm::MSLLHOOKSTRUCT);
            let packed = ((info.pt.x as u32 as u64) << 32) | info.pt.y as u32 as u64;
            let _ = wm::PostMessageW(hwnd(target.0), target.1, WPARAM(ev as usize), LPARAM(packed as isize));
        }
    }
    wm::CallNextHookEx(None, code, wp, lp)
}

/// Watch the mouse system-wide, posting `msg` to window `h` for each left
/// press, release, and move while pressed (decode with `mouse_message`).
///
/// The hook gets its own thread. Windows holds every mouse movement on the
/// system until a low-level hook returns, so it must never wait behind
/// Lattice's own work, like drawing the drop zone.
pub fn install_mouse_hook(h: Hwnd, msg: u32) {
    *MOUSE_TARGET.lock().unwrap() = (h, msg);
    std::thread::spawn(|| unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        if wm::SetWindowsHookExW(wm::WH_MOUSE_LL, Some(raw_mouse), inst, 0).is_ok() {
            let mut m = wm::MSG::default();
            while wm::GetMessageW(&mut m, None, 0, 0).as_bool() {}
        }
    });
}

/// The event and point carried by a message from the mouse hook.
pub fn mouse_message(wparam: usize, lparam: isize) -> Option<(MouseEvent, (i32, i32))> {
    let ev = [MouseEvent::LeftDown, MouseEvent::LeftUp, MouseEvent::Move].into_iter().find(|e| *e as usize == wparam)?;
    let p = lparam as u64;
    Some((ev, ((p >> 32) as u32 as i32, p as u32 as i32)))
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
        // what's under it never sees it. LATTICE_RECORDING=1 leaves it in,
        // for screen recordings; Lattice captures before showing it anyway,
        // except when a drag crosses to another display.
        if std::env::var_os("LATTICE_RECORDING").is_none() {
            let _ = wm::SetWindowDisplayAffinity(h, wm::WDA_EXCLUDEFROMCAPTURE);
        }
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
// Tab strips: a small window over each tab group, showing its tabs. It
// never takes focus; clicks and drags on it go to the app's callback.

/// One tab as a strip shows it.
#[derive(Clone)]
pub struct Tab {
    pub title: String,
    pub icon: isize,
}

struct StripData {
    tabs: Vec<Tab>,
    active: usize,
    focused: bool,
    light: bool,
}

static STRIPS: Mutex<Vec<(Hwnd, StripData)>> = Mutex::new(Vec::new());

/// Mouse input on a strip, with the pointer in screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripEvent {
    Down,
    /// The pointer moved with the left button held since a `Down`.
    Drag,
    Up,
    Middle,
}

type StripFn = fn(Hwnd, StripEvent, (i32, i32));
static STRIP_FN: Mutex<Option<StripFn>> = Mutex::new(None);

unsafe extern "system" fn strip_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ev = match msg {
        wm::WM_MOUSEACTIVATE => return LRESULT(wm::MA_NOACTIVATE as isize),
        wm::WM_ERASEBKGND => return LRESULT(1),
        wm::WM_PAINT => {
            paint_strip(h);
            return LRESULT(0);
        }
        wm::WM_LBUTTONDOWN => {
            SetCapture(h);
            StripEvent::Down
        }
        wm::WM_MOUSEMOVE if GetCapture() == h => StripEvent::Drag,
        wm::WM_LBUTTONUP if GetCapture() == h => {
            let _ = ReleaseCapture();
            StripEvent::Up
        }
        wm::WM_MBUTTONUP => StripEvent::Middle,
        _ => return wm::DefWindowProcW(h, msg, wp, lp),
    };
    let mut p = POINT { x: lp.0 as i16 as i32, y: (lp.0 >> 16) as i16 as i32 };
    let _ = ClientToScreen(h, &mut p);
    let f = *STRIP_FN.lock().unwrap();
    if let Some(f) = f {
        f(h.0 as isize, ev, (p.x, p.y));
    }
    LRESULT(0)
}

/// Set the callback for input on tab strips.
pub fn on_strip_input(f: StripFn) {
    *STRIP_FN.lock().unwrap() = Some(f);
}

pub fn create_strip() -> Hwnd {
    unsafe {
        let Ok(inst) = GetModuleHandleW(None) else { return 0 };
        let class = w!("LatticeTabs");
        let wc = wm::WNDCLASSW {
            lpfnWndProc: Some(strip_proc),
            hInstance: inst.into(),
            lpszClassName: class,
            hCursor: wm::LoadCursorW(None, wm::IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        // Fails harmlessly once the class exists.
        wm::RegisterClassW(&wc);
        let ex = wm::WS_EX_TOOLWINDOW | wm::WS_EX_NOACTIVATE;
        let Ok(h) = wm::CreateWindowExW(ex, class, w!(""), wm::WS_POPUP, 0, 0, 0, 0, None, None, inst, None) else {
            return 0;
        };
        let round = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            h,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &round as *const _ as *const c_void,
            std::mem::size_of_val(&round) as u32,
        );
        h.0 as isize
    }
}

/// What strip `h` shows: its tabs, which one is picked, and whether that
/// window has the focus. Repaints it.
pub fn set_strip(h: Hwnd, tabs: Vec<Tab>, active: usize, focused: bool) {
    let data = StripData { tabs, active, focused, light: light_theme() };
    {
        let mut strips = STRIPS.lock().unwrap();
        match strips.iter_mut().find(|(s, _)| *s == h) {
            Some((_, d)) => *d = data,
            None => strips.push((h, data)),
        }
    }
    unsafe {
        let _ = InvalidateRect(hwnd(h), None, false);
    }
}

/// Where each tab of strip `h` is, in its client coordinates.
fn strip_tab_rects(h: Hwnd) -> Vec<Rect> {
    let n = STRIPS.lock().unwrap().iter().find(|(s, _)| *s == h).map_or(0, |(_, d)| d.tabs.len());
    let mut rc = RECT::default();
    let s = unsafe {
        let _ = wm::GetClientRect(hwnd(h), &mut rc);
        GetDpiForWindow(hwnd(h)) as f32 / 96.0
    };
    crate::layout::tab_rects(rc.right, rc.bottom, n, (3.0 * s) as i32, (220.0 * s) as i32)
}

/// Which tab of strip `h` is under screen point `pt`.
pub fn strip_tab_at(h: Hwnd, pt: (i32, i32)) -> Option<usize> {
    let mut p = POINT { x: pt.0, y: pt.1 };
    unsafe {
        let _ = ScreenToClient(hwnd(h), &mut p);
    }
    crate::layout::tab_at(&strip_tab_rects(h), p.x)
}

/// Show strip `h` at `r`, just above window `owner`, or hide it.
pub fn place_strip(h: Hwnd, r: Rect, owner: Hwnd, show: bool) {
    unsafe {
        if !show {
            if wm::IsWindowVisible(hwnd(h)).as_bool() {
                let _ = wm::ShowWindow(hwnd(h), wm::SW_HIDE);
            }
            return;
        }
        // Stack it right above its window: after whatever is above that.
        let above = wm::GetWindow(hwnd(owner), wm::GW_HWNDPREV).ok().filter(|p| !p.0.is_null());
        let mut flags = wm::SWP_NOACTIVATE | wm::SWP_SHOWWINDOW;
        let after = match above {
            Some(p) if p == hwnd(h) => {
                flags |= wm::SWP_NOZORDER;
                HWND::default()
            }
            Some(p) if wm::GetWindowLongW(p, wm::GWL_EXSTYLE) as u32 & wm::WS_EX_TOPMOST.0 == 0 => p,
            _ => wm::HWND_TOP,
        };
        let _ = wm::SetWindowPos(hwnd(h), after, r.x, r.y, r.w, r.h, flags);
    }
}

unsafe fn paint_strip(h: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let dc = BeginPaint(h, &mut ps);
    let mut rc = RECT::default();
    let _ = wm::GetClientRect(h, &mut rc);
    let (w, ht) = (rc.right.max(1), rc.bottom.max(1));
    let mem = CreateCompatibleDC(dc);
    let bmp = CreateCompatibleBitmap(dc, w, ht);
    let old = SelectObject(mem, bmp);

    let (tabs, active, focused, light) = match STRIPS.lock().unwrap().iter().find(|(s, _)| *s == h.0 as isize) {
        Some((_, d)) => (d.tabs.clone(), d.active, d.focused, d.light),
        None => (vec![], 0, false, false),
    };
    // Colours as 0xBBGGRR, after Windows 11's own.
    let (bg, tab_bg, text, dim, accent) = if light {
        (0xe8e8e8, 0xffffff, 0x1a1a1a, 0x5c5c5c, 0xb85f00)
    } else {
        (0x1c1c1c, 0x383838, 0xffffff, 0xa8a8a8, 0xffc24c)
    };
    let fill = |r: RECT, c: u32| {
        let b = CreateSolidBrush(COLORREF(c));
        FillRect(mem, &r, b);
        let _ = DeleteObject(b);
    };
    fill(rc, bg);

    let s = GetDpiForWindow(h) as f32 / 96.0;
    let px = |v: f32| (v * s).round() as i32;
    let font = CreateFontW(-px(12.0), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, w!("Segoe UI"));
    let old_font = SelectObject(mem, font);
    let _ = SetBkMode(mem, TRANSPARENT);
    let old_pen = SelectObject(mem, GetStockObject(NULL_PEN));
    for (i, (t, r)) in tabs.iter().zip(strip_tab_rects(h.0 as isize)).enumerate() {
        let picked = i == active;
        if picked {
            let b = CreateSolidBrush(COLORREF(tab_bg));
            let old_b = SelectObject(mem, b);
            let rad = px(6.0);
            let _ = RoundRect(mem, r.x, r.y, r.x + r.w + 1, r.y + r.h + 1, rad, rad);
            SelectObject(mem, old_b);
            let _ = DeleteObject(b);
            if focused {
                let (lw, lh) = (px(16.0), px(3.0).max(2));
                let cx = r.x + r.w / 2;
                fill(RECT { left: cx - lw / 2, top: r.y + r.h - lh, right: cx + lw / 2, bottom: r.y + r.h }, accent);
            }
        }
        let mut x = r.x + px(8.0);
        let icon = px(16.0);
        if t.icon != 0 {
            let _ = wm::DrawIconEx(
                mem,
                x,
                r.y + (r.h - icon) / 2,
                wm::HICON(t.icon as *mut c_void),
                icon,
                icon,
                0,
                None,
                wm::DI_NORMAL,
            );
            x += icon + px(8.0);
        }
        let _ = SetTextColor(mem, COLORREF(if picked { text } else { dim }));
        let mut tr = RECT { left: x, top: r.y, right: r.x + r.w - px(8.0), bottom: r.y + r.h };
        let mut title: Vec<u16> = t.title.encode_utf16().collect();
        DrawTextW(mem, &mut title, &mut tr, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX | DT_LEFT);
    }
    SelectObject(mem, old_pen);
    SelectObject(mem, old_font);
    let _ = DeleteObject(font);

    let _ = BitBlt(dc, 0, 0, w, ht, mem, 0, 0, SRCCOPY);
    SelectObject(mem, old);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mem);
    let _ = EndPaint(h, &ps);
}

// ---------------------------------------------------------------------------
// Overview: a window covering a display, showing every docked window on it
// as a card with a live DWM thumbnail. The app lays the cards out and moves
// the thumbnails; this draws the cards and reports input.

/// One card as the overview draws it, in the overview's client coordinates.
#[derive(Clone)]
pub struct Card {
    pub rect: Rect,
    pub caption: Rect,
    /// The card's tabs and where each sits in the caption. A lone window
    /// has one, drawn as a plain title.
    pub tabs: Vec<(Rect, Tab)>,
    pub active: usize,
    /// The app's colour (0xBBGGRR), tinting the caption, if it has one.
    pub tint: Option<u32>,
    /// Too small for a picture: the app's icon goes here instead.
    pub icon_only: Option<(Rect, isize)>,
}

struct OverviewData {
    cards: Vec<Card>,
    hover: Option<usize>,
    light: bool,
}

static OVERVIEW: Mutex<OverviewData> = Mutex::new(OverviewData { cards: Vec::new(), hover: None, light: false });

/// Input on the overview, with the pointer in screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverviewEvent {
    Move,
    Click,
    Middle,
    Key(u32),
    /// Another window took the focus.
    Away,
}

type OverviewFn = fn(OverviewEvent, (i32, i32));
static OVERVIEW_FN: Mutex<Option<OverviewFn>> = Mutex::new(None);
/// Posted to itself when the overview loses the focus, so the app hears of
/// it from the message loop rather than in the middle of whatever moved it.
const WM_OVERVIEW_AWAY: u32 = wm::WM_APP + 5;

unsafe extern "system" fn overview_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ev = match msg {
        wm::WM_ERASEBKGND => return LRESULT(1),
        wm::WM_PAINT => {
            paint_overview(h);
            return LRESULT(0);
        }
        wm::WM_ACTIVATE => {
            if wp.0 & 0xffff == wm::WA_INACTIVE as usize {
                let _ = wm::PostMessageW(h, WM_OVERVIEW_AWAY, WPARAM(0), LPARAM(0));
            }
            return LRESULT(0);
        }
        WM_OVERVIEW_AWAY => OverviewEvent::Away,
        wm::WM_MOUSEMOVE => OverviewEvent::Move,
        wm::WM_LBUTTONUP => OverviewEvent::Click,
        wm::WM_MBUTTONUP => OverviewEvent::Middle,
        wm::WM_KEYDOWN => OverviewEvent::Key(wp.0 as u32),
        _ => return wm::DefWindowProcW(h, msg, wp, lp),
    };
    let mut p = POINT::default();
    let _ = wm::GetCursorPos(&mut p);
    let f = *OVERVIEW_FN.lock().unwrap();
    if let Some(f) = f {
        f(ev, (p.x, p.y));
    }
    LRESULT(0)
}

/// Set the callback for input on the overview.
pub fn on_overview_input(f: OverviewFn) {
    *OVERVIEW_FN.lock().unwrap() = Some(f);
}

pub fn create_overview() -> Hwnd {
    unsafe {
        let Ok(inst) = GetModuleHandleW(None) else { return 0 };
        let class = w!("LatticeOverview");
        let wc = wm::WNDCLASSW {
            lpfnWndProc: Some(overview_proc),
            hInstance: inst.into(),
            lpszClassName: class,
            hCursor: wm::LoadCursorW(None, wm::IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        wm::RegisterClassW(&wc);
        let ex = wm::WS_EX_TOPMOST | wm::WS_EX_TOOLWINDOW;
        let Ok(h) =
            wm::CreateWindowExW(ex, class, w!("Lattice overview"), wm::WS_POPUP, 0, 0, 0, 0, None, None, inst, None)
        else {
            return 0;
        };
        h.0 as isize
    }
}

/// Show the overview over `r`, on top of everything, with the focus.
pub fn show_overview(h: Hwnd, r: Rect) {
    OVERVIEW.lock().unwrap().light = light_theme();
    unsafe {
        let _ = wm::SetWindowPos(hwnd(h), wm::HWND_TOPMOST, r.x, r.y, r.w, r.h, wm::SWP_SHOWWINDOW);
        let _ = wm::SetForegroundWindow(hwnd(h));
        let _ = UpdateWindow(hwnd(h));
    }
}

pub fn hide_overview(h: Hwnd) {
    set_overview(h, vec![], None);
    unsafe {
        let _ = wm::ShowWindow(hwnd(h), wm::SW_HIDE);
    }
}

/// What the overview shows: its cards (none while they fly in or out), and
/// the one under the pointer. Repaints it.
pub fn set_overview(h: Hwnd, cards: Vec<Card>, hover: Option<usize>) {
    {
        let mut d = OVERVIEW.lock().unwrap();
        d.cards = cards;
        d.hover = hover;
    }
    unsafe {
        let _ = InvalidateRect(hwnd(h), None, false);
    }
}

/// Mix `a` toward `b` by `k` (0..=1), both 0xBBGGRR.
fn mix(a: u32, b: u32, k: f32) -> u32 {
    let ch = |s: u32| {
        let (x, y) = (((a >> s) & 0xff) as f32, ((b >> s) & 0xff) as f32);
        ((x + (y - x) * k).round() as u32) << s
    };
    ch(0) | ch(8) | ch(16)
}

unsafe fn paint_overview(h: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let dc = BeginPaint(h, &mut ps);
    let mut rc = RECT::default();
    let _ = wm::GetClientRect(h, &mut rc);
    let (w, ht) = (rc.right.max(1), rc.bottom.max(1));
    let mem = CreateCompatibleDC(dc);
    let bmp = CreateCompatibleBitmap(dc, w, ht);
    let old = SelectObject(mem, bmp);

    let (cards, hover, light) = {
        let d = OVERVIEW.lock().unwrap();
        (d.cards.clone(), d.hover, d.light)
    };
    // Colours as 0xBBGGRR.
    let (bg, card_bg, chip, text, dim, accent) = if light {
        (0xd6d2ce, 0xf3f3f3, 0xffffff, 0x1a1a1a, 0x5c5c5c, 0xb85f00)
    } else {
        (0x161412, 0x2c2c2c, 0x444444, 0xffffff, 0xa8a8a8, 0xffc24c)
    };
    let fill = |r: RECT, c: u32| {
        let b = CreateSolidBrush(COLORREF(c));
        FillRect(mem, &r, b);
        let _ = DeleteObject(b);
    };
    let round = |r: Rect, c: u32, rad: i32| {
        let b = CreateSolidBrush(COLORREF(c));
        let old_b = SelectObject(mem, b);
        let _ = RoundRect(mem, r.x, r.y, r.x + r.w + 1, r.y + r.h + 1, rad, rad);
        SelectObject(mem, old_b);
        let _ = DeleteObject(b);
    };
    fill(rc, bg);

    let s = GetDpiForWindow(h) as f32 / 96.0;
    let px = |v: f32| (v * s).round() as i32;
    let font = CreateFontW(-px(12.0), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, w!("Segoe UI"));
    let old_font = SelectObject(mem, font);
    let _ = SetBkMode(mem, TRANSPARENT);
    let old_pen = SelectObject(mem, GetStockObject(NULL_PEN));
    let icon_at = |icon: isize, x: i32, y: i32, size: i32| {
        if icon != 0 {
            let _ = wm::DrawIconEx(mem, x, y, wm::HICON(icon as *mut c_void), size, size, 0, None, wm::DI_NORMAL);
        }
    };
    for (i, c) in cards.iter().enumerate() {
        let tint = |base: u32, k: f32| c.tint.map_or(base, |t| mix(base, t, k));
        if c.icon_only.is_some() {
            round(c.rect, tint(card_bg, 0.45), px(10.0));
        } else {
            // The caption, tinted with the app's colour, rounded at the top
            // and square where the picture meets it.
            round(c.rect, card_bg, px(10.0));
            let cap = tint(card_bg, 0.25);
            round(c.caption, cap, px(10.0));
            let half = c.caption.h / 2;
            let (x, y) = (c.caption.x, c.caption.y);
            fill(RECT { left: x, top: y + half, right: x + c.caption.w, bottom: y + c.caption.h }, cap);
        }
        let lone = c.tabs.len() == 1;
        for (j, (r, t)) in c.tabs.iter().enumerate() {
            let picked = j == c.active;
            if picked && !lone {
                round(*r, tint(chip, 0.2), px(6.0));
            }
            let icon = px(16.0);
            let mut x = r.x + px(8.0);
            if t.icon != 0 {
                icon_at(t.icon, x, r.y + (r.h - icon) / 2, icon);
                x += icon + px(8.0);
            }
            let _ = SetTextColor(mem, COLORREF(if picked { text } else { dim }));
            let mut tr = RECT { left: x, top: r.y, right: r.x + r.w - px(8.0), bottom: r.y + r.h };
            let mut title: Vec<u16> = t.title.encode_utf16().collect();
            DrawTextW(mem, &mut title, &mut tr, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX | DT_LEFT);
        }
        if let Some((r, icon)) = c.icon_only {
            icon_at(icon, r.x, r.y, r.w);
        }
        if hover == Some(i) {
            let pen = CreatePen(PS_SOLID, px(2.0).max(2), COLORREF(accent));
            let p = SelectObject(mem, pen);
            let b = SelectObject(mem, GetStockObject(NULL_BRUSH));
            let r = c.rect;
            let _ = RoundRect(mem, r.x, r.y, r.x + r.w + 1, r.y + r.h + 1, px(10.0), px(10.0));
            SelectObject(mem, b);
            SelectObject(mem, p);
            let _ = DeleteObject(pen);
        }
    }
    SelectObject(mem, old_pen);
    SelectObject(mem, old_font);
    let _ = DeleteObject(font);

    let _ = BitBlt(dc, 0, 0, w, ht, mem, 0, 0, SRCCOPY);
    SelectObject(mem, old);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mem);
    let _ = EndPaint(h, &ps);
}

/// Show a live picture of window `src` in window `dest`. 0 if it can't.
pub fn thumb_register(dest: Hwnd, src: Hwnd) -> isize {
    unsafe { DwmRegisterThumbnail(hwnd(dest), hwnd(src)).unwrap_or(0) }
}

pub fn thumb_unregister(t: isize) {
    if t != 0 {
        unsafe {
            let _ = DwmUnregisterThumbnail(t);
        }
    }
}

/// Draw the `crop` part of thumbnail `t`'s window (relative to its window
/// rect) at `dest`, in the overview's client coordinates.
pub fn thumb_set(t: isize, crop: Rect, dest: Rect, visible: bool) {
    let r = |r: Rect| RECT { left: r.x, top: r.y, right: r.x + r.w, bottom: r.y + r.h };
    let props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_RECTSOURCE | DWM_TNP_VISIBLE,
        rcDestination: r(dest),
        rcSource: r(crop),
        fVisible: visible.into(),
        ..Default::default()
    };
    unsafe {
        let _ = DwmUpdateThumbnailProperties(t, &props);
    }
}

/// The visible part of window `h`, relative to its window rect: what a
/// thumbnail of it shows, leaving out the invisible resize borders.
pub fn window_crop(h: Hwnd) -> Option<Rect> {
    let (w, f) = (window_rect(h)?, frame_bounds(h)?);
    Some(Rect::new(f.left - w.left, f.top - w.top, f.right - f.left, f.bottom - f.top))
}

/// The main colour of an icon (0xBBGGRR): the average of its solid pixels,
/// leaning toward the more colourful ones.
pub fn icon_colour(icon: isize) -> Option<u32> {
    if icon == 0 {
        return None;
    }
    unsafe {
        let mut info = wm::ICONINFO::default();
        wm::GetIconInfo(wm::HICON(icon as *mut c_void), &mut info).ok()?;
        let colour = icon_bitmap_colour(info.hbmColor);
        let _ = DeleteObject(info.hbmColor);
        let _ = DeleteObject(info.hbmMask);
        colour
    }
}

unsafe fn icon_bitmap_colour(bmp: HBITMAP) -> Option<u32> {
    let mut bm = BITMAP::default();
    if bmp.is_invalid() || GetObjectW(bmp, std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void)) == 0 {
        return None;
    }
    let (w, h) = (bm.bmWidth.max(1), bm.bmHeight.abs().max(1));
    let mut px = vec![0u32; (w * h) as usize];
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let dc = GetDC(None);
    let got = GetDIBits(dc, bmp, 0, h as u32, Some(px.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);
    ReleaseDC(None, dc);
    if got == 0 {
        return None;
    }
    let has_alpha = px.iter().any(|p| p >> 24 != 0);
    let (mut sum, mut total) = ([0f32; 3], 0f32);
    for &p in &px {
        if has_alpha && p >> 24 < 128 {
            continue;
        }
        let (r, g, b) = (((p >> 16) & 0xff) as f32, ((p >> 8) & 0xff) as f32, (p & 0xff) as f32);
        let k = 0.1 + (r.max(g).max(b) - r.min(g).min(b)) / 255.0;
        sum[0] += r * k;
        sum[1] += g * k;
        sum[2] += b * k;
        total += k;
    }
    (total > 0.0).then(|| {
        let c = |v: f32| (v / total).round().clamp(0.0, 255.0) as u32;
        c(sum[2]) << 16 | c(sum[1]) << 8 | c(sum[0])
    })
}

/// Whether apps are set to the light theme (Settings, Personalization,
/// Colors).
fn light_theme() -> bool {
    let mut v: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    r.is_ok() && v != 0
}

/// A window's title bar text.
pub fn window_title(h: Hwnd) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { wm::GetWindowTextW(hwnd(h), &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// A window's small icon, or 0. Asks the window, briefly, then its class.
pub fn window_icon(h: Hwnd) -> isize {
    unsafe {
        for kind in [wm::ICON_SMALL2, wm::ICON_SMALL, wm::ICON_BIG] {
            let mut icon = 0usize;
            wm::SendMessageTimeoutW(
                hwnd(h),
                wm::WM_GETICON,
                WPARAM(kind as usize),
                LPARAM(0),
                wm::SMTO_ABORTIFHUNG,
                50,
                Some(&mut icon),
            );
            if icon != 0 {
                return icon as isize;
            }
        }
        for which in [wm::GCLP_HICONSM, wm::GCLP_HICON] {
            let icon = wm::GetClassLongPtrW(hwnd(h), which);
            if icon != 0 {
                return icon as isize;
            }
        }
    }
    0
}

/// How much bigger than at 96 DPI display `m` draws things.
pub fn monitor_scale(m: isize) -> f32 {
    let (mut x, mut y) = (96u32, 96u32);
    unsafe {
        let _ = GetDpiForMonitor(mon(m), MDT_EFFECTIVE_DPI, &mut x, &mut y);
    }
    x as f32 / 96.0
}

pub fn set_foreground(h: Hwnd) {
    unsafe {
        let _ = wm::SetForegroundWindow(hwnd(h));
    }
}

/// Ask a window to close, as its close button would.
pub fn close_window(h: Hwnd) {
    unsafe {
        let _ = wm::PostMessageW(hwnd(h), wm::WM_CLOSE, WPARAM(0), LPARAM(0));
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
