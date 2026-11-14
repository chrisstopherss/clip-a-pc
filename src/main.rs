#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    clip_a_pc::win::app::run();
    #[cfg(target_os = "macos")]
    clip_a_pc::mac::app::run();
    #[cfg(not(any(windows, target_os = "macos")))]
    println!("clip-a-pc runs on Windows and macOS");
}
