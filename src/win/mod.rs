//! Windows integration: watches windows, docks them into the layout, and
//! moves them to where the layout says they belong.

mod sys;

use crate::frost;
use crate::layout::{ease, fullscreen_drop, tab_bar_over, Edge, Layout, Placement, Rect, WinId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sys::*;

/// Every layout motion: split, dock, zoom, close.
const MOTION: Duration = Duration::from_millis(240);
const GAP: i32 = 8;
/// Height of the band above a tab group for its strip of tabs, at 96 DPI.
/// The strip leaves half a gap under it.
const TAB_BAR: f32 = 32.0;
const ANIM_TIMER: usize = 1;
/// Updates the drop zone while a window is being dragged.
const DRAG_TIMER: usize = 2;

/// How the drop zone looks: frosted glass, a touch darker than what's under it.
const ZONE_STYLE: frost::Style = frost::Style { blur: 2, darken: 40, corner: 10.0, border: 60 };
/// The drop zone's frost is captured at 1/ZONE_SCALE size, then blurred.
const ZONE_SCALE: i32 = 8;

/// A small snapshot of a display's work area, taken once when a drag starts
/// and cropped for every drop zone in that drag. Reading the screen waits
/// for the compositor (about a frame, whatever the size), so it happens once,
/// on a worker thread, rather than each time the zone moves.
struct Backdrop {
    area: Rect,
    w: usize,
    h: usize,
    px: Vec<u32>,
}

type SharedBackdrop = Arc<Mutex<Option<Backdrop>>>;

/// Start capturing `area` in the background; the slot fills when it's done.
fn snapshot(area: Rect) -> SharedBackdrop {
    let slot = SharedBackdrop::default();
    let fill = slot.clone();
    std::thread::spawn(move || {
        let (w, h) = ((area.w / ZONE_SCALE).max(1) as usize, (area.h / ZONE_SCALE).max(1) as usize);
        let px = capture_scaled(area, w, h);
        *fill.lock().unwrap() = Some(Backdrop { area, w, h, px });
    });
    slot
}

/// The backdrop under `r`, frosted.
fn frosted(r: Rect, b: &Backdrop) -> Vec<u32> {
    let (sw, sh) = ((r.w / ZONE_SCALE).max(1) as usize, (r.h / ZONE_SCALE).max(1) as usize);
    let (x, y) = ((r.x - b.area.x) / ZONE_SCALE, (r.y - b.area.y) / ZONE_SCALE);
    let mut small = frost::crop(&b.px, b.w, b.h, x as isize, y as isize, sw, sh);
    frost::blur(&mut small, sw, sh, ZONE_STYLE.blur);
    frost::compose(&small, sw, sh, r.w.max(1) as usize, r.h.max(1) as usize, &ZONE_STYLE)
}

type MonId = isize;

/// A fresh, empty arrangement for display `m`.
fn new_layout(m: MonId) -> Layout {
    let mut l = Layout::new(GAP);
    l.tab_bar = (TAB_BAR * monitor_scale(m)).round() as i32;
    l
}

/// A tab strip over a tab group: the window showing under it, and the
/// height of the band it sits in.
struct Bar {
    strip: Hwnd,
    owner: WinId,
    band: i32,
}

/// Where the strip goes in the band above window rect `r`.
fn strip_rect(r: Rect, band: i32) -> Rect {
    let b = tab_bar_over(r, band);
    Rect::new(b.x, b.y, b.w, b.h - GAP / 2)
}

struct Anim {
    start: Instant,
    from: HashMap<WinId, Rect>,
    to: Vec<(WinId, Rect, bool)>,
}

#[derive(Default)]
struct App {
    hwnd: Hwnd,
    layouts: HashMap<MonId, Layout>,
    /// Windows the user minimized, and where to put them back.
    minimized: HashMap<WinId, (MonId, Placement)>,
    /// Visual rect of a window when a user drag began.
    drag_start: HashMap<WinId, Rect>,
    anim: Option<Anim>,
    /// The drop-zone panel, where it's showing (None = hidden), and the
    /// window being dragged, which it sits just under.
    overlay: Hwnd,
    drop_zone: Option<Rect>,
    dragging: Hwnd,
    /// What the screen looked like when the drag began, for the frost.
    backdrop: Option<SharedBackdrop>,
    /// A left-button press on a maximized window (and where), which may turn
    /// into a drag that Lattice has to start itself.
    press: Option<(Hwnd, (i32, i32))>,
    /// A floating window framed to fill its display, and where it was, to
    /// put it back when you step out.
    framed_float: Option<(WinId, Rect)>,
    /// Tab strips in use, and hidden ones to reuse.
    bars: Vec<Bar>,
    spare_strips: Vec<Hwnd>,
    /// Windows' icons, for their tabs.
    icons: HashMap<WinId, isize>,
    /// A left-button press on a tab (its window, and where), which is a
    /// click or the start of dragging the tab out.
    tab_press: Option<(WinId, (i32, i32))>,
    /// A tab pulled off its strip, being carried by the pointer: the
    /// window, and its rect relative to the pointer.
    tearing: Option<(WinId, Rect)>,
    /// The last app window to have the focus, and when it last went away
    /// (closed, hidden or minimized). Windows then hands the focus to
    /// another window by itself, which may be a parked one: that isn't a
    /// request to show it.
    last_fg: Hwnd,
    focus_lost: Option<Instant>,
}

thread_local! {
    static APP: RefCell<App> = RefCell::new(App::default());
}

/// Debug builds append a trace of what Lattice saw and decided to
/// `%LOCALAPPDATA%\Lattice\lattice.log`. Release builds log nothing.
macro_rules! trace {
    ($($t:tt)*) => {
        if cfg!(debug_assertions) {
            trace_line(&format!($($t)*));
        }
    };
}

fn trace_line(s: &str) {
    use std::io::Write;
    let Some(dir) = std::env::var_os("LOCALAPPDATA") else { return };
    let dir = std::path::PathBuf::from(dir).join("Lattice");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("lattice.log")) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let _ = writeln!(f, "{}.{:03} {s}", t.as_secs(), t.subsec_millis());
    }
}

