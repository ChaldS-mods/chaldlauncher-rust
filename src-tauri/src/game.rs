use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::errors::{AppError, AppResult};
use crate::instances;
use crate::java;
use crate::launch_args::{build_launch_command, redact, split_args, LaunchAuth, LaunchInput};
use crate::paths;
use crate::settings::Settings;
use crate::version_json;

type ChildHandle = Arc<Mutex<Child>>;
type Running = Arc<Mutex<HashMap<String, ChildHandle>>>;
type Logs = Arc<Mutex<HashMap<String, Vec<String>>>>;

/// Starts the game, keeps track of running processes and streams their output to the renderer.
#[derive(Default)]
pub struct GameManager {
    running: Running,
    logs: Logs,
}

impl GameManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn running_ids(&self) -> Vec<String> {
        self.running.lock().unwrap().keys().cloned().collect()
    }

    pub fn running_count(&self) -> usize {
        self.running.lock().unwrap().len()
    }

    pub fn logs(&self, id: &str) -> Vec<String> {
        self.logs.lock().unwrap().get(id).cloned().unwrap_or_default()
    }

    pub fn stop(&self, id: &str) {
        if let Some(handle) = self.running.lock().unwrap().get(id).cloned() {
            if let Ok(mut child) = handle.lock() {
                let _ = child.kill();
            }
        }
    }

    pub fn launch(&self, app: &AppHandle, data: &Path, settings: &Settings, id: &str, auth: LaunchAuth) -> AppResult<()> {
        if self.running.lock().unwrap().contains_key(id) {
            return Err(AppError::new("ALREADY_RUNNING", "Этот инстанс уже запускается или запущен"));
        }
        let meta = instances::read_meta_at(data, settings, id)?;
        if !meta.installed {
            return Err(AppError::new("NOT_INSTALLED", "Инстанс не установлен. Запусти установку."));
        }
        let Some(version_id) = meta.version_id.clone() else {
            return Err(AppError::new("NOT_INSTALLED", "Инстанс не установлен. Запусти установку."));
        };

        let version = version_json::load_resolved_version(data, &version_id)?;
        let jar = version_json::client_jar(data, &meta.mc_version);
        if !jar.is_file() {
            return Err(AppError::new(
                "FILES_MISSING",
                "Не найден клиент Minecraft. Запусти восстановление инстанса.",
            ));
        }
        let required = version
            .java_version
            .as_ref()
            .and_then(|java| java.major_version)
            .unwrap_or(8);
        let configured = if meta.settings.java_path.trim().is_empty() {
            settings.java_path.clone()
        } else {
            meta.settings.java_path.clone()
        };
        let java_path = java::ensure_java(&configured, required, &paths::runtime_dir(data))?;

        let instance_dir = instances::instance_dir(data, settings, id)?;
        let game_dir = instance_dir.join(".minecraft");
        let natives_dir = instance_dir.join(".natives");
        let libraries_dir = paths::libraries_dir(data);
        let ctx = version_json::RuleContext {
            platform: version_json::current_platform(),
            features: HashMap::new(),
        };
        let resolved = version_json::resolve_libraries(&version, &ctx)?;
        version_json::extract_natives(&resolved, &libraries_dir, &natives_dir)?;

        let max_mb = meta.settings.memory_max_mb.unwrap_or(settings.memory_max_mb).max(256);
        let mut min_mb = meta.settings.memory_min_mb.unwrap_or(settings.memory_min_mb);
        if min_mb > max_mb {
            min_mb = max_mb;
        }
        let width = meta.settings.width.unwrap_or(settings.width);
        let height = meta.settings.height.unwrap_or(settings.height);
        let fullscreen = meta.settings.fullscreen.unwrap_or(settings.fullscreen);
        let mut extra_jvm_args = split_args(&settings.jvm_args)?;
        extra_jvm_args.extend(split_args(&meta.settings.jvm_args)?);
        let version_name = if meta.loader.r#type == "forge" || meta.loader.r#type == "neoforge" {
            version.inherits_from.clone()
        } else {
            None
        };

        let command = build_launch_command(&LaunchInput {
            version: &version,
            platform: version_json::current_platform(),
            java_path: java_path.clone(),
            game_dir: game_dir.to_string_lossy().to_string(),
            assets_dir: paths::assets_dir(data).to_string_lossy().to_string(),
            libraries_dir: libraries_dir.to_string_lossy().to_string(),
            natives_dir: natives_dir.to_string_lossy().to_string(),
            client_jar: jar.to_string_lossy().to_string(),
            auth,
            memory_min_mb: min_mb,
            memory_max_mb: max_mb,
            extra_jvm_args,
            resolution: Some((width, height)),
            fullscreen,
            launcher_name: "ChaldLauncher".to_string(),
            launcher_version: env!("CARGO_PKG_VERSION").to_string(),
            version_name,
        })?;

        let mut process = Command::new(&command.command);
        process
            .args(&command.args)
            .current_dir(&command.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            process.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = process
            .spawn()
            .map_err(|e| AppError::new("SPAWN_FAILED", format!("Не удалось запустить Java: {e}")))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        self.logs.lock().unwrap().insert(id.to_string(), Vec::new());
        let handle: ChildHandle = Arc::new(Mutex::new(child));
        self.running.lock().unwrap().insert(id.to_string(), handle.clone());

        let command_line = format!("{} {}", command.command, command.args.join(" "));
        push_log(&self.logs, app, id, format!("[launcher] {}", redact(&command_line, &command.secrets)));
        push_log(
            &self.logs,
            app,
            id,
            format!("[launcher] память: {min_mb}–{max_mb} МБ, Java: {java_path}"),
        );
        if let Some(out) = stdout {
            pipe(app.clone(), self.logs.clone(), id.to_string(), out, command.secrets.clone());
        }
        if let Some(err) = stderr {
            pipe(app.clone(), self.logs.clone(), id.to_string(), err, command.secrets.clone());
        }

        progress(app, id, "Игра запущена", Some(1.0), "done", None);
        emit(app, "game:status", json!({ "instanceId": id, "status": "running", "exitCode": null }));

        let running = self.running.clone();
        let logs = self.logs.clone();
        let app = app.clone();
        let data = data.to_path_buf();
        let settings = settings.clone();
        let id = id.to_string();
        thread::spawn(move || {
            let started = Instant::now();
            let status = loop {
                let result = {
                    let mut child = handle.lock().unwrap();
                    child.try_wait()
                };
                match result {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => thread::sleep(Duration::from_millis(150)),
                    Err(_) => break None,
                }
            };
            running.lock().unwrap().remove(&id);
            let code = status.and_then(|status| status.code());
            let crashed = code.map(|code| code != 0).unwrap_or(true);
            push_log(
                &logs,
                &app,
                &id,
                format!("[launcher] процесс завершён, код: {}", code.map(|c| c.to_string()).unwrap_or_else(|| "?".to_string())),
            );
            emit(
                &app,
                "game:status",
                json!({
                    "instanceId": id,
                    "status": if crashed { "crashed" } else { "exited" },
                    "exitCode": code,
                }),
            );
            let seconds = started.elapsed().as_secs();
            if seconds > 0 {
                let _ = instances::add_play_time(&data, &settings, &id, seconds);
            }
        });

        Ok(())
    }
}

