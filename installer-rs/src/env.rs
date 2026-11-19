use std::path::{Path, PathBuf};

use crate::payload::PayloadMeta;

/// A copy of the product already installed on this machine (registry + our marker).
#[derive(Debug, Clone)]
pub struct ExistingInstall {
    pub dir: PathBuf,
    pub version: String,
}

/// Everything that touches the system, so the install logic can be tested without Windows.
pub trait InstallEnv {
    fn desktop_dir(&self) -> PathBuf;
    fn start_menu_dir(&self) -> PathBuf;
    fn create_shortcut(&self, lnk: &Path, target: &Path, cwd: &Path, description: &str) -> bool;
    fn stop_running(&self, _exe: &str) -> Result<(), String> {
        Ok(())
    }
    fn register(&self, _meta: &PayloadMeta, _dir: &Path, _size_kb: u64) -> Result<(), String> {
        Ok(())
    }
}

pub fn uninstall_key_name(meta: &PayloadMeta) -> String {
    meta.product_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect()
}

pub fn registry_key(meta: &PayloadMeta) -> String {
    format!(
        "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{}",
        uninstall_key_name(meta)
    )
}

/// `%LOCALAPPDATA%\Programs\<product>` like the old installer.
pub fn default_install_dir(meta: &PayloadMeta) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join("AppData").join("Local")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Programs").join(&meta.product_name)
}

/// Already installed copy: the registry entry points at a folder that still has our marker.
pub fn find_existing(meta: &PayloadMeta) -> Option<ExistingInstall> {
    #[cfg(windows)]
    {
        let key = registry_key(meta);
        let output = std::process::Command::new("reg")
            .args(["query", &key, "/v", "InstallLocation"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let Some(pos) = line.find("InstallLocation") else { continue };
            let rest = line[pos + "InstallLocation".len()..].trim();
            let value = rest.strip_prefix("REG_SZ").map(str::trim).unwrap_or("");
            if value.is_empty() {
                continue;
            }
            let dir = PathBuf::from(value);
            if let Some(version) = crate::setup::read_marker(&dir) {
                return Some(ExistingInstall { dir, version });
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        None
    }
}

#[cfg(windows)]
fn psq(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub struct SystemEnv;

impl InstallEnv for SystemEnv {
    fn desktop_dir(&self) -> PathBuf {
        dirs::desktop_dir()
            .or_else(|| dirs::home_dir().map(|home| home.join("Desktop")))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn start_menu_dir(&self) -> PathBuf {
        #[cfg(windows)]
        {
            let base = std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            base.join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
        }
        #[cfg(not(windows))]
        {
            dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("start-menu")
        }
    }

    fn create_shortcut(&self, lnk: &Path, target: &Path, cwd: &Path, description: &str) -> bool {
        #[cfg(windows)]
        {
            if let Some(parent) = lnk.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let script = format!(
                "$ws = New-Object -ComObject WScript.Shell; $sc = $ws.CreateShortcut({}); $sc.TargetPath = {}; $sc.WorkingDirectory = {}; $sc.Description = {}; $sc.IconLocation = {}; $sc.Save()",
                psq(&lnk.to_string_lossy()),
                psq(&target.to_string_lossy()),
                psq(&cwd.to_string_lossy()),
                psq(description),
                psq(&format!("{},0", target.to_string_lossy())),
            );
            return std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
        }
        #[cfg(not(windows))]
        {
            let _ = (lnk, target, cwd, description);
            false
        }
    }

    fn stop_running(&self, exe: &str) -> Result<(), String> {
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/T", "/IM", exe])
                .output();
            std::thread::sleep(std::time::Duration::from_millis(600));
        }
        #[cfg(not(windows))]
        {
            let _ = exe;
        }
        Ok(())
    }

    fn register(&self, meta: &PayloadMeta, dir: &Path, size_kb: u64) -> Result<(), String> {
        #[cfg(windows)]
        {
            let key = registry_key(meta);
            let exe_path = dir.join(&meta.exe);
            let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
            let powershell = Path::new(&system_root)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe");
            let uninstall = format!(
                "\"{}\" -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File \"{}\"",
                powershell.display(),
                dir.join(crate::setup::UNINSTALL_SCRIPT).display()
            );
            let date = chrono::Local::now().format("%Y%m%d").to_string();
            let values: [(&str, &str, String); 10] = [
                ("DisplayName", "REG_SZ", meta.product_name.clone()),
                ("DisplayVersion", "REG_SZ", meta.version.clone()),
                ("Publisher", "REG_SZ", meta.publisher.clone()),
                ("InstallLocation", "REG_SZ", dir.to_string_lossy().to_string()),
                ("DisplayIcon", "REG_SZ", exe_path.to_string_lossy().to_string()),
                ("UninstallString", "REG_SZ", uninstall),
                ("InstallDate", "REG_SZ", date),
                ("EstimatedSize", "REG_DWORD", size_kb.to_string()),
                ("NoModify", "REG_DWORD", "1".to_string()),
                ("NoRepair", "REG_DWORD", "1".to_string()),
            ];
            for (name, kind, value) in values {
                let status = std::process::Command::new("reg")
                    .args(["add", &key, "/v", name, "/t", kind, "/d", &value, "/f"])
                    .status()
                    .map_err(|e| e.to_string())?;
                if !status.success() {
                    return Err(format!("Не удалось записать {name} в реестр"));
                }
            }
            return Ok(());
        }
        #[cfg(not(windows))]
        {
            let _ = (meta, dir, size_kb);
            Ok(())
        }
    }
}
