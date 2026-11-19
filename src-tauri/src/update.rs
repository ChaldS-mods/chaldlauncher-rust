//! Проверка/скачивание/установка обновлений. Порт `src/main/update.ts` + `updater.ts`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{json, Value};
use tauri::AppHandle;

use crate::config::DEFAULT_UPDATE_REPO;
use crate::download::{download_file, is_file_valid, DownloadSpec};
use crate::errors::{AppError, AppResult};
use crate::paths;
use crate::{game, net, versions};

#[derive(Debug, Clone, Deserialize)]
struct GhAsset {
    name: String,
    size: u64,
    browser_download_url: String,
    digest: Option<String>,
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub url: String,
    pub sha256: Option<String>,
}

#[derive(Debug)]
struct Release {
    version: String,
    page_url: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdaterRuntime {
    pub latest_version: Option<String>,
    pub page_url: Option<String>,
    pub asset: Option<Asset>,
    pub downloading: bool,
}

fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None) => {
            !owner.is_empty()
                && !name.is_empty()
                && owner.len() <= 100
                && name.len() <= 100
                && owner
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        }
        _ => false,
    }
}

async fn fetch_latest_release(repo: &str) -> AppResult<Release> {
    if !valid_repo(repo) {
        return Err(AppError::new(
            "INVALID_ARGS",
            "Некорректный репозиторий обновлений",
        ));
    }
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let release: GhRelease = net::fetch_json(&url, "Проверка обновлений лаунчера").await?;
    Ok(Release {
        version: release.tag_name.trim_start_matches('v').to_string(),
        page_url: release.html_url,
        assets: release
            .assets
            .into_iter()
            .filter(|asset| asset.state.as_deref().is_none_or(|state| state == "uploaded"))
            .map(|asset| Asset {
                name: asset.name,
                size: asset.size,
                url: asset.browser_download_url,
                sha256: asset.digest.as_deref().and_then(|digest| {
                    digest
                        .strip_prefix("sha256:")
                        .filter(|hex| hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
                        .map(|hex| hex.to_ascii_lowercase())
                }),
            })
            .collect(),
    })
}

pub fn pick_installer<'a>(
    assets: &'a [Asset],
    platform: &str,
    app_image: bool,
    arch: &str,
) -> Option<&'a Asset> {
    if platform == "win32" {
        return assets
            .iter()
            .filter(|asset| {
                let name = asset.name.to_ascii_lowercase();
                name.ends_with(".exe")
                    && !name.contains("uninstall")
                    && !name.contains("portable")
                    && !name.contains("blockmap")
            })
            .find(|asset| {
                let name = asset.name.to_ascii_lowercase();
                name.contains("setup") || name.contains("install")
            })
            .or_else(|| {
                assets.iter().find(|asset| {
                    let name = asset.name.to_ascii_lowercase();
                    name.ends_with(".exe")
                        && !name.contains("uninstall")
                        && !name.contains("portable")
                        && !name.contains("blockmap")
                })
            });
    }
    if platform == "linux" && app_image {
        let want_arm = arch == "arm64" || arch == "aarch64";
        return assets.iter().find(|asset| {
            let name = asset.name.to_ascii_lowercase();
            if !name.ends_with(".appimage") {
                return false;
            }
            let is_arm = name.contains("arm64") || name.contains("aarch64");
            is_arm == want_arm
        });
    }
    None
}

fn no_source(current: &str) -> Value {
    json!({
        "configured": false,
        "current": current,
        "latest": Value::Null,
        "url": Value::Null,
        "hasUpdate": false,
        "assetName": Value::Null,
        "downloaded": false,
        "downloading": false,
    })
}

fn platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

#[allow(dead_code)]
pub fn file_for(updates_dir: &Path, asset: &Asset) -> PathBuf {
    let safe = paths::safe_file_name(&asset.name).unwrap_or_else(|_| "update.bin".to_string());
    updates_dir.join(&asset.name.replace('.', "_").replace('/', "_"))
        .join(safe)
}