fn with_app(f: impl FnOnce(&mut App)) {
    APP.with(|a| {
        // Skip re-entrant calls rather than panic: a nested message during a
        // window move will see the layout again on the next event.
        if let Ok(mut app) = a.try_borrow_mut() {
            f(&mut app);
        }
    });
}

pub fn run() {
    if !claim_single_instance() {
        message_box("Lattice", "Lattice is already running. Look for its icon in the system tray.");
        return;
    }
    install_rescue_hook();
    set_dpi_aware();
    let hwnd = create_message_window(wndproc);
    let failed = register_hotkeys(hwnd, &HOTKEYS);
    trace!("started; hotkeys taken: {failed:?}");
    add_tray_icon(hwnd, "Lattice: dock, nest and zoom your windows");
    install_event_hooks(on_win_event);
    install_mouse_hook(hwnd, WM_MOUSE_HOOK);
    on_strip_input(on_strip);
    let overlay = create_overlay();
    with_app(|app| {
        app.hwnd = hwnd;
        app.overlay = overlay;
    });
    if !failed.is_empty() {
        message_box(
            "Lattice",
            &format!(
                "These shortcuts are already taken by another app, so Lattice couldn't claim them:\n\n{}",
                failed.join("\n")
            ),
        );
    }
    message_loop();
    remove_tray_icon(hwnd);
}

// ---------------------------------------------------------------------------
// Hotkeys

#[derive(Clone, Copy, Debug)]
enum Cmd {
    ShowAll,
    ZoomOut,
    ZoomIn,
    Frame,
    Prev,
    Next,
    Back,
    Forward,
    Dock(Edge),
    PrevTab,
    NextTab,
    Float,
    FloatAll,
    Quit,
}

const HOTKEYS: [(Cmd, Mods, u32, &str); 16] = [
    (Cmd::ShowAll, Mods::CtrlAlt, VK_SPACE, "Ctrl+Alt+Space  show everything"),
    (Cmd::ZoomOut, Mods::CtrlAlt, VK_UP, "Ctrl+Alt+Up  step out a level"),
    (Cmd::ZoomIn, Mods::CtrlAlt, VK_DOWN, "Ctrl+Alt+Down  step in toward the pointer"),
    (Cmd::Frame, Mods::CtrlAlt, VK_RETURN, "Ctrl+Alt+Enter  frame a window on its own"),
    (Cmd::Prev, Mods::CtrlAlt, VK_LEFT, "Ctrl+Alt+Left  previous space"),
    (Cmd::Next, Mods::CtrlAlt, VK_RIGHT, "Ctrl+Alt+Right  next space"),
    (Cmd::Back, Mods::CtrlAlt, VK_OEM_4, "Ctrl+Alt+[  back"),
    (Cmd::Forward, Mods::CtrlAlt, VK_OEM_6, "Ctrl+Alt+]  forward"),
    (Cmd::Dock(Edge::Left), Mods::CtrlAltShift, VK_LEFT, "Ctrl+Alt+Shift+Left  dock left"),
    (Cmd::Dock(Edge::Right), Mods::CtrlAltShift, VK_RIGHT, "Ctrl+Alt+Shift+Right  dock right"),
    (Cmd::Dock(Edge::Top), Mods::CtrlAltShift, VK_UP, "Ctrl+Alt+Shift+Up  dock top"),
    (Cmd::Dock(Edge::Bottom), Mods::CtrlAltShift, VK_DOWN, "Ctrl+Alt+Shift+Down  dock bottom"),
    (Cmd::PrevTab, Mods::CtrlAlt, VK_PRIOR, "Ctrl+Alt+Page Up  previous tab"),
    (Cmd::NextTab, Mods::CtrlAlt, VK_NEXT, "Ctrl+Alt+Page Down  next tab"),
    (Cmd::Float, Mods::CtrlAlt, VK_F, "Ctrl+Alt+F  float / unfloat window"),
    (Cmd::Quit, Mods::CtrlAltShift, VK_Q, "Ctrl+Alt+Shift+Q  quit Lattice"),
];

