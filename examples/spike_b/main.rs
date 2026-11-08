#[path = "../../src/mixer.rs"]
#[allow(dead_code)]
mod mixer;
#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    println!("spike_b only runs on Windows");
}