fn file_for_version(updates_dir: &Path, version: &str, asset: &Asset) -> AppResult<PathBuf> {
    let dir_name = paths::safe_file_name(&format!("v{version}"))?;
    let file_name = paths::safe_file_name(&asset.name)?;
    Ok(updates_dir.join(dir_name).join(file_name))
}

fn is_downloaded(updates_dir: &Path, version: &str, asset: &Asset) -> bool {
    let Ok(path) = file_for_version(updates_dir, version, asset) else {
        return false;
    };
    is_file_valid(
        &path,
        Some(asset.size),
        None,
        asset.sha256.as_deref(),
    )
}

/// `update:check`.
pub async fn check(
    data_dir: &Path,
    runtime: &Arc<Mutex<UpdaterRuntime>>,
    repo: &str,
    enabled: bool,
) -> AppResult<Value> {
    let current = env!("CARGO_PKG_VERSION");
    let effective_repo = if repo.is_empty() { DEFAULT_UPDATE_REPO } else { repo };
    if !enabled || effective_repo.is_empty() {
        let mut st = runtime.lock().unwrap();
        st.latest_version = None;
        st.page_url = None;
        st.asset = None;
        return Ok(no_source(current));
    }
    let release = match fetch_latest_release(effective_repo).await {
        Ok(release) => release,
        Err(error) if error.message.contains("HTTP 404") => {
            let mut info = no_source(current);
            info["configured"] = json!(true);
            let mut st = runtime.lock().unwrap();
            st.latest_version = None;
            st.page_url = None;
            st.asset = None;
            return Ok(info);
        }
        Err(error) => return Err(error),
    };

    let has_update = versions::compare_versions(&release.version, current) > 0;
    let app_image = std::env::var_os("APPIMAGE").is_some();
    let asset = if has_update {
        pick_installer(
            &release.assets,
            platform(),
            app_image,
            std::env::consts::ARCH,
        )
        .cloned()
    } else {
        None
    };

    let updates_dir = paths::updates_dir(data_dir);
    let downloaded = asset
        .as_ref()
        .map(|a| is_downloaded(&updates_dir, &release.version, a))
        .unwrap_or(false);
    let downloading = runtime.lock().unwrap().downloading;

    {
        let mut st = runtime.lock().unwrap();
        st.latest_version = Some(release.version.clone());
        st.page_url = Some(release.page_url.clone());
        st.asset = asset.clone();
    }

    Ok(json!({
        "configured": true,
        "current": current,
        "latest": release.version,
        "url": release.page_url,
        "hasUpdate": has_update,
        "assetName": asset.map(|item| json!(item.name)).unwrap_or(Value::Null),
        "downloaded": downloaded,
        "downloading": downloading,
    }))
}

fn emit_update_progress(
    app: &AppHandle,
    op_id: &str,
    label: &str,
    fraction: Option<f64>,
    status: &str,
    message: Option<String>,
) {
    game::emit(
        app,
        "op:progress",
        json!({
            "opId": op_id,
            "kind": "update",
            "instanceId": Value::Null,
            "label": label,
            "fraction": fraction,
            "status": status,
            "message": message,
        }),
    );
}

