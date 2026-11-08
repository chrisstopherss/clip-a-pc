#[cfg(windows)]
mod mft;
#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    println!("spike_a only runs on Windows");
}
