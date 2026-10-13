//! Windows integration: watches windows, docks them into the layout, and
//! moves them to where the layout says they belong.

mod sys;

use crate::frost;
use crate::layout::{ease, Edge, Layout, Placement, Rect, WinId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use sys::*;

/// Every layout motion: split, dock, zoom, close.
const MOTION: Duration = Duration::from_millis(240);
const GAP: i32 = 8;
const ANIM_TIMER: usize = 1;
/// Updates the drop zone while a window is being dragged.
const DRAG_TIMER: usize = 2;

/// How the drop zone looks: frosted glass, a touch darker than what's under it.
const ZONE_STYLE: frost::Style = frost::Style { blur: 2, darken: 40, corner: 10.0, border: 60 };
/// The drop zone's frost is captured at 1/ZONE_SCALE size, then blurred.
const ZONE_SCALE: i32 = 8;
/// How often the frost is redrawn while the zone stays put.
const ZONE_REFRESH: Duration = Duration::from_millis(100);

/// The screen under `r`, frosted.
fn frosted(r: Rect) -> Vec<u32> {
    let (sw, sh) = ((r.w / ZONE_SCALE).max(1) as usize, (r.h / ZONE_SCALE).max(1) as usize);
    let mut small = capture_scaled(r, sw, sh);
    frost::blur(&mut small, sw, sh, ZONE_STYLE.blur);
    frost::compose(&small, sw, sh, r.w.max(1) as usize, r.h.max(1) as usize, &ZONE_STYLE)
}

type MonId = isize;

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
    /// The drop-zone panel, where it's showing (None = hidden), when it was
    /// last drawn, and the window being dragged, which it sits just under.
    overlay: Hwnd,
    drop_zone: Option<Rect>,
    zone_drawn: Option<Instant>,
    dragging: Hwnd,
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

#[derive(Clone, Copy)]
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
    Float,
    FloatAll,
    Quit,
}

const HOTKEYS: [(Cmd, Mods, u32, &str); 14] = [
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
        match cmd {
            Cmd::ShowAll => app.camera(mon, |l| l.zoom_all()),
            Cmd::ZoomOut => app.camera(mon, |l| l.zoom_out()),
            Cmd::ZoomIn => {
                let under = root_window_at(cursor_pos());
                app.camera(mon, |l| l.zoom_in_toward(under));
            }
            Cmd::Frame => {
                let m = app.monitor_of(fg).unwrap_or(mon);
                app.camera(m, |l| l.frame(fg));
            }
            Cmd::Prev => app.camera(mon, |l| l.neighbour(-1)),
            Cmd::Next => app.camera(mon, |l| l.neighbour(1)),
            Cmd::Back => app.camera(mon, |l| l.back()),
            Cmd::Forward => app.camera(mon, |l| l.forward()),
            Cmd::Dock(edge) => {
                if is_manageable(fg) {
                    let m = monitor_of_window(fg);
                    app.undock(fg);
                    app.dock(fg, m, edge, None);
                }
            }
            Cmd::Float => {
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
    if !(EVENT_OBJECT_DESTROY..=EVENT_OBJECT_HIDE).contains(&event) {
        trace!("event {event:#x} hwnd {hwnd:#x}");
    }
    with_app(|app| match event {
        EVENT_SYSTEM_MOVESIZESTART => {
            if let Some(r) = visual_rect(hwnd) {
                app.drag_start.insert(hwnd, r);
                set_timer(app.hwnd, DRAG_TIMER, 16);
            }
            // A drag interrupts any motion in flight.
            app.finish_anim();
        }
        EVENT_SYSTEM_MOVESIZEEND => {
            kill_timer(app.hwnd, DRAG_TIMER);
            app.show_drop_zone(None);
            app.drag_ended(hwnd);
        }
        EVENT_SYSTEM_MINIMIZESTART => {
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
                app.layouts.entry(m).or_insert_with(|| Layout::new(GAP)).restore(hwnd, p);
                app.apply(m);
            }
        }
        EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE => {
            app.minimized.remove(&hwnd);
            if let Some(m) = app.monitor_of(hwnd) {
                app.undock(hwnd);
                app.apply(m);
            }
        }
        _ => {}
    });
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
        self.layouts.entry(mon).or_insert_with(|| Layout::new(GAP)).dock(id, edge, target);
        rescue_list_add(id);
        self.apply(mon);
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
            None => Layout::new(GAP).dock_target(area, pt.0, pt.1, hwnd),
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
        let zone = if resizing {
            None
        } else {
            self.drop_target(hwnd, cursor_pos()).and_then(|(mon, target, edge)| {
                let area = work_area(mon);
                let l = self.layouts.get(&mon).cloned().unwrap_or_else(|| Layout::new(GAP));
                l.preview(hwnd, edge, target, area)
            })
        };
        self.show_drop_zone(zone);
    }

    fn show_drop_zone(&mut self, zone: Option<Rect>) {
        // Redraw when the zone moves, and every so often while it stays put,
        // so the frost keeps up with what's underneath.
        let stale = self.zone_drawn.is_none_or(|t| t.elapsed() >= ZONE_REFRESH);
        if zone == self.drop_zone && (zone.is_none() || !stale) {
            return;
        }
        self.drop_zone = zone;
        match zone {
            Some(r) => {
                self.zone_drawn = Some(Instant::now());
                show_overlay(self.overlay, r, &frosted(r), self.dragging);
            }
            None => hide_overlay(self.overlay),
        }
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
            place_window(*id, Rect::lerp(from, *to, k));
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
            }
        }
    }

    /// Release every window: back to a normal desktop, nothing lost.
    fn float_all(&mut self) {
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
    }
}
