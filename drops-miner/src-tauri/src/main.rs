// Bonkr Twitch Miner. On Windows this hides the black console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    drops_miner_lib::run()
}
