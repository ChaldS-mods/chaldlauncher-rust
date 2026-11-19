use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::env::{self, InstallEnv};
use crate::payload::{extract_bytes, read_header_bytes, PayloadMeta};
use crate::uninstall::{build_uninstall_script, UninstallParams};

pub const MARKER: &str = ".chald-install.json";
pub const UNINSTALL_SCRIPT: &str = "uninstall.ps1";

#[derive(Debug, Clone, Serialize)]
pub struct InstallProgress {
    pub phase: String,
    pub percent: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub dir: PathBuf,
    pub desktop_shortcut: bool,
    pub start_menu_shortcut: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallResult {
    pub dir: String,
    pub exe_path: String,
    pub version: String,
    pub warnings: Vec<String>,
}

fn progress(phase: &str, percent: u32, file: Option<&str>) -> InstallProgress {
    InstallProgress {
        phase: phase.to_string(),
        percent,
        file: file.filter(|f| !f.is_empty()).map(str::to_string),
    }
}

fn exists(path: &Path) -> bool {
    fs::metadata(path).is_ok()
}

fn io_message(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(28) | Some(112) => "Недостаточно места на диске.".to_string(),
        Some(13) | Some(5) => "Нет прав на запись в выбранную папку. Выберите другую папку или закройте программы, которые используют файлы лаунчера.".to_string(),
        Some(32) | Some(33) => "Файл занят другой программой. Закройте ChaldLauncher и повторите.".to_string(),
        _ => error.to_string(),
    }
}

/// Version of the installed copy, if this folder was created by our installer.
pub fn read_marker(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join(MARKER)).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.get("version").and_then(serde_json::Value::as_str).map(str::to_string)
}

/// The folder the installation really goes to. A foreign non-empty folder and a drive root are not touched:
/// in those cases the product name is appended, like the old installer did.
pub fn resolve_install_dir(chosen: &Path, product: &str) -> Result<PathBuf, String> {
    if !chosen.is_absolute() {
        return Err("Папка установки должна быть абсолютным путём".to_string());
    }
    if chosen.parent().is_none() {
        return Ok(chosen.join(product));
    }
    let metadata = match fs::metadata(chosen) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(chosen.to_path_buf()),
    };
    if !metadata.is_dir() {
        return Err("Выбранный путь — это файл, а не папка".to_string());
    }
    if read_marker(chosen).is_some() {
        return Ok(chosen.to_path_buf());
    }
    let empty = fs::read_dir(chosen).map(|mut dir| dir.next().is_none()).unwrap_or(false);
    if empty {
        Ok(chosen.to_path_buf())
    } else {
        Ok(chosen.join(product))
    }
}

/// Free bytes on the volume that holds `dir` (walks up to the first existing folder).
pub fn free_space(dir: &Path) -> Option<u64> {
    let mut current = Some(dir);
    for _ in 0..64 {
        let path = current?;
        if let Ok(bytes) = fs2::available_space(path) {
            return Some(bytes);
        }
        current = path.parent();
    }
    None
}

fn swap_in(staging: &Path, final_dir: &Path) -> Result<(), String> {
    let mut old = final_dir.as_os_str().to_os_string();
    old.push(".chald-old");
    let old = PathBuf::from(old);
    if exists(&old) {
        fs::remove_dir_all(&old).map_err(|e| io_message(&e))?;
    }
    if exists(final_dir) {
        if read_marker(final_dir).is_some() {
            fs::rename(final_dir, &old).map_err(|e| io_message(&e))?;
        } else {
            // resolve_install_dir only lets an empty folder through here.
            fs::remove_dir(final_dir).map_err(|e| io_message(&e))?;
        }
    }
    match fs::rename(staging, final_dir) {
        Ok(()) => {
            let _ = fs::remove_dir_all(&old);
            Ok(())
        }
        Err(error) => {
            if exists(&old) {
                let _ = fs::rename(&old, final_dir);
            }
            Err(io_message(&error))
        }
    }
}

