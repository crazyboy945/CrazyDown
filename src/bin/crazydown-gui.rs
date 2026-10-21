#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let sources: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = crazydown::gui::run(sources) {
        eprintln!("无法启动 CrazyDown GUI: {error}");
    }
}