/// `update:download`: starts background download.
pub fn start_download(
    app: AppHandle,
    data_dir: PathBuf,
    runtime: Arc<Mutex<UpdaterRuntime>>,
    auto_install_on_close: bool,
) -> AppResult<()> {
    let (version, asset) = {
        let st = runtime.lock().unwrap();
        if st.downloading {
            return Ok(());
        }
        match (st.latest_version.clone(), st.asset.clone()) {
            (Some(v), Some(a)) => (v, a),
            _ => return Ok(()),
        }
    };
    let updates_dir = paths::updates_dir(&data_dir);
    let path = file_for_version(&updates_dir, &version, &asset)?;
    if is_file_valid(&path, Some(asset.size), None, asset.sha256.as_deref()) {
        let op_id = format!("update:{version}");
        emit_update_progress(
            &app,
            &op_id,
            &format!("Обновление лаунчера {version}"),
            Some(1.0),
            "done",
            Some("Обновление скачано — его можно установить".to_string()),
        );
        return Ok(());
    }

    {
        let mut st = runtime.lock().unwrap();
        st.downloading = true;
    }
    let op_id = format!("update:{version}");
    let label = format!("Обновление лаунчера {version}");
    emit_update_progress(&app, &op_id, &label, Some(0.0), "running", Some("Скачивание установщика…".to_string()));

    tauri::async_runtime::spawn(async move {
        let mut received: u64 = 0;
        let total = asset.size.max(1);
        let spec = DownloadSpec {
            url: asset.url.clone(),
            dest: path.clone(),
            sha1: None,
            sha256: asset.sha256.clone(),
            size: Some(asset.size),
            purpose: format!("Скачивание обновления лаунчера {version}"),
        };
        let app_for_progress = app.clone();
        let op_for_progress = op_id.clone();
        let label_for_progress = label.clone();
        let result = download_file(&spec, |delta| {
            received = received.saturating_add(delta);
            let fraction = (received as f64 / total as f64).clamp(0.0, 1.0);
            emit_update_progress(
                &app_for_progress,
                &op_for_progress,
                &label_for_progress,
                Some(fraction),
                "running",
                Some("Скачивание установщика…".to_string()),
            );
        })
        .await;

        {
            let mut st = runtime.lock().unwrap();
            st.downloading = false;
        }

        match result {
            Ok(()) => {
                let msg = if auto_install_on_close {
                    "Обновление установится при закрытии лаунчера"
                } else {
                    "Обновление скачано — его можно установить"
                };
                emit_update_progress(&app, &op_id, &label, Some(1.0), "done", Some(msg.to_string()));
            }
            Err(error) => {
                emit_update_progress(
                    &app,
                    &op_id,
                    "Не удалось скачать обновление",
                    None,
                    "error",
                    Some(error.message),
                );
            }
        }
    });
    Ok(())
}

fn replace_appimage(source: &Path, target: &Path) -> AppResult<()> {
    let target_str = target.to_string_lossy().to_string();
    if !target.is_absolute() || !target_str.to_ascii_lowercase().ends_with(".appimage") {
        return Err(AppError::new(
            "UPDATE_BAD_TARGET",
            "Не удалось определить файл AppImage, который нужно заменить.",
        ));
    }
    let tmp = target.with_extension(format!("new-{}", std::process::id()));
    if let Err(e) = std::fs::copy(source, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(map_write_err(e, target));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(map_write_err(e, target));
        }
    }
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(map_write_err(e, target));
    }
    Ok(())
}

fn map_write_err(e: std::io::Error, target: &Path) -> AppError {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem => {
            AppError::new(
                "UPDATE_NO_WRITE",
                format!(
                    "Нет прав на запись в {}: обнови лаунчер вручную.",
                    target
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| target.display().to_string())
                ),
            )
        }
        _ => AppError::new("FS", format!("Не удалось заменить AppImage: {e}")),
    }
}

fn relaunch_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(k, _)| !matches!(k.as_str(), "APPIMAGE" | "APPDIR" | "ARGV0" | "OWD" | "CHROME_DESKTOP"))
        .collect()
}

fn start_detached_appimage(target: &Path, launch: bool) -> AppResult<()> {
    if launch {
        // old process may still hold locks briefly; delay start like TS does
        let target_str = target.to_string_lossy().to_string();
        Command::new("/bin/sh")
            .args(["-c", "sleep 2; exec \"$0\"", &target_str])
            .env_clear()
            .envs(relaunch_env())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| AppError::new("UPDATE_RUN_FAILED", format!("Не удалось запустить обновлённый лаунчер: {e}")))?;
    }
    Ok(())
}

fn run_installer(file: &Path, launch: bool) -> AppResult<()> {
    if cfg!(target_os = "windows") {
        let mut cmd = Command::new(file);
        if launch {
            cmd.args(["--silent", "--launch"]);
        } else {
            cmd.arg("--silent");
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| AppError::new("UPDATE_RUN_FAILED", format!("Не удалось запустить установщик: {e}")))?;
        return Ok(());
    }
    if cfg!(target_os = "linux") {
        if let Some(appimage) = std::env::var_os("APPIMAGE") {
            let target = PathBuf::from(appimage);
            replace_appimage(file, &target)?;
            start_detached_appimage(&target, launch)?;
            return Ok(());
        }
    }
    Err(AppError::new(
        "UPDATE_NO_FILE",
        "Автообновление на этой системе недоступно.",
    ))
}

