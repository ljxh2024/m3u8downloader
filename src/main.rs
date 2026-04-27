#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(e) = m3u8downloader::run() {
        eprintln!("{e}");
    }
}