#![allow(dead_code)]
#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
mod h264;
mod mixer;
mod mp4;
mod ring;

#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    win::app::run();
    #[cfg(not(windows))]
    println!("clip-a-pc only runs on Windows");
}
