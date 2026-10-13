# Lattice

A nesting, zooming tiling window manager for Windows, written in Rust.

Drag a window to the edge of the screen and it docks; everything else makes room. Drop a window on the edge of another and they split that space. Splits nest inside splits as deep as you like. Then zoom: frame one window or one group full-screen, step back out, or show everything at once.

## Install

Grab `lattice.exe` from the latest [build](../../actions/workflows/build.yml) artifact, or build it yourself:

```powershell
cargo build --release
.\target\release\lattice.exe
```

Lattice lives in the system tray. Right-click the icon to show everything, float every window, or quit.

## Using it

**Docking with the mouse**

- While you drag, a frosted panel shows exactly where the window will land. No panel means it will float where you drop it.
- Drag a window by its title bar until the pointer meets a screen edge to dock it there. The first window takes that half of the screen; the next fills the other half.
- Near either end of an edge, it stacks instead: top or bottom of the right edge puts it above or below what's docked on the right, and the ends of the top or bottom edge put it beside what's docked there.
- Push a window against the middle of the top edge to fill the screen with it. A docked window is framed where it sits; a floating one comes back to where you picked it up when you step out (`Ctrl+Alt+Space`, `Ctrl+Alt+Up` or `Ctrl+Alt+[`).
- Drop it near the edge of a docked window to split that window's space.
- Drop it on a docked window's title bar to make them tabs. They share that spot under a strip of tabs, and only the picked one shows. Drop more windows on the strip to add tabs.
- Drop it in the middle of the screen or of any window and it floats. Drop a docked window there to float it again.
- Maximized windows drag too: grab the title bar and the window restores under the pointer, as it does with Windows Snap on.
- Drag the border between two docked windows to resize both at once.
- Minimize a docked window and its neighbours take the space. Restore it and it comes back to the same spot, tab groups included.

**Tabs**

- Click a tab to show that window. Middle-click a tab to close its window.
- Drag a tab off the strip to take it out of the group, then drop it anywhere as usual: an edge docks it, the middle floats it, another title bar makes it a tab there. Dragging the showing window by its own title bar does the same.
- Tabs work with any app, because Lattice only moves windows. It never puts one app's window inside another's.
- Switching to a hidden tab with Alt+Tab or the taskbar shows it. The same goes for any docked window zoomed out of view: Lattice steps out until it's in view.

**Keyboard**

| Shortcut | Action |
| --- | --- |
| `Ctrl+Alt+Space` | Show everything |
| `Ctrl+Alt+Up` | Step out a level |
| `Ctrl+Alt+Down` | Step in toward the window under the pointer |
| `Ctrl+Alt+Enter` | Frame the active window on its own, docked or floating (again to put a floating one back) |
| `Ctrl+Alt+Left` / `Right` | Move to the neighbouring space |
| `Ctrl+Alt+[` / `]` | Back / forward |
| `Ctrl+Alt+Page Up` / `Page Down` | Previous / next tab in the active window's group |
| `Ctrl+Alt+Shift+Arrow` | Dock the active window at that edge |
| `Ctrl+Alt+F` | Float or dock the active window |
| `Ctrl+Alt+Shift+Q` | Quit |

Each display keeps its own arrangement and camera.

## Tips

- **Turn off Windows Snap** (Settings → System → Multitasking → Snap windows). Snap and Lattice both act on edge drags. Lattice wins, but you'll see Snap's preview flash first.
- Some laptops bind `Ctrl+Alt+Arrow` to screen rotation (Intel graphics). If Lattice can't claim a shortcut, it lists the ones that were taken when it starts.
- **Quitting is safe.** Quitting, or choosing *Float all windows*, puts every window back on screen. If Lattice ever crashes, a crash handler moves any off-screen window back into view.

## How it works

- `src/layout.rs` is the layout engine. It's pure Rust with no Windows code, and unit-tested. The desktop is a tree: leaves are windows, splits divide space by weight. The camera is a path into the tree, and zooming maps that node's rect onto the screen. Windows outside it land off-screen and are parked. A leaf can hold a tab group: the tree holds the showing tab, and the rest are parked behind it.
- `src/win/` is the Windows layer. It hooks window events (`SetWinEventHook`) for drags, minimizes and closes, registers global hotkeys, and moves windows with async `SetWindowPos`, so a hung app can never freeze Lattice. Placement accounts for the invisible resize borders on Windows 10/11, so gaps are exact.

```powershell
cargo test   # layout engine tests, runs on any OS
```

## Roadmap

- Zoomed-out view with live DWM thumbnails, plus app colour and icon for windows too small to read
- Pinch-to-zoom on precision touchpads
- Saved views (`Ctrl+Alt+Shift+1–9` to save, `Ctrl+Alt+1–9` to recall)
- Persist arrangements across restarts
- Per-app rules ("don't arrange this app")

## License

MIT
