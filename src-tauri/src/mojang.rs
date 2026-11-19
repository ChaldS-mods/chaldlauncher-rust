
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "windows")]
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::{AppError, AppResult};

pub const MOJANG_GAMES: [&str; 3] = ["bedrock", "dungeons", "legends"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MojangGameStatus {
    pub id: String,
    pub via: String,   // "direct" | "launcher"
    pub state: String, // "ready" | "missing"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MojangStatus {
    pub supported: bool,
    pub launcher_found: bool,
    pub games: Vec<MojangGameStatus>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BedrockServer {
    pub name: String,
    pub host: String,
    pub port: u16,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct AppxPkg {
    name: String,
    family: String,
    app_id: Option<String>,
}

#[cfg(target_os = "windows")]
const APPX_SCRIPT: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$list = @(Get-AppxPackage | Where-Object { $_.Name -match 'Minecraft|4297127D64EC6' } | ForEach-Object {
  $id = (Get-AppxPackageManifest -Package $_.PackageFullName).Package.Applications.Application | Select-Object -First 1 -ExpandProperty Id
  [pscustomobject]@{ Name = $_.Name; Family = $_.PackageFamilyName; AppId = $id }
})
ConvertTo-Json -InputObject $list -Compress
"#;

#[allow(dead_code)]
fn is_safe_part(s: &str) -> bool {
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes.iter().all(|b| {
            matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')
        })
}

#[allow(dead_code)]
pub(crate) fn parse_appx(stdout: &str) -> Vec<AppxPkg> {
    let text = stdout.trim().trim_start_matches('\u{feff}');
    if text.is_empty() {
        return Vec::new();
    }
    let raw: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let items: Vec<&Value> = match raw.as_array() {
        Some(a) => a.iter().collect(),
        None => vec![&raw],
    };
    let mut out = Vec::new();
    for it in items {
        let Some(obj) = it.as_object() else { continue };
        let Some(name) = obj.get("Name").and_then(Value::as_str) else { continue };
        let Some(family) = obj.get("Family").and_then(Value::as_str) else { continue };
        if !is_safe_part(family) {
            continue;
        }
        let app_id = obj
            .get("AppId")
            .and_then(Value::as_str)
            .filter(|s| is_safe_part(s))
            .map(|s| s.to_string());
        out.push(AppxPkg {
            name: name.to_string(),
            family: family.to_string(),
            app_id,
        });
    }
    out
}

#[allow(dead_code)]
fn find_bedrock(pkgs: &[AppxPkg]) -> Option<&AppxPkg> {
    pkgs.iter().find(|p| p.name == "Microsoft.MinecraftUWP")
}

#[allow(dead_code)]
fn find_store_launcher(pkgs: &[AppxPkg]) -> Option<&AppxPkg> {
    pkgs.iter().find(|p| {
        p.name.contains("4297127D64EC6") || p.name.to_lowercase().contains("minecraftlauncher")
    })
}

#[cfg(target_os = "windows")]
fn launcher_exe_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(v) = std::env::var("ProgramFiles(x86)") {
        if !v.trim().is_empty() {
            roots.push(v);
        }
    }
    if let Ok(v) = std::env::var("ProgramFiles") {
        if !v.trim().is_empty() {
            roots.push(v);
        }
    }
    roots
        .into_iter()
        .map(|r| Path::new(&r).join("Minecraft Launcher").join("MinecraftLauncher.exe"))
        .collect()
}

#[allow(dead_code)]
fn is_valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

#[allow(dead_code)]
fn is_valid_name(name: &str) -> bool {
    let n = name.chars().count();
    n >= 1
        && n <= 32
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '.' | '-')
        })
}

#[allow(dead_code)]
pub fn parse_bedrock_server(name: &str, host: &str, port: i64) -> Option<BedrockServer> {
    let name = name.trim();
    let host = host.trim().to_lowercase();
    if !is_valid_name(name) || !is_valid_host(&host) {
        return None;
    }
    if !(1..=65535).contains(&port) {
        return None;
    }
    Some(BedrockServer {
        name: name.to_string(),
        host,
        port: port as u16,
    })
}

