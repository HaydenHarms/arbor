#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg_attr(not(windows), allow(dead_code))]
mod layout;
#[cfg_attr(not(windows), allow(dead_code))]
mod frost;
#[cfg_attr(not(windows), allow(dead_code))]
mod template;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    win::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Lattice is a Windows window manager. On other platforms, run `cargo test` to exercise the layout engine.");
}
