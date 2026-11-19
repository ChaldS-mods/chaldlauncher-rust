use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::cli::Cli;
use crate::env::{self, SystemEnv};
use crate::payload::{read_header_bytes, PayloadHeader};
use crate::setup::{self, InstallOptions, InstallResult};

pub struct SetupState {
    pub payload: Vec<u8>,
    pub cli: Cli,
    pub cancel: Arc<AtomicBool>,
    pub installing: Arc<AtomicBool>,
    pub last: Mutex<Option<InstallResult>>,
}

impl SetupState {
    pub fn new(payload: Vec<u8>, cli: Cli) -> Self {
        Self {
            payload,
            cli,
            cancel: Arc::new(AtomicBool::new(false)),
            installing: Arc::new(AtomicBool::new(false)),
            last: Mutex::new(None),
        }
    }
}

fn header(state: &SetupState) -> Result<PayloadHeader, String> {
    read_header_bytes(&state.payload)
}

fn parse_options(value: &Value) -> Result<InstallOptions, String> {
    let dir = value
        .get("dir")
        .and_then(Value::as_str)
        .ok_or_else(|| "Некорректная папка установки".to_string())?;
    let path = PathBuf::from(dir);
    if !path.is_absolute() {
        return Err("Папка установки должна быть абсолютным путём".to_string());
    }
    Ok(InstallOptions {
        dir: path,
        desktop_shortcut: value.get("desktopShortcut").and_then(Value::as_bool).unwrap_or(false),
        start_menu_shortcut: value.get("startMenuShortcut").and_then(Value::as_bool).unwrap_or(false),
    })
}

#[tauri::command]
pub fn setup_info(state: State<'_, SetupState>) -> Value {
    let header = match header(&state) {
        Ok(header) => header,
        Err(error) => return json!({ "error": error }),
    };
    let existing = env::find_existing(&header.meta);
    let default_dir = existing
        .as_ref()
        .map(|e| e.dir.to_string_lossy().to_string())
        .unwrap_or_else(|| env::default_install_dir(&header.meta).to_string_lossy().to_string());
    let existing_json = existing
        .as_ref()
        .map(|e| json!({ "dir": e.dir.to_string_lossy(), "version": e.version }));
    json!({
        "productName": header.meta.product_name,
        "version": header.meta.version,
        "publisher": header.meta.publisher,
        "totalSize": header.total_size,
        "defaultDir": default_dir,
        "existing": existing_json,
        "cli": { "desktop": state.cli.desktop, "startMenu": state.cli.start_menu },
    })
}

#[tauri::command]
pub fn setup_choose_dir(app: AppHandle, state: State<'_, SetupState>, current: Option<String>) -> Value {
    let header = match header(&state) {
        Ok(header) => header,
        Err(_) => return Value::Null,
    };
    let mut dialog = app.dialog().file();
    if let Some(current) = current.as_deref() {
        let path = PathBuf::from(current);
        if path.is_dir() {
            dialog = dialog.set_directory(path);
        }
    }
    let Some(picked) = dialog.blocking_pick_folder() else { return Value::Null };
    let Some(path) = picked.into_path().ok() else { return Value::Null };
    let product = &header.meta.product_name;
    let chosen = match path.file_name().and_then(|name| name.to_str()) {
        Some(name) if name.eq_ignore_ascii_case(product) => path,
        _ => path.join(product),
    };
    json!(chosen.to_string_lossy())
}

#[tauri::command]
pub fn setup_check_dir(state: State<'_, SetupState>, dir: String) -> Value {
    let path = PathBuf::from(dir.trim());
    if !path.is_absolute() {
        return json!({ "error": "Некорректная папка" });
    }
    let header = match header(&state) {
        Ok(header) => header,
        Err(error) => return json!({ "error": error }),
    };
    let final_dir = match setup::resolve_install_dir(&path, &header.meta.product_name) {
        Ok(dir) => dir,
        Err(error) => return json!({ "error": error }),
    };
    let need = (header.total_size as f64 * 1.05) as u64 + 50 * 1024 * 1024;
    let free = setup::free_space(&final_dir);
    json!({
        "dir": final_dir.to_string_lossy(),
        "need": need,
        "free": free,
        "enough": free.map(|free| free >= need).unwrap_or(true),
    })
}

#[tauri::command]
pub fn setup_install(app: AppHandle, state: State<'_, SetupState>, opts: Value) -> Value {
    if state.installing.swap(true, Ordering::SeqCst) {
        return json!({ "ok": false, "error": "Установка уже идёт" });
    }
    let result = (|| -> Result<InstallResult, String> {
        let options = parse_options(&opts)?;
        state.cancel.store(false, Ordering::SeqCst);
        let cancel = state.cancel.clone();
        let handle = app.clone();
        let mut last_emit = Instant::now();
        setup::install(&state.payload, &options, &SystemEnv, &cancel, |progress| {
            if last_emit.elapsed() >= Duration::from_millis(40) || progress.percent >= 100 {
                last_emit = Instant::now();
                let _ = handle.emit("setup:progress", &progress);
            }
        })
    })();
    state.installing.store(false, Ordering::SeqCst);
    match result {
        Ok(done) => {
            let dir = done.dir.clone();
            let warnings = done.warnings.clone();
            *state.last.lock().unwrap() = Some(done);
            json!({ "ok": true, "dir": dir, "warnings": warnings })
        }
        Err(error) => {
            if state.cancel.load(Ordering::SeqCst) {
                json!({ "ok": false, "cancelled": true, "error": "Установка отменена" })
            } else {
                json!({ "ok": false, "error": error })
            }
        }
    }
}

#[tauri::command]
pub fn setup_cancel(state: State<'_, SetupState>) -> Value {
    state.cancel.store(true, Ordering::SeqCst);
    Value::Null
}

#[tauri::command]
pub fn setup_launch(app: AppHandle, state: State<'_, SetupState>) -> Value {
    let last = state.last.lock().unwrap().clone();
    if let Some(done) = last {
        let _ = std::process::Command::new(&done.exe_path)
            .current_dir(&done.dir)
            .spawn();
    }
    app.exit(0);
    Value::Null
}

#[tauri::command]
pub fn setup_minimize(app: AppHandle) -> Value {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.minimize();
    }
    Value::Null
}

#[tauri::command]
pub fn setup_close(app: AppHandle, state: State<'_, SetupState>) -> Value {
    if state.installing.load(Ordering::SeqCst) {
        return Value::Null;
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.close();
    }
    Value::Null
}
