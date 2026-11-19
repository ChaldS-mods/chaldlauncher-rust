pub mod cli;
pub mod commands;
pub mod env;
pub mod payload;
pub mod setup;
pub mod silent;
pub mod uninstall;

use std::sync::atomic::Ordering;

use tauri::Manager;

/// The custom installer window: the same HTML/CSS/JS as before, now served by Tauri.
/// `payload` is the embedded `.chpk` built by `chald-pack`.
pub fn run_gui(payload: Vec<u8>, cli: cli::Cli) {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::SetupState::new(payload, cli))
        .invoke_handler(tauri::generate_handler![
            commands::setup_info,
            commands::setup_choose_dir,
            commands::setup_check_dir,
            commands::setup_install,
            commands::setup_cancel,
            commands::setup_launch,
            commands::setup_minimize,
            commands::setup_close,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let state = window.app_handle().state::<commands::SetupState>();
                if state.installing.load(Ordering::SeqCst) {
                    api.prevent_close();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running ChaldLauncher installer");
}
