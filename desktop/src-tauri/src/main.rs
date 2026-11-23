// beleaf for Windows: the web app in its own window (Microsoft Edge WebView2 under the hood).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    beleaf_lib::run()
}
