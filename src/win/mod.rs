//! Windows integration: watches windows, docks them into the layout, and
//! moves them to where the layout says they belong.

mod sys;

use crate::frost;
use crate::layout::{
    card_parts, ease, fit, fullscreen_drop, overview_area, tab_bar_over, tab_rects, Edge, Layout, Placement, Rect, WinId,
};
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

/// Moves the overview's pictures.
const OVERVIEW_TIMER: usize = 3;
/// Stepping out to the overview and back in, a touch slower than other
/// motion as everything on the display moves at once.
const OVERVIEW_MOTION: Duration = Duration::from_millis(300);
/// The overview's cards, at 96 DPI: the gap between them, the height of the
/// caption with their tabs, and the margin round the window's picture.
const OVERVIEW_GAP: f32 = 24.0;
const CAPTION: f32 = 32.0;
const CARD_PAD: f32 = 6.0;
/// A card's picture can't get smaller than this (at 96 DPI) and still be
/// told apart; below it, the card shows the app's icon instead.
const MIN_PICTURE: (f32, f32) = (140.0, 80.0);

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

/// A card in the overview: a spot in the arrangement, with a live picture
/// of the window showing there flying between `from` and `to`.
struct OverviewCell {
    id: WinId,
    /// Its tabs (just `id` for a lone window), which is showing, and where
    /// each sits in the caption.
    wins: Vec<WinId>,
    active: usize,
    tabs: Vec<(WinId, Rect)>,
    card: Rect,
    body: Rect,
    /// The window's visible part, relative to its window rect.
    crop: Rect,
    thumb: isize,
    from: Rect,
    to: Rect,
    /// Too small for a picture to be any use: show the app's icon.
    small: bool,
}