const MENU_SHOW_ALL: usize = 100;
const MENU_FLOAT_ALL: usize = 101;
const MENU_QUIT: usize = 102;

fn wndproc(msg: u32, wparam: usize, lparam: isize) -> Option<isize> {
    match msg {
        WM_HOTKEY => {
            if let Some((cmd, ..)) = HOTKEYS.get(wparam) {
                run_cmd(*cmd);
            }
            Some(0)
        }
        WM_TIMER if wparam == ANIM_TIMER => {
            with_app(|app| app.tick());
            Some(0)
        }
        WM_TIMER if wparam == DRAG_TIMER => {
            with_app(|app| app.drag_tick());
            Some(0)
        }
        WM_MOUSE_HOOK => {
            if let Some((ev, pt)) = mouse_message(wparam, lparam) {
                on_mouse(ev, pt);
            }
            Some(0)
        }
        WM_TRAY => {
            if matches!(lparam as u32 & 0xffff, WM_RBUTTONUP | WM_LBUTTONUP) {
                let hwnd = APP.with(|a| a.borrow().hwnd);
                let pick = show_tray_menu(
                    hwnd,
                    &[(MENU_SHOW_ALL, "Show everything"), (MENU_FLOAT_ALL, "Float all windows"), (0, ""), (MENU_QUIT, "Quit Lattice")],
                );
                match pick {
                    MENU_SHOW_ALL => run_cmd(Cmd::ShowAll),
                    MENU_FLOAT_ALL => run_cmd(Cmd::FloatAll),
                    MENU_QUIT => run_cmd(Cmd::Quit),
                    _ => {}
                }
            }
            Some(0)
        }
        WM_ENDSESSION | WM_CLOSE => {
            run_cmd(Cmd::Quit);
            Some(0)
        }
        _ => None,
    }
}