pub fn emit(app: &AppHandle, channel: &str, payload: Value) {
    if channel == "game:status" {
        track_presence(app, &payload);
    }
    let _ = app.emit("chald:event", json!({ "channel": channel, "payload": payload }));
}

fn progress(app: &AppHandle, id: &str, label: &str, fraction: Option<f64>, status: &str, message: Option<String>) {
    emit(
        app,
        "op:progress",
        json!({
            "opId": format!("launch:{id}"),
            "kind": "launch",
            "instanceId": id,
            "label": label,
            "fraction": fraction,
            "status": status,
            "message": message,
        }),
    );
}

fn push_log(logs: &Logs, app: &AppHandle, id: &str, line: String) {
    {
        let mut map = logs.lock().unwrap();
        let buffer = map.entry(id.to_string()).or_default();
        buffer.push(line.clone());
        if buffer.len() > 5000 {
            let excess = buffer.len() - 5000;
            buffer.drain(0..excess);
        }
    }
    emit(app, "game:log", json!({ "instanceId": id, "line": line }));
}

fn pipe<R: Read + Send + 'static>(app: AppHandle, logs: Logs, id: String, stream: R, secrets: Vec<String>) {
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            push_log(&logs, &app, &id, redact(&line, &secrets));
        }
    });
}

/// Игра запущена/остановлена: отражаем это в статусе Discord и у друзей.
fn track_presence(app: &AppHandle, payload: &Value) {
    use tauri::Manager;
    let Some(id) = payload.get("instanceId").and_then(Value::as_str) else { return };
    let state = app.state::<crate::state::AppState>();
    if payload.get("status").and_then(Value::as_str) != Some("running") {
        state.presence.game_stopped(id);
        state.friends.set_presence("launcher", "");
        return;
    }
    let Ok(settings) = state.settings.get() else { return };
    if let Ok(meta) = crate::instances::read_meta_at(&state.data_dir, &settings, id) {
        state.presence.game_started(crate::presence::Playing {
            id: id.to_string(),
            name: meta.name.clone(),
            mc_version: meta.mc_version.clone(),
            loader: meta.loader.r#type.clone(),
            since_ms: chrono::Utc::now().timestamp_millis(),
        });
        state.friends.set_presence("game", &meta.name);
    }
}