/// `update:install`.
pub fn install(
    app: &AppHandle,
    data_dir: &Path,
    runtime: &Arc<Mutex<UpdaterRuntime>>,
    games_running: usize,
) -> AppResult<()> {
    let (version, asset) = {
        let st = runtime.lock().unwrap();
        match (st.latest_version.clone(), st.asset.clone()) {
            (Some(v), Some(a)) => (v, a),
            _ => {
                return Err(AppError::new(
                    "UPDATE_NO_FILE",
                    "Обновление ещё не скачано.",
                ))
            }
        }
    };
    let updates_dir = paths::updates_dir(data_dir);
    let path = file_for_version(&updates_dir, &version, &asset)?;
    if !is_file_valid(&path, Some(asset.size), None, asset.sha256.as_deref()) {
        return Err(AppError::new(
            "UPDATE_NO_FILE",
            "Обновление ещё не скачано.",
        ));
    }
    if games_running > 0 {
        return Err(AppError::new(
            "UPDATE_GAME_RUNNING",
            "Закрой запущенную игру, чтобы установить обновление.",
        ));
    }
    run_installer(&path, true)?;
    app.exit(0);
    Ok(())
}

/// Best-effort install on quit when auto-install is enabled. Returns true if install was started.
pub fn install_on_quit(
    data_dir: &Path,
    runtime: &Arc<Mutex<UpdaterRuntime>>,
    auto_install: bool,
) -> bool {
    if !auto_install {
        return false;
    }
    let (version, asset) = {
        let st = runtime.lock().unwrap();
        match (st.latest_version.clone(), st.asset.clone()) {
            (Some(v), Some(a)) => (v, a),
            _ => return false,
        }
    };
    let updates_dir = paths::updates_dir(data_dir);
    let Ok(path) = file_for_version(&updates_dir, &version, &asset) else {
        return false;
    };
    if !is_file_valid(&path, Some(asset.size), None, asset.sha256.as_deref()) {
        return false;
    }
    run_installer(&path, false).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_repo() {
        assert!(valid_repo("ChaldS-mods/ChaldLauncher"));
        assert!(!valid_repo(""));
        assert!(!valid_repo("only-owner"));
        assert!(!valid_repo("../etc/passwd"));
        assert!(!valid_repo("a/b/c"));
    }

    #[test]
    fn picks_windows_installer_and_skips_portable() {
        let assets = vec![
            Asset {
                name: "ChaldLauncher-portable.exe".into(),
                size: 1,
                url: "u".into(),
                sha256: None,
            },
            Asset {
                name: "ChaldLauncher-Setup.exe".into(),
                size: 1,
                url: "u".into(),
                sha256: None,
            },
            Asset {
                name: "blockmap.exe".into(),
                size: 1,
                url: "u".into(),
                sha256: None,
            },
        ];
        let picked = pick_installer(&assets, "win32", false, "x86_64").unwrap();
        assert_eq!(picked.name, "ChaldLauncher-Setup.exe");
        assert!(pick_installer(&assets, "linux", true, "x86_64").is_none());
    }

    #[test]
    fn picks_linux_appimage_by_arch() {
        let assets = vec![
            Asset {
                name: "ChaldLauncher-x86_64.AppImage".into(),
                size: 1,
                url: "u".into(),
                sha256: None,
            },
            Asset {
                name: "ChaldLauncher-aarch64.AppImage".into(),
                size: 1,
                url: "u".into(),
                sha256: None,
            },
        ];
        assert_eq!(
            pick_installer(&assets, "linux", true, "aarch64")
                .unwrap()
                .name,
            "ChaldLauncher-aarch64.AppImage"
        );
        assert_eq!(
            pick_installer(&assets, "linux", true, "x86_64")
                .unwrap()
                .name,
            "ChaldLauncher-x86_64.AppImage"
        );
    }
}
