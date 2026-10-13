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

- Drag a window by its title bar to a screen edge to dock it beside everything in view. Pushing the window partly off the left or right side works too.
- Drag it onto the edge of another docked window to split that window's space.
- Drop a docked window away from any edge to float it again.
- Drag the border between two docked windows to resize both at once.
- Minimize a docked window and its neighbours take the space. Restore it and it comes back to the same spot.

**Keyboard**

| Shortcut | Action |
| --- | --- |
| `Ctrl+Alt+Space` | Show everything |
| `Ctrl+Alt+Up` | Step out a level |
| `Ctrl+Alt+Down` | Step in toward the window under the pointer |
| `Ctrl+Alt+Enter` | Frame the active window on its own |
| `Ctrl+Alt+Left` / `Right` | Move to the neighbouring space |
| `Ctrl+Alt+[` / `]` | Back / forward |
| `Ctrl+Alt+Shift+Arrow` | Dock the active window at that edge |
| `Ctrl+Alt+F` | Float or dock the active window |
| `Ctrl+Alt+Shift+Q` | Quit |

Each display keeps its own arrangement and camera.

## Tips

- **Turn off Windows Snap** (Settings → System → Multitasking → Snap windows). Snap and Lattice both act on edge drags. Lattice wins, but you'll see Snap's preview flash first.
- Some laptops bind `Ctrl+Alt+Arrow` to screen rotation (Intel graphics). If Lattice can't claim a shortcut, it lists the ones that were taken when it starts.
- **Quitting is safe.** Quitting, or choosing *Float all windows*, puts every window back on screen. If Lattice ever crashes, a crash handler moves any off-screen window back into view.

## How it works

- `src/layout.rs` is the layout engine. It's pure Rust with no Windows code, and unit-tested. The desktop is a tree: leaves are windows, splits divide space by weight. The camera is a path into the tree, and zooming maps that node's rect onto the screen. Windows outside it land off-screen and are parked.
- `src/win/` is the Windows layer. It hooks window events (`SetWinEventHook`) for drags, minimizes and closes, registers global hotkeys, and moves windows with async `SetWindowPos`, so a hung app can never freeze Lattice. Placement accounts for the invisible resize borders on Windows 10/11, so gaps are exact.

```powershell
cargo test   # layout engine tests, runs on any OS
```

## Roadmap

- Zoomed-out view with live DWM thumbnails, plus app colour and icon for windows too small to read
- Pinch-to-zoom on precision touchpads
- Tab groups (stack windows under one strip)
- Saved views (`Ctrl+Alt+Shift+1–9` to save, `Ctrl+Alt+1–9` to recall)
- Persist arrangements across restarts
- Per-app rules ("don't arrange this app")

## License

MIT
