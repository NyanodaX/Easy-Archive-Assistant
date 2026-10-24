#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod bridge;
use tauri::{Emitter, Manager};

fn main() {
    tauri::Builder::default()
        .manage(bridge::Session::default())
        .invoke_handler(tauri::generate_handler![
            bridge::choose_path,
            bridge::choose_paths,
            bridge::plan_operation,
            bridge::execute_plan,
            bridge::cancel_task
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let session = window.state::<bridge::Session>();
                if session.request_close() {
                    api.prevent_close();
                    let _ = window.emit("archive-closing", ());
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("desktop runtime failed");
}