fn run_cmd(cmd: Cmd) {
    with_app(|app| {
        let fg = foreground_window();
        // Most commands act on the display the pointer is on.
        let mon = monitor_at(cursor_pos());
        trace!("cmd {cmd:?} fg {fg:#x} docked on {:?}", app.monitor_of(fg));
        // Every camera motion first puts a framed floating window back;
        // stepping out (or back) does only that.
        let framed = matches!(
            cmd,
            Cmd::ShowAll | Cmd::ZoomOut | Cmd::ZoomIn | Cmd::Prev | Cmd::Next | Cmd::Back | Cmd::Forward
        ) && app.unframe_float();
        match cmd {
            Cmd::ShowAll => app.camera(mon, |l| l.zoom_all()),
            Cmd::ZoomOut if !framed => app.camera(mon, |l| l.zoom_out()),
            Cmd::ZoomIn => {
                let mut under = root_window_at(cursor_pos());
                if let Some(b) = app.bars.iter().find(|b| b.strip == under) {
                    under = b.owner;
                }
                app.camera(mon, |l| l.zoom_in_toward(under));
            }
            Cmd::Frame => {
                if let Some(m) = app.monitor_of(fg) {
                    app.unframe_float();
                    app.camera(m, |l| l.frame(fg));
                } else if app.framed_float.is_some_and(|(id, _)| id == fg) {
                    app.unframe_float();
                } else if is_manageable(fg) {
                    if let Some(r) = visual_rect(fg) {
                        app.frame_float(fg, monitor_of_window(fg), r);
                    }
                }
            }
            Cmd::Prev => app.camera(mon, |l| l.neighbour(-1)),
            Cmd::Next => app.camera(mon, |l| l.neighbour(1)),
            Cmd::Back if !framed => app.camera(mon, |l| l.back()),
            Cmd::Forward => app.camera(mon, |l| l.forward()),
            Cmd::ZoomOut | Cmd::Back => {}
            Cmd::PrevTab | Cmd::NextTab => {
                let dir = if matches!(cmd, Cmd::NextTab) { 1 } else { -1 };
                let next = app.monitor_of(fg).and_then(|m| app.layouts[&m].tab_beside(fg, dir));
                if let Some(next) = next {
                    app.select_tab(next);
                }
            }
            Cmd::Dock(edge) => {
                if is_manageable(fg) {
                    app.forget_framed(fg);
                    let m = monitor_of_window(fg);
                    app.undock(fg);
                    app.dock(fg, m, edge, None);
                }
            }
            Cmd::Float => {
                app.forget_framed(fg);
                if app.monitor_of(fg).is_some() {
                    app.undock(fg);
                    app.apply_all();
                } else if is_manageable(fg) {
                    let m = monitor_of_window(fg);
                    app.dock(fg, m, Edge::Right, None);
                }
            }
            Cmd::FloatAll => app.float_all(),
            Cmd::Quit => {
                app.float_all();
                post_quit();
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Window events

fn on_win_event(event: u32, hwnd: Hwnd) {
    if !(EVENT_OBJECT_DESTROY..=EVENT_OBJECT_HIDE).contains(&event) && event != EVENT_OBJECT_NAMECHANGE {
        trace!("event {event:#x} hwnd {hwnd:#x}");
    }
    with_app(|app| match event {
        EVENT_SYSTEM_MOVESIZESTART => {
            // Moving a framed floating window by hand ends the framing.
            app.forget_framed(hwnd);
            if let Some(r) = visual_rect(hwnd) {
                app.drag_start.insert(hwnd, r);
                app.backdrop = Some(snapshot(work_area(monitor_of_window(hwnd))));
                set_timer(app.hwnd, DRAG_TIMER, 16);
            }
            // A drag interrupts any motion in flight.
            app.finish_anim();
        }
        EVENT_SYSTEM_MOVESIZEEND => {
            kill_timer(app.hwnd, DRAG_TIMER);
            app.show_drop_zone(None);
            app.backdrop = None;
            app.drag_ended(hwnd);
        }
        EVENT_SYSTEM_MINIMIZESTART => {
            app.focus_gone(hwnd);
            app.forget_framed(hwnd);
            if let Some(m) = app.monitor_of(hwnd) {
                if let Some(p) = app.layouts[&m].placement_of(hwnd) {
                    app.minimized.insert(hwnd, (m, p));
                }
                app.undock(hwnd);
                app.apply(m);
            }
        }
        EVENT_SYSTEM_MINIMIZEEND => {
            if let Some((m, p)) = app.minimized.remove(&hwnd) {
                app.layouts.entry(m).or_insert_with(|| new_layout(m)).restore(hwnd, p);
                app.apply(m);
            }
        }
        EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE => {
            app.focus_gone(hwnd);
            app.icons.remove(&hwnd);
            app.minimized.remove(&hwnd);
            app.forget_framed(hwnd);
            if let Some(m) = app.monitor_of(hwnd) {
                app.undock(hwnd);
                app.apply(m);
            }
        }
        EVENT_SYSTEM_FOREGROUND => app.focus_changed(hwnd),
        EVENT_OBJECT_NAMECHANGE if app.layouts.values().any(|l| l.group(hwnd).is_some()) => app.refresh_bars(),
        _ => {}
    });
}

fn on_strip(strip: Hwnd, ev: StripEvent, pt: (i32, i32)) {
    with_app(|app| app.strip_input(strip, ev, pt));
}

/// How far (px) the pointer must move with the button down to count as a drag.
const DRAG_THRESHOLD: i32 = 6;

/// With Snap off, Windows won't drag a maximized window at all. So Lattice
/// watches (via the mouse hook thread) for a press on a maximized window
/// that starts to move, and restores it under the pointer to carry on as an
/// ordinary drag.
fn on_mouse(ev: MouseEvent, pt: (i32, i32)) {
    with_app(|app| match ev {
        // A torn-off tab follows the pointer until the button comes up,
        // then drops like any dragged window.
        MouseEvent::Move if app.tearing.is_some() => {
            let (w, off) = app.tearing.expect("checked");
            place_window(w, Rect::new(pt.0 + off.x, pt.1 + off.y, off.w, off.h));
        }
        MouseEvent::LeftUp if app.tearing.is_some() => {
            let (w, _) = app.tearing.take().expect("checked");
            kill_timer(app.hwnd, DRAG_TIMER);
            app.show_drop_zone(None);
            app.backdrop = None;
            app.drag_ended(w);
        }
        MouseEvent::LeftDown => {
            let w = root_window_at(pt);
            app.press = (is_maximized(w) && is_manageable(w)).then_some((w, pt));
        }
        MouseEvent::LeftUp => app.press = None,
        MouseEvent::Move => {
            if let Some((w, at)) = app.press {
                if (pt.0 - at.0).abs() + (pt.1 - at.1).abs() > DRAG_THRESHOLD {
                    app.press = None;
                    restore_drag(w, at);
                }
            }
        }
    });
}

/// A press on maximized window `w` at `grab` has started moving: if it was
/// on the title bar, and Windows isn't handling it (Snap off), restore the
/// window and drag it.
fn restore_drag(w: Hwnd, grab: (i32, i32)) {
    if snap_enabled() || !left_button_down() || !is_maximized(w) || !on_title_bar(w, grab) {
        return;
    }
    trace!("restore-drag {w:#x} grabbed at {grab:?}");
    restore_and_drag(w, grab, cursor_pos());
}

impl App {

    fn monitor_of(&self, id: WinId) -> Option<MonId> {
        self.layouts.iter().find(|(_, l)| l.contains(id)).map(|(m, _)| *m)
    }

    fn undock(&mut self, id: WinId) {
        for l in self.layouts.values_mut() {
            l.remove(id);
        }
        rescue_list_remove(id);
    }

    fn dock(&mut self, id: WinId, mon: MonId, edge: Edge, target: Option<WinId>) {
        restore_if_maximized(id);
        self.layouts.entry(mon).or_insert_with(|| new_layout(mon)).dock(id, edge, target);
        rescue_list_add(id);
        self.apply(mon);
    }

    /// Add `id` as a tab in docked window `onto`'s spot on display `mon`.
    fn tab(&mut self, id: WinId, mon: MonId, onto: WinId) {
        let Some(l) = self.layouts.get_mut(&mon) else { return };
        restore_if_maximized(id);
        if l.tab(id, onto) {
            rescue_list_add(id);
        }
        self.apply(mon);
    }

    /// Show tab `id` in its group, at once, and give it the focus.
    fn select_tab(&mut self, id: WinId) {
        let Some(m) = self.monitor_of(id) else { return };
        self.finish_anim();
        let area = work_area(m);
        let l = self.layouts.get_mut(&m).expect("docked");
        let old = l.group(id).map(|g| g.shown());
        trace!("select tab {id:#x} (was {old:?})");
        if l.reveal(id) {
            let to = l.layout(area).into_iter().find(|(w, _)| *w == id).map(|(_, r)| r);
            match (to, old) {
                // Swapped in place: no need to animate.
                (Some(r), Some(old)) if l.is_visible(id) && old != id => {
                    place_window(id, r);
                    park_window(old);
                }
                _ => {
                    set_foreground(id);
                    self.apply(m);
                    return;
                }
            }
        }
        set_foreground(id);
        self.refresh_bars();
    }

    /// The tab `id` was pulled off its strip at `pt`: take it out of its
    /// group and carry on dragging it like any window, to dock or float.
    ///
    /// Lattice carries it itself (see `on_mouse`): the button went down on
    /// the strip, not the window, so Windows won't drag the window.
    fn tear_tab(&mut self, id: WinId, pt: (i32, i32)) {
        let Some(m) = self.monitor_of(id) else { return };
        self.finish_anim();
        let Some(r) = self.layouts[&m].layout(work_area(m)).into_iter().find(|(w, _)| *w == id).map(|(_, r)| r) else {
            return;
        };
        trace!("tear tab {id:#x} at {pt:?}");
        self.undock(id);
        self.apply(m);
        // A bit smaller than the spot it filled, hanging from the pointer
        // by its title bar.
        let (w, h) = ((r.w * 2 / 3).max(r.w.min(640)), (r.h * 2 / 3).max(r.h.min(480)));
        let grab = (16.0 * monitor_scale(m)) as i32;
        let at = Rect::new(pt.0 - w / 2, pt.1 - grab, w, h);
        place_window(id, at);
        set_foreground(id);
        self.tearing = Some((id, Rect::new(-w / 2, -grab, w, h)));
        self.drag_start.insert(id, at);
        self.backdrop = Some(snapshot(work_area(m)));
        set_timer(self.hwnd, DRAG_TIMER, 16);
    }

    fn strip_input(&mut self, strip: Hwnd, ev: StripEvent, pt: (i32, i32)) {
        let Some(owner) = self.bars.iter().find(|b| b.strip == strip).map(|b| b.owner) else { return };
        let Some(m) = self.monitor_of(owner) else { return };
        let tab = strip_tab_at(strip, pt).and_then(|i| self.layouts[&m].group(owner)?.wins.get(i).copied());
        match ev {
            StripEvent::Down => self.tab_press = tab.map(|w| (w, pt)),
            StripEvent::Drag => {
                if let Some((w, at)) = self.tab_press {
                    if (pt.0 - at.0).abs() + (pt.1 - at.1).abs() > DRAG_THRESHOLD {
                        self.tab_press = None;
                        self.tear_tab(w, pt);
                    }
                }
            }
            StripEvent::Up => {
                if let Some((w, _)) = self.tab_press.take() {
                    self.select_tab(w);
                }
            }
            StripEvent::Middle => {
                if let Some(w) = tab {
                    close_window(w);
                }
            }
        }
    }

    /// Window `hwnd` got the focus. If it's docked out of sight (a hidden
    /// tab, or off camera), the user went to it, by Alt+Tab or the taskbar:
    /// bring it into view.
    fn focus_changed(&mut self, hwnd: Hwnd) {
        let handed_on = self.focus_lost.take().is_some_and(|t| t.elapsed() < Duration::from_millis(500));
        if is_manageable(hwnd) {
            self.last_fg = hwnd;
        }
        if let Some(m) = self.monitor_of(hwnd).filter(|_| !handed_on) {
            if self.layouts.get_mut(&m).is_some_and(|l| l.reveal(hwnd)) {
                trace!("revealed {hwnd:#x}");
                self.apply(m);
                return;
            }
        }
        self.refresh_bars();
    }

    /// Window `hwnd` closed, hid or minimized.
    fn focus_gone(&mut self, hwnd: Hwnd) {
        if hwnd == self.last_fg {
            self.focus_lost = Some(Instant::now());
        }
    }

    /// Bring every tab strip up to date: one over each tab group, showing
    /// its tabs, where its group is (unless the group is mid-motion, when
    /// `tick` moves it).
    fn refresh_bars(&mut self) {
        let fg = foreground_window();
        let mut wanted = Vec::new();
        for (&m, l) in &self.layouts {
            let rects = l.layout(work_area(m));
            for g in l.groups() {
                let owner = g.shown();
                let r = rects.iter().find(|(w, _)| *w == owner).map(|(_, r)| *r);
                wanted.push((g.wins.clone(), g.active, r, l.is_visible(owner), l.tab_bar));
            }
        }
        let mut old = std::mem::take(&mut self.bars);
        for (wins, active, r, visible, band) in wanted {
            let owner = wins[active];
            let strip = match old.iter().position(|b| wins.contains(&b.owner)) {
                Some(i) => old.swap_remove(i).strip,
                None => self.spare_strips.pop().unwrap_or_else(create_strip),
            };
            let strip = if is_window(strip) { strip } else { create_strip() };
            let tabs = wins
                .iter()
                .map(|&w| Tab { title: window_title(w), icon: *self.icons.entry(w).or_insert_with(|| window_icon(w)) })
                .collect();
            set_strip(strip, tabs, active, fg == owner);
            let moving = self.anim.as_ref().is_some_and(|a| a.to.iter().any(|t| t.0 == owner));
            if let (false, Some(r)) = (moving, r) {
                place_strip(strip, strip_rect(r, band), owner, visible);
            }
            self.bars.push(Bar { strip, owner, band });
        }
        for b in old {
            place_strip(b.strip, Rect::new(0, 0, 0, 0), 0, false);
            self.spare_strips.push(b.strip);
        }
    }

    /// Window `id` has been put at `r`: bring its tab strip along.
    fn follow(&self, id: WinId, r: Rect, show: bool) {
        if let Some(b) = self.bars.iter().find(|b| b.owner == id) {
            place_strip(b.strip, strip_rect(r, b.band), id, show);
        }
    }

    /// The docked window a window being dragged would join as a tab if
    /// dropped now, and its display.
    fn tab_target(&self, hwnd: Hwnd, pt: (i32, i32)) -> Option<(MonId, WinId)> {
        if !is_manageable(hwnd) {
            return None;
        }
        let mon = monitor_at(pt);
        let onto = self.layouts.get(&mon)?.tab_target(work_area(mon), pt.0, pt.1, hwnd)?;
        Some((mon, onto))
    }

    /// Fill display `mon` with floating window `id`, remembering that it
    /// was at `from`.
    fn frame_float(&mut self, id: WinId, mon: MonId, from: Rect) {
        self.unframe_float();
        restore_if_maximized(id);
        self.framed_float = Some((id, from));
        self.animate(vec![(id, framed_rect(mon), true)]);
    }

    /// Put the framed floating window back where it was. False if none was.
    fn unframe_float(&mut self) -> bool {
        let Some((id, r)) = self.framed_float.take() else { return false };
        if is_window(id) {
            self.animate(vec![(id, r, true)]);
        }
        true
    }

    /// `id` has been moved some other way: it's no longer framed.
    fn forget_framed(&mut self, id: WinId) {
        if self.framed_float.is_some_and(|(f, _)| f == id) {
            self.framed_float = None;
        }
    }

    /// The display a window being dragged would fill if dropped now.
    fn fullscreen_target(&self, hwnd: Hwnd, pt: (i32, i32)) -> Option<MonId> {
        let mon = monitor_at(pt);
        (is_manageable(hwnd) && fullscreen_drop(work_area(mon), pt.0, pt.1)).then_some(mon)
    }

    fn camera(&mut self, mon: MonId, f: impl FnOnce(&mut Layout)) {
        if let Some(l) = self.layouts.get_mut(&mon) {
            f(l);
            self.apply(mon);
        }
    }

    /// Where a window being dragged would dock if dropped now: the display,
    /// and the target and edge to pass to `dock`. None means it would float.
    fn drop_target(&self, hwnd: Hwnd, pt: (i32, i32)) -> Option<(MonId, Option<WinId>, Edge)> {
        if !is_manageable(hwnd) {
            return None;
        }
        let mon = monitor_at(pt);
        let area = work_area(mon);
        let target = match self.layouts.get(&mon) {
            Some(l) => l.dock_target(area, pt.0, pt.1, hwnd),
            None => new_layout(mon).dock_target(area, pt.0, pt.1, hwnd),
        };
        target.map(|(target, edge)| (mon, target, edge))
    }

    /// While a window is being dragged by its title bar, show where it
    /// would dock if dropped now.
    fn drag_tick(&mut self) {
        let Some((&hwnd, &start)) = self.drag_start.iter().next() else {
            self.show_drop_zone(None);
            return;
        };
        self.dragging = hwnd;
        let Some(now) = visual_rect(hwnd) else { return };
        let resizing = (now.w - start.w).abs() > 2 || (now.h - start.h).abs() > 2;
        let pt = cursor_pos();
        let zone = if resizing {
            None
        } else if let Some(mon) = self.fullscreen_target(hwnd, pt) {
            Some(framed_rect(mon))
        } else if let Some((mon, onto)) = self.tab_target(hwnd, pt) {
            self.layouts[&mon].slot(onto, work_area(mon))
        } else {
            self.drop_target(hwnd, pt).and_then(|(mon, target, edge)| {
                let area = work_area(mon);
                let l = self.layouts.get(&mon).cloned().unwrap_or_else(|| new_layout(mon));
                l.preview(hwnd, edge, target, area)
            })
        };
        self.show_drop_zone(zone);
    }

    fn show_drop_zone(&mut self, zone: Option<Rect>) {
        // Drawn once each time the zone moves, from the drag's backdrop.
        if zone == self.drop_zone {
            return;
        }
        let Some(r) = zone else {
            self.drop_zone = None;
            hide_overlay(self.overlay);
            return;
        };
        let Some(slot) = self.backdrop.clone() else { return };
        let guard = slot.lock().unwrap();
        // Still capturing: try again next tick.
        let Some(b) = guard.as_ref() else { return };
        let a = b.area;
        if r.x < a.x || r.y < a.y || r.x + r.w > a.x + a.w || r.y + r.h > a.y + a.h {
            // The zone is on another display: snapshot that one instead.
            drop(guard);
            self.backdrop = Some(snapshot(work_area(monitor_at((r.x + r.w / 2, r.y + r.h / 2)))));
            return;
        }
        let t = Instant::now();
        let px = frosted(r, b);
        drop(guard);
        show_overlay(self.overlay, r, &px, self.dragging);
        self.drop_zone = zone;
        trace!("drop zone {}x{} drawn in {:?}", r.w, r.h, t.elapsed());
    }

    fn drag_ended(&mut self, hwnd: Hwnd) {
        let Some(start) = self.drag_start.remove(&hwnd) else { return };
        let Some(now) = visual_rect(hwnd) else { return };
        let pt = cursor_pos();
        let docked_on = self.monitor_of(hwnd);
        let resized = (now.w - start.w).abs() > 2 || (now.h - start.h).abs() > 2;
        trace!("drag end {hwnd:#x}: {start:?} -> {now:?}, cursor {pt:?}, docked on {docked_on:?}, resized {resized}");

        if resized {
            // Dragged a border: move the seam so the neighbour follows.
            if let Some(m) = docked_on {
                let area = work_area(m);
                if let Some(l) = self.layouts.get_mut(&m) {
                    l.apply_resize(hwnd, now, area);
                }
                self.apply(m);
            }
            return;
        }

        // Dropped on the middle of the top: fill the display. A docked
        // window is framed where it sits; any other floats framed, and
        // stepping out puts it back where the drag began.
        if let Some(mon) = self.fullscreen_target(hwnd, pt) {
            trace!("drop fullscreen on {mon:#x}");
            match docked_on {
                Some(m) if m == mon => self.camera(m, |l| l.frame(hwnd)),
                _ => {
                    if let Some(m) = docked_on {
                        self.undock(hwnd);
                        self.apply(m);
                    }
                    self.frame_float(hwnd, mon, start);
                }
            }
            return;
        }

        // Dropped on a docked window's title bar, or a strip of tabs: join
        // that window's spot as a tab.
        if let Some((mon, onto)) = self.tab_target(hwnd, pt) {
            trace!("drop as tab on {onto:#x}");
            if let Some(old) = docked_on.filter(|&m| m != mon) {
                self.undock(hwnd);
                self.apply(old);
            }
            self.tab(hwnd, mon, onto);
            return;
        }

        // Dragged the title bar: dock where it was dropped, or float it.
        let drop = self.drop_target(hwnd, pt);
        trace!("drop target {drop:?}");
        match drop {
            Some((mon, target, edge)) => {
                if let Some(old) = docked_on {
                    self.undock(hwnd);
                    if old != mon {
                        self.apply(old);
                    }
                }
                self.dock(hwnd, mon, edge, target);
            }
            _ => {
                if let Some(m) = docked_on {
                    self.undock(hwnd);
                    self.apply(m);
                }
            }
        }
    }

    /// Move every window on display `mon` to where the layout says.
    fn apply(&mut self, mon: MonId) {
        let area = work_area(mon);
        let Some(l) = self.layouts.get(&mon) else { return };
        let targets: Vec<_> = l
            .layout(area)
            .into_iter()
            .map(|(id, r)| (id, r, l.is_visible(id)))
            .collect();
        trace!("apply {mon:#x}: {targets:?}");
        if l.is_empty() {
            self.layouts.remove(&mon);
        }
        self.animate(targets);
        self.refresh_bars();
    }

    fn apply_all(&mut self) {
        let mons: Vec<_> = self.layouts.keys().copied().collect();
        for m in mons {
            self.apply(m);
        }
    }

    fn animate(&mut self, targets: Vec<(WinId, Rect, bool)>) {
        // Merge with whatever is still moving so nothing jumps.
        let mut to = self.anim.take().map(|a| a.to).unwrap_or_default();
        for t in targets {
            to.retain(|x| x.0 != t.0);
            // A parked window staying out of sight (a hidden tab, say) stays
            // put, rather than showing where it would be while others move.
            if !t.2 && visual_rect(t.0).is_some_and(|r| r.x <= -20_000) {
                continue;
            }
            to.push(t);
        }
        to.retain(|x| is_window(x.0));
        let from = to
            .iter()
            .filter_map(|(id, target, _)| {
                let now = visual_rect(*id)?;
                // A parked window appears in place rather than flying in from
                // thousands of pixels away.
                Some((*id, if now.x <= -20_000 { *target } else { now }))
            })
            .collect();
        self.anim = Some(Anim { start: Instant::now(), from, to });
        set_timer(self.hwnd, ANIM_TIMER, 8);
        self.tick();
    }

    fn tick(&mut self) {
        let Some(anim) = &self.anim else { return };
        let t = anim.start.elapsed().as_secs_f32() / MOTION.as_secs_f32();
        let k = ease(t);
        for (id, to, _) in &anim.to {
            let from = anim.from.get(id).copied().unwrap_or(*to);
            let r = Rect::lerp(from, *to, k);
            place_window(*id, r);
            self.follow(*id, r, true);
        }
        if t >= 1.0 {
            self.finish_anim();
        }
    }

    fn finish_anim(&mut self) {
        kill_timer(self.hwnd, ANIM_TIMER);
        if let Some(anim) = self.anim.take() {
            for (id, r, visible) in anim.to {
                // Windows outside the camera are parked well off every display.
                if visible {
                    place_window(id, r);
                } else {
                    park_window(id);
                }
                self.follow(id, r, visible);
            }
        }
    }

    /// Release every window: back to a normal desktop, nothing lost.
    fn float_all(&mut self) {
        self.unframe_float();
        self.finish_anim();
        for (&m, l) in self.layouts.iter_mut() {
            l.zoom_all();
            let area = work_area(m);
            for (id, r) in l.layout(area) {
                place_window(id, r);
            }
        }
        self.layouts.clear();
        self.minimized.clear();
        rescue_list_clear();
        self.refresh_bars();
    }
}

/// Where a window framed on its own sits on display `mon`: the work area,
/// with the same margin a docked window framed there gets.
fn framed_rect(mon: MonId) -> Rect {
    let a = work_area(mon);
    Rect::new(a.x + GAP / 2, a.y + GAP / 2, a.w - GAP, a.h - GAP)
}