#[allow(dead_code)]
pub fn bedrock_uri(server: Option<&BedrockServer>) -> String {
    match server {
        Some(s) => format!(
            "minecraft://?addExternalServer={}|{}:{}",
            urlencoding::encode(&s.name),
            s.host,
            s.port
        ),
        None => "minecraft:".to_string(),
    }
}

pub fn store_uri(game: &str) -> &'static str {
    // Microsoft Store product IDs (checked against xbox.com store pages):
    // «Minecraft for Windows» (Bedrock) and the official «Minecraft Launcher», which starts Dungeons and Legends.
    match game {
        "bedrock" => "ms-windows-store://pdp/?ProductId=9NBLGGH2JHXJ",
        _ => "ms-windows-store://pdp/?ProductId=9PGW18NPBZV5",
    }
}

fn require_windows() -> AppResult<()> {
    if !cfg!(target_os = "windows") {
        return Err(AppError::new(
            "MOJANG_UNSUPPORTED",
            "Эти игры запускаются только в Windows.",
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn powershell(script: &str) -> Option<String> {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(target_os = "windows")]
fn scan_windows() -> (Option<AppxPkg>, Option<LauncherTarget>) {
    let pkgs = powershell(APPX_SCRIPT).map(|s| parse_appx(&s)).unwrap_or_default();
    let bedrock = find_bedrock(&pkgs).cloned();
    let mut launcher: Option<LauncherTarget> = None;
    for file in launcher_exe_candidates() {
        if file.exists() {
            launcher = Some(LauncherTarget::Exe(file));
            break;
        }
    }
    if launcher.is_none() {
        if let Some(store) = find_store_launcher(&pkgs) {
            if let Some(app_id) = &store.app_id {
                launcher = Some(LauncherTarget::Store(format!("{}!{}", store.family, app_id)));
            }
        }
    }
    (bedrock, launcher)
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
enum LauncherTarget {
    Exe(PathBuf),
    Store(String),
}

pub async fn status() -> AppResult<MojangStatus> {
    if !cfg!(target_os = "windows") {
        return Ok(MojangStatus {
            supported: false,
            launcher_found: false,
            games: Vec::new(),
        });
    }
    #[cfg(target_os = "windows")]
    {
        let (bedrock, launcher) = tauri::async_runtime::spawn_blocking(scan_windows)
            .await
            .unwrap_or((None, None));
        let launcher_found = launcher.is_some();
        Ok(MojangStatus {
            supported: true,
            launcher_found,
            games: vec![
                MojangGameStatus {
                    id: "bedrock".into(),
                    via: "direct".into(),
                    state: if bedrock.is_some() { "ready" } else { "missing" }.into(),
                },
                MojangGameStatus {
                    id: "dungeons".into(),
                    via: "launcher".into(),
                    state: if launcher_found { "ready" } else { "missing" }.into(),
                },
                MojangGameStatus {
                    id: "legends".into(),
                    via: "launcher".into(),
                    state: if launcher_found { "ready" } else { "missing" }.into(),
                },
            ],
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(MojangStatus {
            supported: false,
            launcher_found: false,
            games: Vec::new(),
        })
    }
}

pub async fn launch(game: &str, server: Option<(String, String, i64)>) -> AppResult<()> {
    require_windows()?;
    if !MOJANG_GAMES.contains(&game) {
        return Err(AppError::new("INVALID_ARGS", "Неизвестная игра"));
    }
    #[cfg(target_os = "windows")]
    {
        if game == "bedrock" {
            let (bedrock, _) = tauri::async_runtime::spawn_blocking(scan_windows)
                .await
                .unwrap_or((None, None));
            if bedrock.is_none() {
                return Err(AppError::new(
                    "MOJANG_NOT_FOUND",
                    "Minecraft Bedrock не найден на этом компьютере. Получи его в Microsoft Store.",
                ));
            }
            let mut uri = bedrock_uri(None);
            if let Some((name, host, port)) = server {
                let s = parse_bedrock_server(&name, &host, port).ok_or_else(|| {
                    AppError::new(
                        "MOJANG_BAD_SERVER",
                        "Некорректный сервер: имя до 32 символов (латиница, цифры, пробел, _ . -), адрес — домен или IPv4, порт 1–65535.",
                    )
                })?;
                uri = bedrock_uri(Some(&s));
            }
            open::that(&uri).map_err(|e| AppError::new("MOJANG_LAUNCH_FAILED", e.to_string()))?;
            return Ok(());
        }
        // Dungeons and Legends are started from the official Minecraft Launcher
        let (_, launcher) = tauri::async_runtime::spawn_blocking(scan_windows)
            .await
            .unwrap_or((None, None));
        let launcher = launcher.ok_or_else(|| {
            AppError::new(
                "MOJANG_NOT_FOUND",
                "Официальный Minecraft Launcher не найден на этом компьютере. Получи его в Microsoft Store.",
            )
        })?;
        match launcher {
            LauncherTarget::Exe(file) => {
                Command::new(&file)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| AppError::new("MOJANG_LAUNCH_FAILED", e.to_string()))?;
            }
            LauncherTarget::Store(aumid) => {
                const CREATE_NO_WINDOW: u32 = 0x08000000;
                Command::new("explorer.exe")
                    .arg(format!("shell:AppsFolder\\{aumid}"))
                    .creation_flags(CREATE_NO_WINDOW)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| AppError::new("MOJANG_LAUNCH_FAILED", e.to_string()))?;
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (game, server);
        Err(AppError::new(
            "MOJANG_UNSUPPORTED",
            "Эти игры запускаются только в Windows.",
        ))
    }
}

pub async fn open_store(game: &str) -> AppResult<()> {
    require_windows()?;
    if !MOJANG_GAMES.contains(&game) {
        return Err(AppError::new("INVALID_ARGS", "Неизвестная игра"));
    }
    open::that(store_uri(game)).map_err(|e| AppError::new("MOJANG_STORE_FAILED", e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_appx_handles_array_and_single_object() {
        let json = r#"[{"Name":"Microsoft.MinecraftUWP","Family":"Microsoft.MinecraftUWP_8wekyb3d8bbwe","AppId":"App"},
{"Name":"Microsoft.4297127D64EC6","Family":"Microsoft.4297127D64EC6_8wekyb3d8bbwe","AppId":"Minecraft"}]"#;
        let pkgs = parse_appx(json);
        assert_eq!(pkgs.len(), 2);
        assert!(find_bedrock(&pkgs).is_some());
        assert!(find_store_launcher(&pkgs).is_some());

        let single = r#"{"Name":"Microsoft.MinecraftUWP","Family":"Microsoft.MinecraftUWP_8wekyb3d8bbwe","AppId":"App"}"#;
        let pkgs = parse_appx(single);
        assert_eq!(pkgs.len(), 1);
    }

    #[test]
    fn parse_appx_rejects_invalid_and_empty() {
        assert!(parse_appx("").is_empty());
        assert!(parse_appx("not json").is_empty());
        // unsafe family/app id filtered
        let bad = r#"[{"Name":"X","Family":"bad family!","AppId":"App"}]"#;
        assert!(parse_appx(bad).is_empty());
    }

    #[test]
    fn bedrock_server_validation_and_uri() {
        let s = parse_bedrock_server("My Server", "play.example.com", 19132).unwrap();
        assert_eq!(s.host, "play.example.com");
        assert_eq!(bedrock_uri(Some(&s)), "minecraft://?addExternalServer=My%20Server|play.example.com:19132");
        assert_eq!(bedrock_uri(None), "minecraft:");

        assert!(parse_bedrock_server("ok", "1.2.3.4", 19132).is_some());
        assert!(parse_bedrock_server("", "example.com", 19132).is_none());
        assert!(parse_bedrock_server("name", "bad host", 19132).is_none());
        assert!(parse_bedrock_server("name", "example.com", 0).is_none());
        assert!(parse_bedrock_server("name", "example.com", 70000).is_none());
    }

    #[test]
    fn store_uris_match_products() {
        assert!(store_uri("bedrock").contains("9NBLGGH2JHXJ"));
        assert!(store_uri("dungeons").contains("9PGW18NPBZV5"));
        assert!(store_uri("legends").contains("9PGW18NPBZV5"));
    }
}
