// beleaf as a program: the web app in its own window (WebView2 on Windows, the system WebView on Android).
// It keeps itself up to date: each start it fetches the newest beleaf from the website in the background and keeps it on
// this device (see live.rs), so changes to the website reach the program without reinstalling it.
mod live;
use std::path::PathBuf;

/// A .beleaf or .psd file beleaf was opened with (double-clicked in Explorer, or dragged onto the icon).
fn startup_path() -> Option<PathBuf> {
    std::env::args_os().skip(1).map(PathBuf::from).find(|p| {
        p.is_file()
            && p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("beleaf") || e.eq_ignore_ascii_case("psd"))
                .unwrap_or(false)
    })
}

#[tauri::command]
fn startup_name() -> Option<String> {
    startup_path().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
}

#[tauri::command]
fn startup_file() -> tauri::ipc::Response {
    let bytes = startup_path().and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
    tauri::ipc::Response::new(bytes)
}

/// "checking", "same", "updated" (a newer beleaf was fetched after this window had opened) or "offline".
#[tauri::command]
fn live_status() -> String {
    live::status()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut ctx = tauri::generate_context!();
    let built_in = ctx.set_assets(Box::new(live::Empty));
    ctx.set_assets(Box::new(live::LiveAssets { inner: built_in }));
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(tauri::generate_handler![startup_name, startup_file, live_status])
        .run(ctx)
        .expect("beleaf couldn't start");
}
