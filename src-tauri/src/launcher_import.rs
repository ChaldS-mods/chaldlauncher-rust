//! Импорт инстансов из других лаунчеров. Порт `src/main/launcherImport.ts`.
//!
//! Источник читается только для чтения: в чужих лаунчерах ничего не меняется.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::AppHandle;

use crate::errors::{AppError, AppResult};
use crate::game;
use crate::instances;
use crate::paths;
use crate::state::AppState;
use crate::versions;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCandidate {
    pub token: String,
    pub name: String,
    pub mc_version: String,
    pub loader: Loader,
    pub mods: u32,
    pub worlds: u32,
    pub imported: bool,
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherSource {
    pub id: String,
    pub name: String,
    pub path: Option<String>,
    pub found: bool,
    pub candidates: Vec<ImportCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Loader {
    #[serde(rename = "type")]
    pub r#type: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone)]
struct RawInstance {
    launcher: String,
    name: String,
    description: String,
    dir: PathBuf,
    game_dir: PathBuf,
    mc_version: String,
    loader: Loader,
    problem: Option<String>,
    memory_min_mb: Option<u32>,
    memory_max_mb: Option<u32>,
    jvm_args: String,
    width: Option<u32>,
    height: Option<u32>,
    fullscreen: Option<bool>,
    last_played_at: Option<String>,
    icon_file: Option<PathBuf>,
    mods: u32,
    worlds: u32,
    cf_index: Option<std::collections::HashMap<String, CfFile>>,
}

fn launcher_name(id: &str) -> &'static str {
    match id {
        "prism" => "Prism Launcher",
        "multimc" => "MultiMC",
        "polymc" => "PolyMC",
        "modrinth" => "Modrinth App",
        "curseforge" => "CurseForge",
        "official" => "Minecraft Launcher",
        _ => "Лаунчер",
    }
}

fn clean_text(text: &str, max: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

fn parse_cfg(text: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') || line.starts_with('[') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        if eq == 0 {
            continue;
        }
        out.insert(line[..eq].trim().to_string(), line[eq + 1..].trim().to_string());
    }
    out
}

fn to_int(value: Option<&str>) -> Option<u64> {
    let value = value?.trim();
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn memory_or_null(mb: Option<u64>) -> Option<u32> {
    let mb = mb?;
    if (256..=65_536).contains(&mb) {
        Some(mb as u32)
    } else {
        None
    }
}

fn to_iso(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let ms = if value.chars().all(|c| c.is_ascii_digit()) {
        let number: i64 = value.parse().ok()?;
        if number < 100_000_000_000 { number * 1000 } else { number }
    } else {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|date| date.timestamp_millis())?
    };
    let min = chrono::DateTime::parse_from_rfc3339("2009-01-01T00:00:00Z")
        .ok()?
        .timestamp_millis();
    let max = chrono::Utc::now().timestamp_millis() + 86_400_000;
    if !(min..=max).contains(&ms) {
        return None;
    }
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn is_mc_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 40
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
}

pub fn strip_heap_args(args: &str) -> String {
    args.split_whitespace()
        .filter(|part| !part.to_ascii_lowercase().starts_with("-xms") && !part.to_ascii_lowercase().starts_with("-xmx"))
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(2000)
        .collect()
}

fn heap_mb(args: &str, kind: char) -> Option<u32> {
    let needle = if kind == 'x' { "-Xmx" } else { "-Xms" };
    let lower = args.to_ascii_lowercase();
    let start = lower.find(&needle.to_ascii_lowercase())? + needle.len();
    let rest = &args[start..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let number: u64 = digits.parse().ok()?;
    let unit = rest.chars().nth(digits.len()).unwrap_or('m').to_ascii_lowercase();
    let mb = match unit {
        'g' => number.saturating_mul(1024),
        'k' => number / 1024,
        _ => number,
    };
    memory_or_null(Some(mb))
}

fn bare_loader_version(loader_type: &str, mc: &str, version: &str) -> String {
    let mut out = version.trim().to_string();
    if loader_type == "neoforge" {
        if let Some(stripped) = out.strip_prefix("neoforge-") {
            out = stripped.to_string();
        }
    }
    if loader_type == "forge" {
        if let Some(stripped) = out.strip_prefix("forge-") {
            out = stripped.to_string();
        }
    }
    let prefix = format!("{mc}-");
    if let Some(stripped) = out.strip_prefix(&prefix) {
        out = stripped.to_string();
    }
    out
}

pub fn neoforge_to_mc(version: &str) -> Option<String> {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let a: u32 = parts[0].parse().ok()?;
    let b: u32 = parts[1].parse().ok()?;
    if a >= 26 {
        let c: u32 = parts.get(2).and_then(|value| value.parse().ok()).unwrap_or(0);
        Some(if c > 0 { format!("{a}.{b}.{c}") } else { format!("{a}.{b}") })
    } else {
        Some(if b > 0 { format!("1.{a}.{b}") } else { format!("1.{a}") })
    }
}

fn loader_by_uid(uid: &str) -> Option<&'static str> {
    match uid {
        "net.fabricmc.fabric-loader" => Some("fabric"),
        "org.quiltmc.quilt-loader" => Some("quilt"),
        "net.minecraftforge" => Some("forge"),
        "net.neoforged" => Some("neoforge"),
        _ => None,
    }
}

pub fn parse_mmc_pack(json: &Value) -> Result<(String, Loader), String> {
    let Some(comps) = json.get("components").and_then(Value::as_array) else {
        return Err("mmc-pack.json повреждён".to_string());
    };
    let mut mc = String::new();
    let mut loader = Loader { r#type: "vanilla".to_string(), version: None };
    for component in comps {
        let uid = component.get("uid").and_then(Value::as_str).unwrap_or("");
        let version = component
            .get("version")
            .and_then(Value::as_str)
            .or_else(|| component.get("cachedVersion").and_then(Value::as_str))
            .unwrap_or("");
        if uid == "net.minecraft" {
            mc = version.to_string();
        } else if uid == "com.mumfrey.liteloader" {
            return Err("LiteLoader не поддерживается".to_string());
        } else if let Some(loader_type) = loader_by_uid(uid) {
            let version = if version.is_empty() {
                None
            } else {
                Some(bare_loader_version(loader_type, &mc, version))
            };
            loader = Loader { r#type: loader_type.to_string(), version };
        }
    }
    if mc.is_empty() {
        return Err("В сборке не указана версия Minecraft".to_string());
    }
    if let Some(version) = &loader.version {
        loader.version = Some(bare_loader_version(&loader.r#type, &mc, version));
    }
    Ok((mc, loader))
}

fn as_loader_type(value: &str) -> Option<&'static str> {
    let normalized = value.to_ascii_lowercase().replace([' ', '_', '-'], "");
    match normalized.as_str() {
        "vanilla" | "" => Some("vanilla"),
        "fabric" => Some("fabric"),
        "quilt" => Some("quilt"),
        "forge" => Some("forge"),
        "neoforge" => Some("neoforge"),
        _ => None,
    }
}

pub fn parse_version_id(id: &str) -> Result<(String, Loader), String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("Не указана версия".to_string());
    }
    if id.to_ascii_lowercase().contains("optifine") {
        return Err("Профили OptiFine не поддерживаются (поставь OptiFine как мод)".to_string());
    }
    // fabric-loader-0.15.7-1.20.1 / quilt-loader-...
    if let Some((left, mc)) = id.split_once("-loader-").and_then(|(left, rest)| {
        let loader_type = as_loader_type(left)?;
        if !["fabric", "quilt"].contains(&loader_type) {
            return None;
        }
        let (loader_version, mc) = rest.rsplit_once('-')?;
        Some(((loader_type, loader_version), mc))
    }) {
        return Ok((
            mc.to_string(),
            Loader { r#type: left.0.to_string(), version: Some(left.1.to_string()) },
        ));
    }
    // 1.20.1-forge-47.2.0
    if let Some(index) = id.find("-forge-") {
        let mc = &id[..index];
        let version = &id[index + "-forge-".len()..];
        let version = version.strip_prefix(&format!("{mc}-")).unwrap_or(version);
        if !version.is_empty() {
            return Ok((
                mc.to_string(),
                Loader { r#type: "forge".to_string(), version: Some(version.to_string()) },
            ));
        }
    }
    // neoforge-20.4.237
    if let Some(version) = id.strip_prefix("neoforge-") {
        return match neoforge_to_mc(version) {
            Some(mc) => Ok((
                mc,
                Loader { r#type: "neoforge".to_string(), version: Some(version.to_string()) },
            )),
            None => Err(format!("Не удалось определить версию Minecraft для {id}")),
        };
    }
    if id.starts_with("latest-") {
        return Err("Профиль без конкретной версии".to_string());
    }
    let lower = id.to_ascii_lowercase();
    if ["forge", "fabric", "quilt", "liteloader", "rift"].iter().any(|key| lower.contains(key)) {
        return Err(format!("Неизвестный профиль загрузчика: {id}"));
    }
    Ok((id.to_string(), Loader { r#type: "vanilla".to_string(), version: None }))
}

pub fn parse_cf_loader(instance: &Value) -> Result<(String, Loader), String> {
    let base = instance.get("baseModLoader");
    let mc = instance
        .get("gameVersion")
        .and_then(Value::as_str)
        .or_else(|| base.and_then(|b| b.get("minecraftVersion")).and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    if mc.is_empty() {
        return Err("Не указана версия Minecraft".to_string());
    }
    let Some(base) = base.and_then(Value::as_object) else {
        return Ok((mc, Loader { r#type: "vanilla".to_string(), version: None }));
    };
    let name = base.get("name").and_then(Value::as_str).unwrap_or("").trim();
    let by_name = name.split_once('-').and_then(|(left, right)| {
        let loader_type = as_loader_type(left)?;
        if loader_type == "vanilla" {
            None
        } else {
            Some((loader_type, right.to_string()))
        }
    });
    let by_type = base.get("type").and_then(Value::as_i64).and_then(|value| match value {
        1 => Some("forge"),
        4 => Some("fabric"),
        5 => Some("quilt"),
        6 => Some("neoforge"),
        _ => None,
    });
    let Some((loader_type, mut version)) = by_name.or_else(|| {
        by_type.map(|loader_type| {
            (
                loader_type,
                base.get("forgeVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )
        })
    }) else {
        return if name.is_empty() {
            Ok((mc, Loader { r#type: "vanilla".to_string(), version: None }))
        } else {
            Err(format!("Неизвестный загрузчик: {name}"))
        };
    };
    let suffix = format!("-{mc}");
    if version.ends_with(&suffix) {
        version.truncate(version.len() - suffix.len());
    }
    let version = if version.is_empty() {
        None
    } else {
        Some(bare_loader_version(loader_type, &mc, &version))
    };
    Ok((mc, Loader { r#type: loader_type.to_string(), version }))
}

fn neoforge_prefix(mc: &str) -> Option<String> {
    // mirror versions::neoforge_prefix logic roughly: 1.20.2 -> 20.2, 1.21.1 -> 21.1, 26.x -> 26.x
    let parts: Vec<&str> = mc.split('.').collect();
    if parts.first().copied() == Some("1") {
        let minor: u32 = parts.get(1)?.parse().ok()?;
        let patch: u32 = parts.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);
        if minor < 20 || (minor == 20 && patch < 2) {
            return None;
        }
        Some(if patch > 0 { format!("{minor}.{patch}") } else { format!("{minor}") })
    } else {
        let major: u32 = parts.first()?.parse().ok()?;
        if major < 26 {
            return None;
        }
        let minor: u32 = parts.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
        let patch: u32 = parts.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);
        Some(if patch > 0 {
            format!("{major}.{minor}.{patch}")
        } else {
            format!("{major}.{minor}")
        })
    }
}

fn forge_supports_mc(mc: &str) -> bool {
    // Forge in this launcher context is supported from 1.13+
    versions::compare_versions(mc, "1.13") >= 0
}

pub fn check_importable(mc_version: &str, loader: &Loader) -> Option<String> {
    if !is_mc_version(mc_version) {
        return Some(format!("Некорректная версия Minecraft: {}", clean_text(mc_version, 30)));
    }
    if loader.r#type != "vanilla" && loader.version.as_deref().unwrap_or("").is_empty() {
        return Some("Не указана версия загрузчика".to_string());
    }
    if loader.r#type == "forge" && !forge_supports_mc(mc_version) {
        return Some(format!("Forge для Minecraft {mc_version} не поддерживается (нужна 1.13 или новее)"));
    }
    if loader.r#type == "neoforge" && neoforge_prefix(mc_version).is_none() {
        return Some(format!("NeoForge для Minecraft {mc_version} недоступен"));
    }
    None
}


fn list_dirs(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
    }
    names
}

fn read_json(path: &Path) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn count_content(game_dir: &Path) -> (u32, u32) {
    let mut mods = 0u32;
    if let Ok(entries) = fs::read_dir(game_dir.join("mods")) {
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if !kind.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name.ends_with(".jar") || name.ends_with(".jar.disabled") {
                mods += 1;
            }
        }
    }
    let worlds = list_dirs(&game_dir.join("saves"))
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .count() as u32;
    (mods, worlds)
}

fn find_icon(dir: &Path, base: &str) -> Option<PathBuf> {
    for ext in [".png", ".jpg", ".jpeg", ".webp"] {
        let file = dir.join(format!("{base}{ext}"));
        if file.is_file() {
            return Some(file);
        }
    }
    None
}



fn read_mmc_instance(launcher: &str, dir: &Path, icons_dir: Option<&Path>) -> Option<RawInstance> {
    let cfg_text = fs::read_to_string(dir.join("instance.cfg")).ok()?;
    let cfg = parse_cfg(&cfg_text);
    let pack = read_json(&dir.join("mmc-pack.json"));
    let parsed = pack
        .as_ref()
        .map(parse_mmc_pack)
        .unwrap_or_else(|| Err("mmc-pack.json повреждён".to_string()));

    let mut memory_min_mb = None;
    let mut memory_max_mb = None;
    let mut jvm_args = String::new();
    let mut width = None;
    let mut height = None;
    let mut fullscreen = None;

    if cfg.get("OverrideMemory").map(String::as_str) == Some("true") {
        memory_max_mb = memory_or_null(to_int(cfg.get("MaxMemAlloc").map(String::as_str)));
        memory_min_mb = memory_or_null(to_int(cfg.get("MinMemAlloc").map(String::as_str)));
        if memory_min_mb.is_some() && memory_max_mb.is_some() && memory_min_mb > memory_max_mb {
            memory_min_mb = None;
        }
    }
    if cfg.get("OverrideJavaArgs").map(String::as_str) == Some("true") {
        if let Some(args) = cfg.get("JvmArgs") {
            jvm_args = strip_heap_args(args);
        }
    }
    if cfg.get("OverrideWindow").map(String::as_str) == Some("true") {
        if cfg.get("LaunchMaximized").map(String::as_str) == Some("true") {
            fullscreen = Some(true);
        }
        let w = to_int(cfg.get("MinecraftWinWidth").map(String::as_str));
        let h = to_int(cfg.get("MinecraftWinHeight").map(String::as_str));
        if let (Some(w), Some(h)) = (w, h) {
            if (320..=7680).contains(&w) && (240..=4320).contains(&h) {
                width = Some(w as u32);
                height = Some(h as u32);
            }
        }
    }

    let note = cfg.get("notes").cloned().unwrap_or_default();
    let note_line = note.split("\\n").next().unwrap_or("").to_string();
    let icon_key = cfg.get("iconKey").and_then(|value| {
        if value.len() <= 64 && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-') {
            Some(value.as_str())
        } else {
            None
        }
    });
    let icon_file = icon_key.and_then(|key| icons_dir.and_then(|dir| find_icon(dir, key)));

    let (mc_version, loader, problem) = match parsed {
        Ok((mc, loader)) => (mc, loader, None),
        Err(problem) => ("?".to_string(), Loader { r#type: "vanilla".to_string(), version: None }, Some(problem)),
    };
    let game_dir = if dir.join(".minecraft").exists() {
        dir.join(".minecraft")
    } else if dir.join("minecraft").exists() {
        dir.join("minecraft")
    } else {
        dir.join(".minecraft")
    };
    let (mods, worlds) = count_content(&game_dir);
    let problem = problem.or_else(|| check_importable(&mc_version, &loader));
    let name = clean_text(cfg.get("name").map(String::as_str).unwrap_or(""), 48);
    let fallback = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "instance".to_string());

    Some(RawInstance {
        launcher: launcher.to_string(),
        name: if name.is_empty() { fallback } else { name },
        description: clean_text(&note_line, 120),
        dir: dir.to_path_buf(),
        game_dir,
        mc_version,
        loader,
        problem,
        memory_min_mb,
        memory_max_mb,
        jvm_args,
        width,
        height,
        fullscreen,
        last_played_at: to_iso(cfg.get("lastLaunchTime").map(String::as_str)),
        icon_file,
        mods,
        worlds,
        cf_index: None,
    })
}

fn mmc_instances_dir(root: &Path, cfg_names: &[&str]) -> PathBuf {
    for name in cfg_names {
        if let Ok(text) = fs::read_to_string(root.join(name)) {
            if let Some(value) = parse_cfg(&text).get("InstanceDir") {
                if !value.is_empty() {
                    return root.join(value);
                }
            }
        }
    }
    root.join("instances")
}

fn scan_mmc_instances_dir(launcher: &str, instances_dir: &Path, icons_dir: Option<&Path>) -> Vec<RawInstance> {
    let mut out = Vec::new();
    for name in list_dirs(instances_dir) {
        if name.starts_with('.') || name.starts_with('_') {
            continue;
        }
        if let Some(instance) = read_mmc_instance(launcher, &instances_dir.join(&name), icons_dir) {
            out.push(instance);
        }
    }
    out.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
    out
}

fn scan_mmc_root(launcher: &str, root: &Path) -> Vec<RawInstance> {
    let cfg_names: &[&str] = match launcher {
        "multimc" => &["multimc.cfg"],
        "polymc" => &["polymc.cfg"],
        _ => &["prismlauncher.cfg", "polymc.cfg", "multimc.cfg"],
    };
    let instances_dir = mmc_instances_dir(root, cfg_names);
    scan_mmc_instances_dir(launcher, &instances_dir, Some(&root.join("icons")))
}


fn read_cf_instance(dir: &Path) -> Option<RawInstance> {
    let json = read_json(&dir.join("minecraftinstance.json"))?;
    let parsed = parse_cf_loader(&json);
    let (mc_version, loader, problem) = match parsed {
        Ok((mc, loader)) => (mc, loader, None),
        Err(problem) => ("?".to_string(), Loader { r#type: "vanilla".to_string(), version: None }, Some(problem)),
    };
    let name = json
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| clean_text(value, 48))
        .filter(|value| !value.is_empty())
        .or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "instance".to_string());
    let (mods, worlds) = count_content(dir);
    let problem = problem.or_else(|| check_importable(&mc_version, &loader));

    let mut cf_index = std::collections::HashMap::new();
    if let Some(addons) = json.get("installedAddons").and_then(Value::as_array) {
        for addon in addons {
            let Some(mod_id) = addon.get("addonID").and_then(Value::as_i64) else { continue };
            let Some(installed) = addon.get("installedFile") else { continue };
            let Some(file_id) = installed.get("id").and_then(Value::as_i64) else { continue };
            let Some(file_name) = installed.get("fileName").and_then(Value::as_str) else { continue };
            if file_name.is_empty() || file_name.contains('/') || file_name.contains('\\') {
                continue;
            }
            let mut found = false;
            for sub in ["mods", "resourcepacks", "shaderpacks"] {
                let base = dir.join(sub).join(file_name);
                if base.exists() || dir.join(sub).join(format!("{file_name}.disabled")).exists() {
                    found = true;
                    break;
                }
            }
            if found {
                cf_index.insert(mod_id.to_string(), CfFile { file: file_name.to_string(), file_id });
            }
        }
    }
    let cf_index = if cf_index.is_empty() { None } else { Some(cf_index) };

    Some(RawInstance {
        launcher: "curseforge".to_string(),
        name,
        description: String::new(),
        dir: dir.to_path_buf(),
        game_dir: dir.to_path_buf(),
        mc_version,
        loader,
        problem,
        memory_min_mb: None,
        memory_max_mb: None,
        jvm_args: String::new(),
        width: None,
        height: None,
        fullscreen: None,
        last_played_at: to_iso(json.get("lastPlayed").and_then(|v| match v {
            Value::String(s) => Some(s.as_str()),
            Value::Number(_) => None,
            _ => None,
        })).or_else(|| json.get("lastPlayed").and_then(Value::as_i64).map(|n| {
            let ms = if n < 100_000_000_000 { n * 1000 } else { n };
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
                .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        }).flatten()),
        icon_file: find_icon(dir, "profileImage"),
        mods,
        worlds,
        cf_index,
    })
}

fn scan_curseforge_dir(instances_dir: &Path) -> Vec<RawInstance> {
    let mut out = Vec::new();
    for name in list_dirs(instances_dir) {
        if let Some(instance) = read_cf_instance(&instances_dir.join(name)) {
            out.push(instance);
        }
    }
    out.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
    out
}

fn json_args(value: Option<String>) -> String {
    let Some(value) = value else { return String::new() };
    if value.is_empty() {
        return String::new();
    }
    let Ok(parsed) = serde_json::from_str::<Value>(&value) else {
        return String::new();
    };
    let Some(items) = parsed.as_array() else {
        return String::new();
    };
    let joined = items
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    strip_heap_args(&joined)
}

fn read_modrinth_db(db_file: &Path) -> Option<Vec<Value>> {
    // Read from a copy so the running Modrinth App is never disturbed.
    let tmp = std::env::temp_dir().join(format!("chald-mr-{:016x}", rand::random::<u64>()));
    fs::create_dir_all(&tmp).ok()?;
    let copy = tmp.join("app.db");
    let cleanup = |tmp: &Path| {
        let _ = fs::remove_dir_all(tmp);
    };
    if fs::copy(db_file, &copy).is_err() {
        cleanup(&tmp);
        return None;
    }
    for ext in ["-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{}", db_file.to_string_lossy(), ext));
        if src.exists() {
            let dst = PathBuf::from(format!("{}{}", copy.to_string_lossy(), ext));
            let _ = fs::copy(&src, &dst);
        }
    }
    let db = match rusqlite::Connection::open_with_flags(
        &copy,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(db) => db,
        Err(_) => {
            cleanup(&tmp);
            return None;
        }
    };
    let mut stmt = match db.prepare("SELECT * FROM profiles") {
        Ok(stmt) => stmt,
        Err(_) => {
            cleanup(&tmp);
            return None;
        }
    };
    let column_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let result = {
        let rows_iter = stmt.query_map([], |row| {
            let mut map = serde_json::Map::new();
            for (i, name) in column_names.iter().enumerate() {
                let value = match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(v) => json!(v),
                    rusqlite::types::ValueRef::Real(v) => json!(v),
                    rusqlite::types::ValueRef::Text(v) => {
                        json!(String::from_utf8_lossy(v).to_string())
                    }
                    rusqlite::types::ValueRef::Blob(v) => json!(base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        v
                    )),
                };
                map.insert(name.clone(), value);
            }
            Ok(Value::Object(map))
        });
        match rows_iter {
            Ok(iter) => iter.flatten().collect::<Vec<Value>>(),
            Err(_) => Vec::new(),
        }
    };
    drop(stmt);
    drop(db);
    cleanup(&tmp);
    Some(result)
}

fn get_str(row: &Value, key: &str) -> Option<String> {
    row.get(key).and_then(Value::as_str).map(|s| s.to_string())
}

fn get_i64(row: &Value, key: &str) -> Option<i64> {
    row.get(key).and_then(|v| v.as_i64().or_else(|| v.as_u64().map(|n| n as i64)))
}

fn modrinth_from_row(root: &Path, row: &Value) -> Option<RawInstance> {
    let folder = get_str(row, "path").unwrap_or_default();
    if folder.is_empty() || folder.contains('/') || folder.contains('\\') || folder == "." || folder == ".." {
        return None;
    }
    let dir = root.join("profiles").join(&folder);
    if !dir.exists() {
        return None;
    }
    let loader_raw = get_str(row, "mod_loader").unwrap_or_default();
    let recognized = as_loader_type(&loader_raw);
    let loader_type = match recognized {
        Some(t) => Some(t),
        None if loader_raw.is_empty() => Some("vanilla"),
        None => None,
    };
    let mc = get_str(row, "game_version").unwrap_or_default();
    let loader_version = match loader_type {
        Some(t) if t != "vanilla" => get_str(row, "mod_loader_version")
            .filter(|v| !v.is_empty())
            .map(|v| bare_loader_version(t, &mc, &v)),
        _ => None,
    };
    let loader = Loader {
        r#type: loader_type.unwrap_or("vanilla").to_string(),
        version: loader_version,
    };

    let memory_max_mb = memory_or_null(get_i64(row, "override_mc_memory_max").and_then(|v| if v >= 0 { Some(v as u64) } else { None }));
    let jvm_args = json_args(get_str(row, "override_extra_launch_args"));
    let fullscreen = if get_i64(row, "override_mc_force_fullscreen") == Some(1) {
        Some(true)
    } else {
        None
    };
    let mut width = None;
    let mut height = None;
    let w = get_i64(row, "override_mc_game_resolution_x");
    let h = get_i64(row, "override_mc_game_resolution_y");
    if let (Some(w), Some(h)) = (w, h) {
        if (320..=7680).contains(&w) && (240..=4320).contains(&h) {
            width = Some(w as u32);
            height = Some(h as u32);
        }
    }

    let icon_file = get_str(row, "icon_path").and_then(|icon_path| {
        if icon_path.is_empty() {
            return None;
        }
        let path = PathBuf::from(&icon_path);
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif") {
            return None;
        }
        if path.exists() { Some(path) } else { None }
    });

    let raw_name = get_str(row, "name")
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| folder.clone());
    let name = {
        let cleaned = clean_text(&raw_name, 48);
        if cleaned.is_empty() { folder } else { cleaned }
    };
    let (mods, worlds) = count_content(&dir);
    let mut problem = if let Some(_t) = recognized.or(if loader_raw.is_empty() { Some("vanilla") } else { None }) {
        check_importable(&mc, &loader)
    } else {
        Some(format!(
            "Неизвестный загрузчик: {}",
            clean_text(&loader_raw, 20)
        ))
    };
    if mc.is_empty() {
        problem = Some("Не указана версия Minecraft".to_string());
    }

    let last_played_at = row
        .get("last_played")
        .and_then(|v| match v {
            Value::String(s) => to_iso(Some(s)),
            Value::Number(n) => n.as_i64().and_then(|ms| {
                let ms = if ms < 100_000_000_000 { ms * 1000 } else { ms };
                chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
                    .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            }),
            _ => None,
        });

    Some(RawInstance {
        launcher: "modrinth".to_string(),
        name,
        description: String::new(),
        dir,
        game_dir: root.join("profiles").join(&get_str(row, "path").unwrap_or_default()),
        mc_version: if mc.is_empty() { "?".to_string() } else { mc },
        loader,
        problem,
        memory_min_mb: None,
        memory_max_mb,
        jvm_args,
        width,
        height,
        fullscreen,
        last_played_at,
        icon_file,
        mods,
        worlds,
        cf_index: None,
    })
}

fn modrinth_from_profile_json(dir: &Path) -> Option<RawInstance> {
    let json = read_json(&dir.join("profile.json"))?;
    let md = json.get("metadata")?.as_object()?;
    let loader_type = md
        .get("loader")
        .and_then(Value::as_str)
        .and_then(as_loader_type)?;
    let mc = md.get("game_version").and_then(Value::as_str).unwrap_or("").to_string();
    let loader_version = md
        .get("loader_version")
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| bare_loader_version(loader_type, &mc, value));
    let loader = Loader {
        r#type: loader_type.to_string(),
        version: if loader_type == "vanilla" { None } else { loader_version },
    };
    let mut memory_max_mb = None;
    let mut width = None;
    let mut height = None;
    let mut fullscreen = None;
    if let Some(mem) = json.get("memory").and_then(|v| v.get("maximum")) {
        memory_max_mb = memory_or_null(mem.as_u64());
    }
    if let Some(res) = json.get("resolution").and_then(Value::as_array) {
        let w = res.first().and_then(Value::as_u64);
        let h = res.get(1).and_then(Value::as_u64);
        if let (Some(w), Some(h)) = (w, h) {
            if (320..=7680).contains(&w) && (240..=4320).contains(&h) {
                width = Some(w as u32);
                height = Some(h as u32);
            }
        }
    }
    if json.get("fullscreen").and_then(Value::as_bool) == Some(true) {
        fullscreen = Some(true);
    }
    let name = md
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| clean_text(value, 48))
        .filter(|value| !value.is_empty())
        .or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "instance".to_string());
    let (mods, worlds) = count_content(dir);
    let problem = if mc.is_empty() {
        Some("Не указана версия Minecraft".to_string())
    } else {
        check_importable(&mc, &loader)
    };
    Some(RawInstance {
        launcher: "modrinth".to_string(),
        name,
        description: String::new(),
        dir: dir.to_path_buf(),
        game_dir: dir.to_path_buf(),
        mc_version: if mc.is_empty() { "?".to_string() } else { mc },
        loader,
        problem,
        memory_min_mb: None,
        memory_max_mb,
        jvm_args: String::new(),
        width,
        height,
        fullscreen,
        last_played_at: None,
        icon_file: None,
        mods,
        worlds,
        cf_index: None,
    })
}

fn scan_modrinth_root(root: &Path) -> Vec<RawInstance> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let db_file = root.join("app.db");
    if db_file.exists() {
        if let Some(rows) = read_modrinth_db(&db_file) {
            for row in rows {
                if let Some(instance) = modrinth_from_row(root, &row) {
                    seen.insert(normalize_path(&instance.dir));
                    out.push(instance);
                }
            }
        }
    }
    for name in list_dirs(&root.join("profiles")) {
        let dir = root.join("profiles").join(&name);
        if seen.contains(&normalize_path(&dir)) {
            continue;
        }
        if let Some(instance) = modrinth_from_profile_json(&dir) {
            out.push(instance);
        }
    }
    out.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
    out
}

fn heap_args_from_java(java_args: &str) -> (Option<u32>, Option<u32>, String) {
    let max = heap_mb(java_args, 'x');
    let min = heap_mb(java_args, 's');
    let jvm_args = strip_heap_args(java_args);
    (min, max, jvm_args)
}

fn scan_official_root(root: &Path) -> Vec<RawInstance> {
    let Some(data) = read_json(&root.join("launcher_profiles.json")) else {
        return Vec::new();
    };
    let profiles = data
        .get("profiles")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    // newest installed release for "latest-release" profiles
    let mut newest: Option<String> = None;
    for name in list_dirs(&root.join("versions")) {
        if name.chars().filter(|c| *c == '.').count() >= 1
            && name.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
            && name.chars().all(|c| c.is_ascii_digit() || c == '.')
        {
            if newest.as_ref().map(|current| versions::compare_versions(&name, current) > 0).unwrap_or(true) {
                newest = Some(name);
            }
        }
    }

    let mut entries: Vec<(String, Value)> = profiles.into_iter().collect();
    entries.sort_by(|a, b| {
        let a_used = a.1.get("lastUsed").and_then(Value::as_str).unwrap_or("");
        let b_used = b.1.get("lastUsed").and_then(Value::as_str).unwrap_or("");
        b_used.cmp(a_used)
    });

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (_, profile) in entries {
        let profile_type = profile.get("type").and_then(Value::as_str).unwrap_or("custom");
        let mut id = profile.get("lastVersionId").and_then(Value::as_str).unwrap_or("").to_string();
        if profile_type == "latest-snapshot" {
            continue;
        }
        if profile_type == "latest-release" || id == "latest-release" {
            let Some(latest) = newest.clone() else { continue };
            id = latest;
        }
        if id.is_empty() {
            continue;
        }
        let custom_dir = profile
            .get("gameDir")
            .and_then(Value::as_str)
            .filter(|value| Path::new(value).is_absolute())
            .map(PathBuf::from)
            .filter(|path| path.exists());
        let game_dir = custom_dir.unwrap_or_else(|| root.to_path_buf());
        let key = format!("{}|{id}", game_dir.to_string_lossy().to_ascii_lowercase());
        if !seen.insert(key) {
            continue;
        }
        let parsed = parse_version_id(&id);
        let (mc_version, loader, problem) = match parsed {
            Ok((mc, loader)) => (mc, loader, None),
            Err(problem) => ("?".to_string(), Loader { r#type: "vanilla".to_string(), version: None }, Some(problem)),
        };
        let java_args = profile.get("javaArgs").and_then(Value::as_str).unwrap_or("");
        let (mut memory_min_mb, memory_max_mb, jvm_args) = heap_args_from_java(java_args);
        if memory_min_mb.is_some() && memory_max_mb.is_some() && memory_min_mb > memory_max_mb {
            memory_min_mb = None;
        }
        let (mut width, mut height) = (None, None);
        if let Some(res) = profile.get("resolution") {
            let w = res.get("width").and_then(Value::as_u64);
            let h = res.get("height").and_then(Value::as_u64);
            if let (Some(w), Some(h)) = (w, h) {
                if (320..=7680).contains(&w) && (240..=4320).contains(&h) {
                    width = Some(w as u32);
                    height = Some(h as u32);
                }
            }
        }
        let label = profile
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(|value| clean_text(value, 48))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("Minecraft {}", if mc_version == "?" { &id } else { &mc_version }));
        let (mods, worlds) = count_content(&game_dir);
        let problem = problem.or_else(|| check_importable(&mc_version, &loader));
        out.push(RawInstance {
            launcher: "official".to_string(),
            name: label,
            description: String::new(),
            dir: game_dir.clone(),
            game_dir,
            mc_version,
            loader,
            problem,
            memory_min_mb,
            memory_max_mb,
            jvm_args,
            width,
            height,
            fullscreen: None,
            last_played_at: profile.get("lastUsed").and_then(Value::as_str).and_then(|s| to_iso(Some(s))),
            icon_file: None,
            mods,
            worlds,
            cf_index: None,
        });
    }
    out
}


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CfFile {
    pub file: String,
    pub file_id: i64,
}

#[derive(Debug, Clone)]
pub struct PendingImport {
    pub launcher: String,
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub game_dir: PathBuf,
    pub mc_version: String,
    pub loader: Loader,
    pub memory_min_mb: Option<u32>,
    pub memory_max_mb: Option<u32>,
    pub jvm_args: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fullscreen: Option<bool>,
    pub last_played_at: Option<String>,
    pub icon_file: Option<PathBuf>,
    pub cf_index: Option<std::collections::HashMap<String, CfFile>>,
}

fn candidate_roots(id: &str) -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let config = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
    let data = dirs::data_dir().unwrap_or(config.clone());
    let documents = dirs::document_dir().unwrap_or_else(|| home.join("Documents"));
    let mut roots = Vec::new();
    match id {
        "prism" => {
            roots.push(data.join("PrismLauncher"));
            if cfg!(target_os = "linux") {
                roots.push(home.join(".var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher"));
            }
        }
        "polymc" => {
            roots.push(data.join("PolyMC"));
            if cfg!(target_os = "linux") {
                roots.push(home.join(".var/app/org.polymc.PolyMC/data/PolyMC"));
            }
        }
        "multimc" => {
            roots.push(home.join("MultiMC"));
            roots.push(home.join("Desktop/MultiMC"));
            roots.push(home.join("Downloads/MultiMC"));
            roots.push(documents.join("MultiMC"));
            if cfg!(target_os = "windows") {
                roots.push(config.join("MultiMC"));
                roots.push(PathBuf::from("C:\\MultiMC"));
                roots.push(PathBuf::from("D:\\MultiMC"));
            } else {
                roots.push(data.join("multimc"));
            }
        }
        "modrinth" => {
            roots.push(data.join("ModrinthApp"));
            roots.push(data.join("com.modrinth.theseus"));
            if cfg!(target_os = "linux") {
                roots.push(home.join(".var/app/com.modrinth.ModrinthApp/data/ModrinthApp"));
            }
        }
        "curseforge" => {
            roots.push(home.join("curseforge/minecraft/Instances"));
            roots.push(documents.join("curseforge/minecraft/Instances"));
        }
        "official" => {
            if cfg!(target_os = "windows") {
                roots.push(dirs::data_dir().unwrap_or(config).join(".minecraft"));
            } else if cfg!(target_os = "macos") {
                roots.push(dirs::data_dir().unwrap_or(config).join("minecraft"));
            } else {
                roots.push(home.join(".minecraft"));
            }
        }
        _ => {}
    }
    roots
}

fn scan_root(id: &str, root: &Path) -> Vec<RawInstance> {
    match id {
        "prism" | "polymc" | "multimc" => scan_mmc_root(id, root),
        "modrinth" => scan_modrinth_root(root),
        "curseforge" => scan_curseforge_dir(root),
        "official" => scan_official_root(root),
        _ => Vec::new(),
    }
}

fn normalize_path(path: &Path) -> String {
    let value = path.to_string_lossy().to_string();
    if cfg!(target_os = "windows") {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

fn import_key(raw: &RawInstance) -> String {
    let base = format!("{}:{}", raw.launcher, normalize_path(&raw.dir));
    if raw.launcher == "official" {
        format!("{base}#{}-{}", raw.mc_version, raw.loader.r#type)
    } else {
        base
    }
}

fn random_token() -> String {
    format!("{:016x}", rand::random::<u64>())
}

fn to_pending(raw: &RawInstance) -> PendingImport {
    PendingImport {
        launcher: raw.launcher.clone(),
        name: raw.name.clone(),
        description: raw.description.clone(),
        dir: raw.dir.clone(),
        game_dir: raw.game_dir.clone(),
        mc_version: raw.mc_version.clone(),
        loader: raw.loader.clone(),
        memory_min_mb: raw.memory_min_mb,
        memory_max_mb: raw.memory_max_mb,
        jvm_args: raw.jvm_args.clone(),
        width: raw.width,
        height: raw.height,
        fullscreen: raw.fullscreen,
        last_played_at: raw.last_played_at.clone(),
        icon_file: raw.icon_file.clone(),
        cf_index: raw.cf_index.clone(),
    }
}

fn register(
    raws: Vec<RawInstance>,
    imported_keys: &HashSet<String>,
    pending: &mut std::collections::HashMap<String, PendingImport>,
) -> Vec<ImportCandidate> {
    raws.into_iter()
        .map(|raw| {
            let token = random_token();
            let imported = imported_keys.contains(&import_key(&raw));
            let candidate = ImportCandidate {
                token: token.clone(),
                name: raw.name.clone(),
                mc_version: raw.mc_version.clone(),
                loader: raw.loader.clone(),
                mods: raw.mods,
                worlds: raw.worlds,
                imported,
                problem: raw.problem.clone(),
            };
            if raw.problem.is_none() {
                pending.insert(token, to_pending(&raw));
            }
            candidate
        })
        .collect()
}

fn imported_keys(data_dir: &Path, settings: &crate::settings::Settings) -> HashSet<String> {
    crate::instances::list(data_dir, settings, &std::collections::HashSet::new())
        .ok()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|view| view.meta.imported_from)
        .collect()
}

pub fn detect_launchers(
    data_dir: &Path,
    settings: &crate::settings::Settings,
    pending: &mut std::collections::HashMap<String, PendingImport>,
) -> Vec<LauncherSource> {
    let imported = imported_keys(data_dir, settings);
    let ids = ["prism", "multimc", "polymc", "modrinth", "curseforge", "official"];
    ids.iter()
        .map(|id| {
            let mut found_path = None;
            let mut instances = Vec::new();
            for root in candidate_roots(id) {
                if !root.exists() {
                    continue;
                }
                found_path.get_or_insert_with(|| root.to_string_lossy().to_string());
                let scanned = scan_root(id, &root);
                if !scanned.is_empty() {
                    found_path = Some(root.to_string_lossy().to_string());
                    instances = scanned;
                    break;
                }
            }
            LauncherSource {
                id: (*id).to_string(),
                name: launcher_name(id).to_string(),
                path: found_path.clone(),
                found: found_path.is_some(),
                candidates: register(instances, &imported, pending),
            }
        })
        .collect()
}

fn norm_key(path: &Path) -> String {
    normalize_path(path)
}

pub fn inspect_folder(
    dir: &Path,
    data_dir: &Path,
    settings: &crate::settings::Settings,
    pending: &mut std::collections::HashMap<String, PendingImport>,
) -> LauncherSource {
    let imported = imported_keys(data_dir, settings);
    let mut out: Vec<RawInstance> = Vec::new();
    let mut add = |items: Vec<RawInstance>| {
        for item in items {
            let duplicate = out.iter().any(|existing| {
                norm_key(&existing.dir) == norm_key(&item.dir)
                    && existing.launcher == item.launcher
                    && existing.mc_version == item.mc_version
                    && existing.loader.r#type == item.loader.r#type
            });
            if !duplicate {
                out.push(item);
            }
        }
    };

    // a single instance
    let icons_dir = dir.parent().and_then(|p| p.parent()).map(|p| p.join("icons"));
    if let Some(single) = read_mmc_instance("prism", dir, icons_dir.as_deref())
        .or_else(|| read_cf_instance(dir))
        .or_else(|| modrinth_from_profile_json(dir))
    {
        out.push(single);
    } else {
        let mr_root = if dir.file_name().and_then(|n| n.to_str()).map(|n| n.eq_ignore_ascii_case("profiles")).unwrap_or(false)
            && dir.parent().map(|p| p.join("app.db").exists()).unwrap_or(false)
        {
            dir.parent().unwrap_or(dir).to_path_buf()
        } else {
            dir.to_path_buf()
        };
        if mr_root.join("app.db").exists() || dir.join("profiles").exists() {
            add(scan_modrinth_root(&mr_root));
        }
        if dir.join("launcher_profiles.json").exists() {
            add(scan_official_root(dir));
        }
        add(scan_mmc_root("prism", dir));
        for sub in ["instances", "Instances"] {
            add(scan_mmc_instances_dir("prism", &dir.join(sub), Some(&dir.join("icons"))));
            add(scan_curseforge_dir(&dir.join(sub)));
        }
        add(scan_mmc_instances_dir("prism", dir, dir.parent().map(|p| p.join("icons")).as_deref()));
        add(scan_curseforge_dir(dir));
    }

    LauncherSource {
        id: "custom".to_string(),
        name: dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| dir.to_string_lossy().to_string()),
        path: Some(dir.to_string_lossy().to_string()),
        found: true,
        candidates: register(out, &imported, pending),
    }
}

fn launcher_display_name(id: &str) -> &'static str {
    launcher_name(id)
}

fn normalize_for_key(path: &Path) -> String {
    normalize_path(path)
}

pub fn import_key_for(pending: &PendingImport) -> String {
    let base = format!("{}:{}", pending.launcher, normalize_for_key(&pending.dir));
    if pending.launcher == "official" {
        format!("{base}#{}-{}", pending.mc_version, pending.loader.r#type)
    } else {
        base
    }
}

fn cover_hash(name: &str) -> u32 {
    let mut hash: u32 = 0;
    for ch in name.chars() {
        hash = (hash.wrapping_mul(31).wrapping_add(ch as u32)) % 8;
    }
    hash
}

fn to_create_input(pending: &PendingImport) -> Value {
    let from = launcher_display_name(&pending.launcher);
    let desc = if pending.description.is_empty() {
        format!("Импортировано из {from}")
    } else {
        format!("{} ({from})", pending.description)
    };
    let desc: String = desc.chars().take(120).collect();
    let settings = json!({
        "javaPath": "",
        "memoryMinMb": pending.memory_min_mb,
        "memoryMaxMb": pending.memory_max_mb,
        "jvmArgs": pending.jvm_args,
        "width": pending.width,
        "height": pending.height,
        "fullscreen": pending.fullscreen,
    });
    json!({
        "name": if pending.name.is_empty() { "Imported".to_string() } else { pending.name.clone() },
        "description": desc,
        "mcVersion": pending.mc_version,
        "loader": {
            "type": pending.loader.r#type,
            "version": if pending.loader.r#type == "vanilla" { Value::Null } else { json!(pending.loader.version) },
        },
        "kind": if pending.loader.r#type == "vanilla" { "official" } else { "modded" },
        "coverColor": cover_hash(&pending.name),
        "settings": settings,
    })
}

const COPY_EXCLUDE: &[&str] = &[
    "logs", "crash-reports", ".fabric", ".mixin.out", "webcache", "webcache2", "cache", ".cache",
    "natives", "bin", "versions", "libraries", "assets", "runtime", "jarmods", "patches",
    "usercache.json", "usernamecache.json", "realms_persistence.json", "treatmenttags.json",
    "launcher_profiles.json", "launcher_accounts.json", "launcher_accounts_microsoft_store.json",
    "launcher_msa_credentials.bin", "launcher_log.txt", "launcher_settings.json",
    "launcher_ui_state.json", "launcher_gamer_pic.png", "launcher_entitlements.json",
    "launcher_cef_log.txt", "instance.cfg", "mmc-pack.json", "profile.json",
    "minecraftinstance.json", "manifest.json", "modlist.html", "profileimage.png",
    ".chald-modrinth.json", ".chald-curseforge.json", "app.db", "app.db-wal", "app.db-shm",
];
const MAX_FILES: usize = 400_000;

#[derive(Clone)]
struct CopyItem {
    rel: Vec<String>,
    size: u64,
}

fn is_excluded_top(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    COPY_EXCLUDE.iter().any(|item| *item == lower)
}

fn list_game_files(src: &Path, cancel: &AtomicBool) -> AppResult<Vec<CopyItem>> {
    let mut files = Vec::new();
    let mut stack: Vec<Vec<String>> = vec![Vec::new()];
    while let Some(rel) = stack.pop() {
        if cancel.load(Ordering::SeqCst) {
            return Err(AppError::new("IMPORT_CANCELLED", "Импорт отменён"));
        }
        let dir = if rel.is_empty() {
            src.to_path_buf()
        } else {
            let refs: Vec<&str> = rel.iter().map(String::as_str).collect();
            paths::resolve_inside(src, &refs)?
        };
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            if cancel.load(Ordering::SeqCst) {
                return Err(AppError::new("IMPORT_CANCELLED", "Импорт отменён"));
            }
            let file_type = match entry.file_type() {
                Ok(kind) => kind,
                Err(_) => continue,
            };
            let name = match entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => continue,
            };
            if rel.is_empty() && is_excluded_top(&name) {
                continue;
            }
            if file_type.is_symlink() {
                continue;
            }
            let mut child = rel.clone();
            child.push(name);
            if file_type.is_dir() {
                stack.push(child);
            } else if file_type.is_file() {
                let refs: Vec<&str> = child.iter().map(String::as_str).collect();
                let path = paths::resolve_inside(src, &refs)?;
                let size = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
                files.push(CopyItem { rel: child, size });
                if files.len() > MAX_FILES {
                    return Err(AppError::new("IMPORT_TOO_BIG", "В инстансе слишком много файлов для импорта"));
                }
            }
        }
    }
    Ok(files)
}

fn copy_game_files(
    src: &Path,
    dest: &Path,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(f64),
) -> AppResult<()> {
    if !src.exists() {
        return Ok(());
    }
    let files = list_game_files(src, cancel)?;
    let total = files.iter().map(|file| file.size).sum::<u64>().max(1);
    let mut done: u64 = 0;
    let mut last_emit = Instant::now();
    for item in files {
        if cancel.load(Ordering::SeqCst) {
            return Err(AppError::new("IMPORT_CANCELLED", "Импорт отменён"));
        }
        let refs: Vec<&str> = item.rel.iter().map(String::as_str).collect();
        let from = paths::resolve_inside(src, &refs)?;
        let to = paths::resolve_inside(dest, &refs)?;
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&from, &to)?;
        done = done.saturating_add(item.size);
        let fraction = (done as f64 / total as f64).min(1.0);
        if fraction >= 1.0 || last_emit.elapsed().as_millis() >= 120 {
            on_progress(fraction);
            last_emit = Instant::now();
        }
    }
    on_progress(1.0);
    Ok(())
}

fn is_abort(error: &AppError) -> bool {
    error.code == "IMPORT_CANCELLED" || error.code == "CANCELLED"
}

fn emit_import(
    app: &AppHandle,
    batch: &str,
    current_id: Option<&str>,
    label: &str,
    fraction: Option<f64>,
    status: &str,
    message: Option<String>,
) {
    game::emit(
        app,
        "op:progress",
        json!({
            "opId": format!("modpack:import-{batch}"),
            "kind": "modpack",
            "instanceId": current_id,
            "label": label,
            "fraction": fraction,
            "status": status,
            "message": message,
        }),
    );
}

pub async fn run_import_batch(
    app: &AppHandle,
    state: &AppState,
    _key: &str,
    batch: &str,
    items: Vec<PendingImport>,
    cancel: Arc<AtomicBool>,
) -> AppResult<Vec<String>> {
    emit_import(app, batch, None, "Подготовка импорта…", None, "running", None);
    let mut created_ids: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let total = items.len().max(1);

    for (index, pending) in items.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        let head = if items.len() > 1 {
            format!("Импорт {} из {}: «{}»", index + 1, items.len(), pending.name)
        } else {
            format!("Импорт «{}»", pending.name)
        };
        emit_import(app, batch, None, &format!("{head}…"), Some(index as f64 / total as f64), "running", None);

        let settings = state.settings.get()?;
        let running: std::collections::HashSet<String> = state.games.running_ids().into_iter().collect();
        let input = to_create_input(pending);
        let created = match instances::create(&state.data_dir, &settings, &running, &input) {
            Ok(view) => view,
            Err(error) => {
                failures.push(format!("«{}»: {}", pending.name, error.message));
                continue;
            }
        };
        let instance_id = created.meta.id.clone();
        let game_dir = match instances::game_dir_of(&state.data_dir, &settings, &instance_id) {
            Ok(dir) => dir,
            Err(error) => {
                let _ = instances::remove(&state.data_dir, &settings, &json!({ "id": instance_id, "confirm": true }));
                failures.push(format!("«{}»: {}", pending.name, error.message));
                continue;
            }
        };

        let src = pending.game_dir.clone();
        let dest = game_dir.clone();
        let cancel_copy = cancel.clone();
        let app_copy = app.clone();
        let batch_copy = batch.to_string();
        let head_copy = head.clone();
        let base = index as f64 / total as f64;
        let copy_result = tauri::async_runtime::spawn_blocking(move || {
            copy_game_files(&src, &dest, cancel_copy.as_ref(), |fraction| {
                let overall = base + fraction / total as f64;
                emit_import(
                    &app_copy,
                    &batch_copy,
                    None,
                    &format!("{head_copy}: копирую файлы"),
                    Some(overall.min(1.0)),
                    "running",
                    None,
                );
            })
        })
        .await
        .unwrap_or_else(|_| Err(AppError::new("IMPORT_FAILED", "Импорт прерван")));

        if let Err(error) = copy_result {
            let _ = instances::remove(&state.data_dir, &settings, &json!({ "id": instance_id, "confirm": true }));
            if is_abort(&error) || cancel.load(Ordering::SeqCst) {
                break;
            }
            failures.push(format!("«{}»: {}", pending.name, error.message));
            continue;
        }

        if let Some(index_map) = &pending.cf_index {
            let cf_path = game_dir.join(".chald-curseforge.json");
            let payload = json!({ "projects": index_map });
            if let Ok(bytes) = serde_json::to_vec_pretty(&payload) {
                let _ = fs::write(cf_path, bytes);
            }
        }
        if let Some(icon) = &pending.icon_file {
            let _ = instances::set_cover_from_file(&state.data_dir, &settings, &instance_id, icon);
        }
        let imported_from = import_key_for(pending);
        let _ = instances::patch_import_meta(
            &state.data_dir,
            &settings,
            &instance_id,
            &imported_from,
            pending.last_played_at.as_deref(),
        );

        created_ids.push(instance_id);
    }

    for (i, message) in failures.iter().enumerate() {
        game::emit(
            app,
            "op:progress",
            json!({
                "opId": format!("modpack:import-{batch}-err{i}"),
                "kind": "modpack",
                "instanceId": null,
                "label": "Не удалось импортировать",
                "fraction": null,
                "status": "error",
                "message": message,
            }),
        );
    }

    if !created_ids.is_empty() {
        let label = if created_ids.len() == 1 {
            "Инстанс импортирован. Устанавливаю Minecraft…".to_string()
        } else {
            format!("Импортировано инстансов: {}. Устанавливаю Minecraft…", created_ids.len())
        };
        let last = created_ids.last().cloned();
        emit_import(app, batch, last.as_deref(), &label, Some(1.0), "done", None);
    } else if cancel.load(Ordering::SeqCst) {
        emit_import(app, batch, None, "Импорт отменён", None, "cancelled", None);
    } else if failures.is_empty() {
        emit_import(app, batch, None, "Нечего импортировать", None, "cancelled", None);
    } else {
        emit_import(app, batch, None, "Импорт не удался", None, "cancelled", None);
    }

    Ok(created_ids)
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cfg_and_heap_args() {
        let cfg = parse_cfg("[General]\nname=Pack\nMaxMemAlloc=4096\nJvmArgs=-Xmx2G -XX:+UseG1GC\n");
        assert_eq!(cfg.get("name").map(String::as_str), Some("Pack"));
        assert_eq!(memory_or_null(to_int(cfg.get("MaxMemAlloc").map(String::as_str))), Some(4096));
        assert_eq!(strip_heap_args("-Xmx2G -Xms1G -XX:+UseG1GC"), "-XX:+UseG1GC");
        assert_eq!(heap_mb("-Xmx2048M -Xms512M", 'x'), Some(2048));
    }

    #[test]
    fn parses_mmc_pack_and_version_ids() {
        let pack = json!({
            "components": [
                { "uid": "net.minecraft", "version": "1.20.1" },
                { "uid": "net.fabricmc.fabric-loader", "version": "0.16.9" }
            ]
        });
        let (mc, loader) = parse_mmc_pack(&pack).unwrap();
        assert_eq!(mc, "1.20.1");
        assert_eq!(loader.r#type, "fabric");
        assert_eq!(loader.version.as_deref(), Some("0.16.9"));

        let (mc, loader) = parse_version_id("fabric-loader-0.15.7-1.20.1").unwrap();
        assert_eq!(mc, "1.20.1");
        assert_eq!(loader.r#type, "fabric");
        let (mc, loader) = parse_version_id("neoforge-20.4.237").unwrap();
        assert_eq!(mc, "1.20.4");
        assert_eq!(loader.r#type, "neoforge");
    }

    #[test]
    fn parses_curseforge_loader_and_importability() {
        let cf = json!({
            "gameVersion": "1.20.1",
            "baseModLoader": { "name": "forge-47.4.29", "type": 1 }
        });
        let (mc, loader) = parse_cf_loader(&cf).unwrap();
        assert_eq!(mc, "1.20.1");
        assert_eq!(loader.r#type, "forge");
        assert_eq!(loader.version.as_deref(), Some("47.4.29"));
        assert!(check_importable(&mc, &loader).is_none());
        assert!(check_importable("1.8.9", &Loader { r#type: "neoforge".into(), version: Some("x".into()) }).is_some());
    }
}