/// The overview: every docked window on a display at once, as cards.
struct Overview {
    mon: MonId,
    area: Rect,
    cells: Vec<OverviewCell>,
    hover: Option<usize>,
    /// When the pictures set off, while they're flying.
    motion: Option<Instant>,
    /// Putting it away: who gets the focus after.
    closing: Option<Hwnd>,
    /// Who had the focus before it opened.
    return_to: Hwnd,
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
    /// The overview's window, and the overview while it's up.
    overview_host: Hwnd,
    overview: Option<Overview>,
    /// Apps' colours, from their icons, for their cards in the overview.
    tints: HashMap<WinId, Option<u32>>,
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
    on_overview_input(|ev, pt| with_app(|app| app.overview_input(ev, pt)));
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
const MENU_OVERVIEW: usize = 103;
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
        WM_TIMER if wparam == OVERVIEW_TIMER => {
            with_app(|app| app.overview_tick());
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
                    &[
                        (MENU_SHOW_ALL, "Show everything"),
                        (MENU_OVERVIEW, "Overview"),
                        (MENU_FLOAT_ALL, "Float all windows"),
                        (0, ""),
                        (MENU_QUIT, "Quit Lattice"),
                    ],
                );
                match pick {
                    MENU_SHOW_ALL => run_cmd(Cmd::ShowAll),
                    MENU_OVERVIEW => with_app(|app| {
                        app.unframe_float();
                        app.open_overview(monitor_at(cursor_pos()));
                    }),
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
        // With the overview up, showing everything (or stepping in) steps
        // in from it; stepping out does nothing more. Anything else puts it
        // away first.
        if let Some(ov) = &app.overview {
            let back = ov.return_to;
            match cmd {
                Cmd::ShowAll | Cmd::ZoomIn => app.close_overview(back, |l| l.zoom_all()),
                Cmd::ZoomOut => {}
                _ => app.end_overview(back),
            }
            if matches!(cmd, Cmd::ShowAll | Cmd::ZoomIn | Cmd::ZoomOut) {
                return;
            }
        }
        // Already showing everything: step out once more, to the overview.
        let at_top = app.layouts.get(&mon).is_some_and(|l| l.camera.is_empty());
        // Every camera motion first puts a framed floating window back;
        // stepping out (or back) does only that.
        let framed = matches!(
            cmd,
            Cmd::ShowAll | Cmd::ZoomOut | Cmd::ZoomIn | Cmd::Prev | Cmd::Next | Cmd::Back | Cmd::Forward
        ) && app.unframe_float();
        match cmd {
            Cmd::ShowAll | Cmd::ZoomOut if at_top && !framed => app.open_overview(mon),
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
            app.tints.remove(&hwnd);
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
        if self.overview.as_ref().is_some_and(|o| o.mon == mon) {
            self.rebuild_overview();
        }
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

    // -----------------------------------------------------------------------
    // Overview

    /// Step back past everything on display `mon`: every docked window as a
    /// card with a live picture, flying out from where it is now.
    fn open_overview(&mut self, mon: MonId) {
        if self.layouts.get(&mon).is_none_or(|l| l.is_empty()) {
            return;
        }
        self.finish_anim();
        if !is_window(self.overview_host) {
            self.overview_host = create_overview();
        }
        let area = work_area(mon);
        set_overview(self.overview_host, vec![], None);
        show_overview(self.overview_host, area);
        let return_to = self.last_fg;
        let cells = self.overview_cells(mon, area);
        // Fly out from where each window is now; parked ones from where
        // they'd be, off-screen, so they come into view as the camera pulls
        // back.
        let now: HashMap<WinId, Rect> = self.layouts[&mon].layout(area).into_iter().collect();
        let mut cells = cells;
        for c in &mut cells {
            c.from = visual_rect(c.id).filter(|r| r.x > -20_000).or_else(|| now.get(&c.id).copied()).unwrap_or(c.to);
        }
        trace!("overview of {mon:#x}: {} cards", cells.len());
        self.overview = Some(Overview { mon, area, cells, hover: None, motion: Some(Instant::now()), closing: None, return_to });
        set_timer(self.hwnd, OVERVIEW_TIMER, 8);
        self.overview_tick();
    }

    /// The overview's cards for display `mon`, each with its thumbnail
    /// registered and sitting where it ends up.
    fn overview_cells(&mut self, mon: MonId, area: Rect) -> Vec<OverviewCell> {
        let s = monitor_scale(mon);
        let px = |v: f32| (v * s).round() as i32;
        let Some(l) = self.layouts.get(&mon) else { return vec![] };
        let mut out = Vec::new();
        for (id, card) in l.overview(overview_area(area), px(OVERVIEW_GAP)) {
            let (cap, body) = card_parts(card, px(CAPTION), px(CARD_PAD));
            // Too small even for a caption: just the app's icon, in the
            // middle of the card.
            let tiny = card.h < px(CAPTION) * 2 || card.w < px(CAPTION) * 3;
            let body = if tiny { card } else { body };
            let (wins, active) = l.group(id).map_or((vec![id], 0), |g| (g.wins.clone(), g.active));
            let chips = if wins.len() == 1 {
                vec![Rect::new(cap.x + px(2.0), cap.y, cap.w - px(4.0), cap.h)]
            } else {
                tab_rects(cap.w, cap.h, wins.len(), px(4.0), px(220.0))
                    .into_iter()
                    .map(|r| Rect::new(cap.x + r.x, cap.y + r.y, r.w, r.h))
                    .collect()
            };
            let tabs = if tiny { vec![] } else { wins.iter().copied().zip(chips).collect() };
            let crop = window_crop(id).unwrap_or(Rect::new(0, 0, body.w, body.h));
            let to = fit(crop.w, crop.h, body);
            let small = tiny || body.w < px(MIN_PICTURE.0) || body.h < px(MIN_PICTURE.1);
            let thumb = thumb_register(self.overview_host, id);
            out.push(OverviewCell { id, wins, active, tabs, card, body, crop, thumb, from: to, to, small });
        }
        out
    }

    /// The cards as the overview window draws them, in its own coordinates.
    fn overview_cards(&mut self) -> Vec<Card> {
        let Some(ov) = &self.overview else { return vec![] };
        let s = monitor_scale(ov.mon);
        let local = |r: Rect| Rect::new(r.x - ov.area.x, r.y - ov.area.y, r.w, r.h);
        let mut cards = Vec::new();
        for c in &ov.cells {
            let tabs = c
                .tabs
                .iter()
                .map(|&(w, r)| {
                    let icon = *self.icons.entry(w).or_insert_with(|| window_icon(w));
                    (local(r), Tab { title: window_title(w), icon })
                })
                .collect();
            let icon = *self.icons.entry(c.id).or_insert_with(|| window_icon(c.id));
            let tint = *self.tints.entry(c.id).or_insert_with(|| icon_colour(icon));
            let icon_only = c.small.then(|| {
                let size = ((32.0 * s) as i32).min(c.body.w).min(c.body.h).max(1);
                let b = local(c.body);
                (Rect::new(b.x + (b.w - size) / 2, b.y + (b.h - size) / 2, size, size), icon)
            });
            let (cap, _) = card_parts(c.card, (CAPTION * s).round() as i32, 0);
            cards.push(Card { rect: local(c.card), caption: local(cap), tabs, active: c.active, tint, icon_only });
        }
        cards
    }

    /// Move the overview's thumbnails along their flight, and settle it
    /// (or put it away) when they land.
    fn overview_tick(&mut self) {
        let Some(ov) = &self.overview else { return };
        let t = ov.motion.map_or(1.0, |m| m.elapsed().as_secs_f32() / OVERVIEW_MOTION.as_secs_f32());
        let k = ease(t);
        let settled = t >= 1.0 && ov.closing.is_none();
        for c in &ov.cells {
            let r = Rect::lerp(c.from, c.to, k);
            let local = Rect::new(r.x - ov.area.x, r.y - ov.area.y, r.w, r.h);
            // The window may be changing size under it as it flies back in.
            let crop = if ov.closing.is_some() { window_crop(c.id).unwrap_or(c.crop) } else { c.crop };
            thumb_set(c.thumb, crop, local, !(settled && c.small));
        }
        if t < 1.0 {
            return;
        }
        kill_timer(self.hwnd, OVERVIEW_TIMER);
        if let Some(focus) = ov.closing {
            self.end_overview(focus);
        } else {
            let hover = ov.hover;
            self.overview.as_mut().expect("checked").motion = None;
            let cards = self.overview_cards();
            set_overview(self.overview_host, cards, hover);
        }
    }

    /// Fly back into the arrangement, after `f` has moved the camera (or
    /// not, to go back to where it was), and give `focus` the focus.
    fn close_overview(&mut self, focus: Hwnd, f: impl FnOnce(&mut Layout)) {
        let Some(ov) = &self.overview else { return };
        if ov.closing.is_some() {
            return;
        }
        let (mon, area) = (ov.mon, ov.area);
        let t = ov.motion.map_or(1.0, |m| ease(m.elapsed().as_secs_f32() / OVERVIEW_MOTION.as_secs_f32()));
        let now: Vec<Rect> = ov.cells.iter().map(|c| Rect::lerp(c.from, c.to, t)).collect();
        self.overview.as_mut().expect("checked").closing = Some(focus);
        // The windows go straight to their places, behind the overview,
        // while their pictures fly there.
        if let Some(l) = self.layouts.get_mut(&mon) {
            f(l);
        }
        self.apply(mon);
        self.finish_anim();
        let l = self.layouts.get(&mon);
        let to: HashMap<WinId, Rect> = l.map(|l| l.layout(area).into_iter().collect()).unwrap_or_default();
        let shown: Vec<WinId> = ov_ids(&self.overview).map(|id| l.and_then(|l| l.group(id)).map_or(id, |g| g.shown())).collect();
        let host = self.overview_host;
        let ov = self.overview.as_mut().expect("checked");
        for ((c, now), shown) in ov.cells.iter_mut().zip(now).zip(shown) {
            // Picked another tab: fly in with that one's picture.
            if shown != c.id {
                thumb_unregister(c.thumb);
                c.thumb = thumb_register(host, shown);
                c.crop = window_crop(shown).unwrap_or(c.crop);
                c.id = shown;
            }
            c.from = now;
            c.to = to.get(&c.id).copied().unwrap_or(now);
        }
        ov.motion = Some(Instant::now());
        set_overview(self.overview_host, vec![], None);
        set_timer(self.hwnd, OVERVIEW_TIMER, 8);
        self.overview_tick();
    }

    /// Put the overview away at once.
    fn end_overview(&mut self, focus: Hwnd) {
        let Some(ov) = self.overview.take() else { return };
        kill_timer(self.hwnd, OVERVIEW_TIMER);
        for c in ov.cells {
            thumb_unregister(c.thumb);
        }
        // Focus first: hiding the overview first would hand the focus to
        // whatever window was last active, which may be off camera, and
        // Lattice would go to show it.
        if focus != 0 && is_window(focus) {
            set_foreground(focus);
        }
        hide_overview(self.overview_host);
    }

    /// Something changed on the overview's display while it's up (a window
    /// closed, say): lay the cards out again.
    fn rebuild_overview(&mut self) {
        let Some(ov) = &mut self.overview else { return };
        if ov.closing.is_some() {
            return;
        }
        let (mon, area) = (ov.mon, ov.area);
        for c in std::mem::take(&mut ov.cells) {
            thumb_unregister(c.thumb);
        }
        if self.layouts.get(&mon).is_none_or(|l| l.is_empty()) {
            let to = self.overview.as_ref().expect("checked").return_to;
            self.end_overview(to);
            return;
        }
        let cells = self.overview_cells(mon, area);
        let ov = self.overview.as_mut().expect("checked");
        ov.cells = cells;
        ov.motion = None;
        ov.hover = None;
        self.overview_tick();
    }

    /// The card under screen point `pt`, and the window in it that's meant:
    /// the tab under the pointer, or the one showing.
    fn overview_hit(&self, pt: (i32, i32)) -> Option<(usize, WinId)> {
        let ov = self.overview.as_ref()?;
        let i = ov.cells.iter().position(|c| c.card.contains(pt.0, pt.1))?;
        let c = &ov.cells[i];
        let tab = c.tabs.iter().find(|(_, r)| c.wins.len() > 1 && r.contains(pt.0, pt.1)).map(|&(w, _)| w);
        Some((i, tab.unwrap_or(c.id)))
    }

    fn overview_input(&mut self, ev: OverviewEvent, pt: (i32, i32)) {
        let Some(ov) = &self.overview else { return };
        if ov.closing.is_some() {
            return;
        }
        let return_to = ov.return_to;
        let hit = self.overview_hit(pt);
        trace!("overview {ev:?} at {pt:?}: {hit:?}");
        match ev {
            OverviewEvent::Move => {
                let hover = hit.map(|(i, _)| i);
                let ov = self.overview.as_mut().expect("checked");
                if hover != ov.hover {
                    ov.hover = hover;
                    if ov.motion.is_none() {
                        let cards = self.overview_cards();
                        set_overview(self.overview_host, cards, hover);
                    }
                }
            }
            OverviewEvent::Click => match hit {
                Some((_, w)) => self.close_overview(w, |l| {
                    l.select(w);
                    l.frame(w);
                }),
                None => self.close_overview(return_to, |_| {}),
            },
            OverviewEvent::Middle => {
                if let Some((_, w)) = hit {
                    close_window(w);
                }
            }
            OverviewEvent::Key(VK_ESCAPE) => self.close_overview(return_to, |_| {}),
            OverviewEvent::Key(VK_RETURN | VK_SPACE) => match self.overview.as_ref().and_then(|o| o.hover) {
                Some(i) => {
                    let w = self.overview.as_ref().expect("checked").cells[i].id;
                    self.close_overview(w, |l| l.frame(w));
                }
                None => self.close_overview(return_to, |l| l.zoom_all()),
            },
            OverviewEvent::Key(_) => {}
            // Gone to another window (Alt+Tab, the taskbar): step back in.
            OverviewEvent::Away => self.close_overview(0, |_| {}),
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

/// The windows on the overview's cards, if it's up.
fn ov_ids(ov: &Option<Overview>) -> impl Iterator<Item = WinId> + '_ {
    ov.iter().flat_map(|o| o.cells.iter().map(|c| c.id))
}

/// Where a window framed on its own sits on display `mon`: the work area,
/// with the same margin a docked window framed there gets.
fn framed_rect(mon: MonId) -> Rect {
    let a = work_area(mon);
    Rect::new(a.x + GAP / 2, a.y + GAP / 2, a.w - GAP, a.h - GAP)
}
