mod ai;
mod archive;
mod auth;
mod chaldpack;
mod config;
mod content;
mod curseforge;
mod dispatch;
mod forge;
mod friends;
mod game;
mod java;
mod discord;
mod dns;
mod download;
mod installer;
mod launch_args;
mod modrinth;
mod net;
mod version_json;
mod versions;
mod errors;
mod ftbquests;
mod instances;
mod mem;
mod mod_deps;
mod mojang;
mod offline;
mod paths;
mod presence;
mod settings;
mod state;
mod undo;
mod update;
mod launcher_import;
mod overlay;
mod versionchange;
mod wardrobe;

use state::AppState;

pub use errors::AppError;

/// Installs a version without the UI; used by the `install` helper binary to check the pipeline end-to-end.
pub async fn install_test(
    mc: &str,
    loader: &str,
    loader_version: Option<&str>,
    data: &std::path::Path,
    report: &(dyn Fn(&str, Option<f64>) + Sync),
) -> Result<String, AppError> {
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let target = installer::InstallTarget {
        mc_version: mc.to_string(),
        loader_type: loader.to_string(),
        loader_version: loader_version.map(str::to_string),
    };
    installer::install_version(&target, data, report, &cancel).await
}

/// End-to-end smoke test of the launch pipeline: build the command and start Java.
pub fn launch_test(data: &std::path::Path, mc_version: &str, version_id: &str) -> Result<std::process::Child, AppError> {
    let version = version_json::load_resolved_version(data, version_id)?;
    let jar = version_json::client_jar(data, mc_version);
    if !jar.is_file() {
        return Err(AppError::new("FILES_MISSING", format!("Нет клиента {}", jar.display())));
    }
    let required = version.java_version.as_ref().and_then(|java| java.major_version).unwrap_or(8);
    let java_path = java::ensure_java("", required, &paths::runtime_dir(data))?;
    let game_dir = data.join("instances").join("test").join(".minecraft");
    let natives_dir = data.join("instances").join("test").join(".natives");
    std::fs::create_dir_all(&game_dir)?;
    let libraries_dir = paths::libraries_dir(data);
    let ctx = version_json::RuleContext {
        platform: version_json::current_platform(),
        features: std::collections::HashMap::new(),
    };
    let resolved = version_json::resolve_libraries(&version, &ctx)?;
    version_json::extract_natives(&resolved, &libraries_dir, &natives_dir)?;
    let command = launch_args::build_launch_command(&launch_args::LaunchInput {
        version: &version,
        platform: version_json::current_platform(),
        java_path: java_path.clone(),
        game_dir: game_dir.to_string_lossy().to_string(),
        assets_dir: paths::assets_dir(data).to_string_lossy().to_string(),
        libraries_dir: libraries_dir.to_string_lossy().to_string(),
        natives_dir: natives_dir.to_string_lossy().to_string(),
        client_jar: jar.to_string_lossy().to_string(),
        auth: launch_args::LaunchAuth {
            name: "Steve".to_string(),
            uuid: offline::uuid("Steve"),
            access_token: "0".to_string(),
            user_type: "legacy".to_string(),
            offline_fallback: false,
        },
        memory_min_mb: 1024,
        memory_max_mb: 2048,
        extra_jvm_args: Vec::new(),
        resolution: Some((854, 480)),
        fullscreen: false,
        launcher_name: "ChaldLauncher".to_string(),
        launcher_version: env!("CARGO_PKG_VERSION").to_string(),
        version_name: if version_id.contains("-forge") || version_id.contains("-neoforge") {
            version.inherits_from.clone()
        } else {
            None
        },
    })?;
    println!("java: {}", command.command);
    println!("args: {}", command.args.join(" "));
    let mut process = std::process::Command::new(&command.command);
    process.args(&command.args).current_dir(&command.cwd);
    process
        .spawn()
        .map_err(|error| AppError::new("SPAWN_FAILED", format!("Не удалось запустить Java: {error}")))
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    use tauri::Manager;
                    if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        let state = app.state::<AppState>();
                        state.overlay.toggle(app);
                    }
                })
                .build(),
        )
        .manage(AppState::new())
        .setup(|app| {
            use tauri::Manager;
            use tauri_plugin_global_shortcut::GlobalShortcutExt;
            let state = app.state::<AppState>();
            state.friends.start_heartbeat();
            if let Ok(settings) = state.settings.get() {
                state.presence.configure(
                    settings.discord_rpc,
                    settings.discord_show_instance,
                    &settings.language,
                );
            }
            let shortcut = if cfg!(target_os = "macos") {
                "CommandOrControl+Shift+O"
            } else {
                "CommandOrControl+Shift+O"
            };
            let _ = app.global_shortcut().register(shortcut);
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                use tauri::Manager;
                if window.label() == "main" {
                    let state = window.app_handle().state::<AppState>();
                    if let Ok(settings) = state.settings.get() {
                        let auto = settings.auto_install_updates
                            && settings.close_behavior != "minimize-while-playing";
                        let _ = crate::update::install_on_quit(
                            &state.data_dir,
                            &state.updater,
                            auto,
                        );
                    }
                    state.presence.dispose();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![dispatch::chald_dispatch])
        .run(tauri::generate_context!())
        .expect("error while running ChaldLauncher");
}