/// Installs the embedded payload: verify every file, unpack into a staging folder, then swap it in.
/// A failure at any point leaves the previous installation untouched.
pub fn install<E: InstallEnv>(
    payload: &[u8],
    opts: &InstallOptions,
    env: &E,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(InstallProgress),
) -> Result<InstallResult, String> {
    let header = read_header_bytes(payload)?;
    let meta: &PayloadMeta = &header.meta;
    let final_dir = resolve_install_dir(&opts.dir, &meta.product_name)?;
    let mut staging = final_dir.as_os_str().to_os_string();
    staging.push(".chald-new");
    let staging = PathBuf::from(staging);
    let mut warnings = Vec::new();

    on_progress(progress("prepare", 1, None));
    let need = (header.total_size as f64 * 1.05) as u64 + 50 * 1024 * 1024;
    if let Some(free) = free_space(&final_dir) {
        if free < need {
            return Err("Недостаточно места на диске.".to_string());
        }
    }
    if cancel.load(Ordering::SeqCst) {
        return Err("Установка отменена".to_string());
    }

    on_progress(progress("prepare", 2, None));
    if let Some(parent) = final_dir.parent() {
        fs::create_dir_all(parent).map_err(|e| io_message(&e))?;
    }
    if exists(&staging) {
        fs::remove_dir_all(&staging).map_err(|e| io_message(&e))?;
    }

    let result = (|| -> Result<(), String> {
        extract_bytes(payload, &header, &staging, cancel, |done, total, file| {
            let percent = 3 + (92 * done / total.max(1)) as u32;
            on_progress(progress("extract", percent.min(95), Some(file)));
        })?;
        if cancel.load(Ordering::SeqCst) {
            return Err("Установка отменена".to_string());
        }
        if !exists(&staging.join(&meta.exe)) {
            return Err(format!("После распаковки не найден {}", meta.exe));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let exe = staging.join(&meta.exe);
            if let Ok(metadata) = fs::metadata(&exe) {
                let mut permissions = metadata.permissions();
                permissions.set_mode(0o755);
                let _ = fs::set_permissions(&exe, permissions);
            }
        }
        on_progress(progress("finish", 96, None));

        let marker = serde_json::json!({
            "product": meta.product_name,
            "version": meta.version,
            "installedAt": chrono::Local::now().to_rfc3339(),
        });
        fs::write(staging.join(MARKER), serde_json::to_vec_pretty(&marker).map_err(|e| e.to_string())?)
            .map_err(|e| io_message(&e))?;
        let script = build_uninstall_script(&UninstallParams {
            product_name: &meta.product_name,
            exe: &meta.exe,
            key_name: &env::uninstall_key_name(meta),
        });
        fs::write(staging.join(UNINSTALL_SCRIPT), script).map_err(|e| io_message(&e))?;

        if cancel.load(Ordering::SeqCst) {
            return Err("Установка отменена".to_string());
        }
        env.stop_running(&meta.exe)?;
        swap_in(&staging, &final_dir)?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    let exe_path = final_dir.join(&meta.exe);
    on_progress(progress("finish", 98, None));

    if opts.desktop_shortcut {
        let link = env.desktop_dir().join(format!("{}.lnk", meta.product_name));
        if !env.create_shortcut(&link, &exe_path, &final_dir, &meta.product_name) {
            warnings.push("Не удалось создать ярлык на рабочем столе.".to_string());
        }
    }
    if opts.start_menu_shortcut {
        let folder = env.start_menu_dir().join(&meta.product_name);
        if fs::create_dir_all(&folder).is_ok() {
            let link = folder.join(format!("{}.lnk", meta.product_name));
            if !env.create_shortcut(&link, &exe_path, &final_dir, &meta.product_name) {
                warnings.push("Не удалось создать ярлык в меню «Пуск».".to_string());
            }
        } else {
            warnings.push("Не удалось создать папку в меню «Пуск».".to_string());
        }
    }

    if let Err(error) = env.register(meta, &final_dir, header.total_size.div_ceil(1024)) {
        warnings.push(format!("Запись в «Приложения и возможности»: {error}"));
    }

    on_progress(progress("finish", 100, None));
    Ok(InstallResult {
        dir: final_dir.to_string_lossy().to_string(),
        exe_path: exe_path.to_string_lossy().to_string(),
        version: meta.version.clone(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{pack_dir, PayloadMeta};
    use std::cell::RefCell;

    struct FakeEnv {
        desktop: PathBuf,
        menu: PathBuf,
        shortcuts: RefCell<Vec<PathBuf>>,
        registered: RefCell<bool>,
    }

    impl InstallEnv for FakeEnv {
        fn desktop_dir(&self) -> PathBuf {
            self.desktop.clone()
        }
        fn start_menu_dir(&self) -> PathBuf {
            self.menu.clone()
        }
        fn create_shortcut(&self, lnk: &Path, _target: &Path, _cwd: &Path, _description: &str) -> bool {
            self.shortcuts.borrow_mut().push(lnk.to_path_buf());
            true
        }
        fn register(&self, _meta: &PayloadMeta, _dir: &Path, _size_kb: u64) -> Result<(), String> {
            *self.registered.borrow_mut() = true;
            Ok(())
        }
    }

    fn payload(dir: &Path) -> Vec<u8> {
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("ChaldLauncher.exe"), b"exe").unwrap();
        let out = dir.join("payload.chpk");
        let meta = PayloadMeta {
            version: "1.1.0".into(),
            product_name: "ChaldLauncher".into(),
            exe: "ChaldLauncher.exe".into(),
            publisher: "Chald Studio".into(),
            app_id: "studio.chald.launcher".into(),
        };
        pack_dir(&src, &out, &meta, |_, _, _| {}).unwrap();
        fs::read(&out).unwrap()
    }

    #[test]
    fn resolve_dir_rules() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(resolve_install_dir(&empty, "ChaldLauncher").unwrap(), empty);

        let full = dir.path().join("full");
        fs::create_dir_all(&full).unwrap();
        fs::write(full.join("other.txt"), b"x").unwrap();
        assert_eq!(resolve_install_dir(&full, "ChaldLauncher").unwrap(), full.join("ChaldLauncher"));

        let installed = dir.path().join("installed");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join(MARKER), b"{\"version\":\"1.0.0\"}").unwrap();
        assert_eq!(resolve_install_dir(&installed, "ChaldLauncher").unwrap(), installed);

        assert!(resolve_install_dir(Path::new("relative"), "ChaldLauncher").is_err());
        let file = dir.path().join("file.txt");
        fs::write(&file, b"x").unwrap();
        assert!(resolve_install_dir(&file, "ChaldLauncher").is_err());
    }

    #[test]
    fn install_writes_files_marker_and_uninstaller() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = payload(dir.path());
        let env = FakeEnv {
            desktop: dir.path().join("desktop"),
            menu: dir.path().join("menu"),
            shortcuts: RefCell::new(Vec::new()),
            registered: RefCell::new(false),
        };
        let target = dir.path().join("install");
        let cancel = AtomicBool::new(false);
        let mut percents = Vec::new();
        let result = install(
            &bytes,
            &InstallOptions { dir: target.clone(), desktop_shortcut: true, start_menu_shortcut: true },
            &env,
            &cancel,
            |p| percents.push(p.percent),
        )
        .unwrap();
        assert!(target.join("ChaldLauncher.exe").is_file());
        assert!(target.join(MARKER).is_file());
        assert!(target.join(UNINSTALL_SCRIPT).is_file());
        assert_eq!(read_marker(&target).as_deref(), Some("1.1.0"));
        assert_eq!(result.version, "1.1.0");
        assert_eq!(env.shortcuts.borrow().len(), 2);
        assert!(*env.registered.borrow());
        assert_eq!(percents.last().copied(), Some(100));
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn cancel_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = payload(dir.path());
        let env = FakeEnv {
            desktop: dir.path().join("desktop"),
            menu: dir.path().join("menu"),
            shortcuts: RefCell::new(Vec::new()),
            registered: RefCell::new(false),
        };
        let target = dir.path().join("install");
        let cancel = AtomicBool::new(true);
        let error = install(
            &bytes,
            &InstallOptions { dir: target.clone(), desktop_shortcut: false, start_menu_shortcut: false },
            &env,
            &cancel,
            |_| {},
        )
        .unwrap_err();
        assert!(error.contains("отменена"));
        assert!(!target.exists());
    }

    #[test]
    fn free_space_is_reported() {
        assert!(free_space(Path::new(".")).unwrap() > 0);
    }
}
