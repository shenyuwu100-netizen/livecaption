#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if livecaption_lib::capture_worker::run_if_requested() {
        return;
    }
    livecaption_lib::run()
}
