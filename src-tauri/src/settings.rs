use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::errors::{AppError, AppResult};

pub const FRIENDS_SERVER: &str = "https://chaldserver.dynmap.xyz";

fn hex_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value.chars().skip(1).all(|c| c.is_ascii_hexdigit())
}

fn one_of(value: &str, allowed: &[&str]) -> bool {
    allowed.contains(&value)
}

/// The same defaults as `src/main/settings.ts` / `src/shared/theme.ts` / `src/shared/dns.ts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub instances_dir: String,
    pub downloads_dir: String,
    pub java_path: String,
    pub memory_min_mb: u32,
    pub memory_max_mb: u32,
    pub memory_auto: bool,
    pub jvm_args: String,
    pub width: u32,
    pub height: u32,
    pub fullscreen: bool,
    pub close_behavior: String,
    pub check_updates: bool,
    pub auto_download_updates: bool,
    pub auto_install_updates: bool,
    pub update_repo: String,
    pub ui_sounds: bool,
    pub sound_volume: u32,
    pub discord_rpc: bool,
    pub discord_show_instance: bool,
    pub theme_bg: String,
    pub theme_accent: String,
    pub theme_secondary: String,
    pub theme_highlight: String,
    pub ui_scale: u32,
    pub corner_style: String,
    pub glow: u32,
    pub font_mode: String,
    pub animations: bool,
    pub compact_sidebar: bool,
    pub hidden_pages: Vec<String>,
    pub sidebar_side: String,
    pub panel_side: String,
    pub panel_width: u32,
    pub show_status_bar: bool,
    pub custom_css: String,
    pub custom_html: String,
    pub dns_mode: String,
    pub dns_servers: String,
    pub dns_doh_url: String,
    pub ms_client_id: String,
    pub account_mode: String,
    pub offline_name: String,
    pub last_instance_id: Option<String>,
    pub language: String,
    pub launcher_import_seen: bool,
    pub friends_server_url: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            instances_dir: String::new(),
            downloads_dir: String::new(),
            java_path: String::new(),
            memory_min_mb: 1024,
            memory_max_mb: 4096,
            memory_auto: true,
            jvm_args: "-XX:+UseG1GC".to_string(),
            width: 1280,
            height: 720,
            fullscreen: false,
            close_behavior: "quit".to_string(),
            check_updates: true,
            auto_download_updates: true,
            auto_install_updates: true,
            update_repo: String::new(),
            ui_sounds: true,
            sound_volume: 50,
            discord_rpc: true,
            discord_show_instance: true,
            theme_bg: "#020d18".to_string(),
            theme_accent: "#00acff".to_string(),
            theme_secondary: "#289d85".to_string(),
            theme_highlight: "#00d5ff".to_string(),
            ui_scale: 100,
            corner_style: "normal".to_string(),
            glow: 100,
            font_mode: "pixel".to_string(),
            animations: true,
            compact_sidebar: false,
            hidden_pages: Vec::new(),
            sidebar_side: "left".to_string(),
            panel_side: "right".to_string(),
            panel_width: 380,
            show_status_bar: true,
            custom_css: String::new(),
            custom_html: String::new(),
            dns_mode: "system".to_string(),
            dns_servers: "1.1.1.1, 8.8.8.8".to_string(),
            dns_doh_url: "https://cloudflare-dns.com/dns-query".to_string(),
            ms_client_id: String::new(),
            account_mode: "microsoft".to_string(),
            offline_name: String::new(),
            last_instance_id: None,
            language: "ru".to_string(),
            launcher_import_seen: false,
            friends_server_url: FRIENDS_SERVER.to_string(),
        }
    }
}

/// Keys the UI may change. Everything else is rejected, like the `.strict()` zod patch schema.
const PATCH_KEYS: &[&str] = &[
    "instancesDir",
    "downloadsDir",
    "javaPath",
    "memoryMinMb",
    "memoryMaxMb",
    "memoryAuto",
    "jvmArgs",
    "width",
    "height",
    "fullscreen",
    "closeBehavior",
    "checkUpdates",
    "autoDownloadUpdates",
    "autoInstallUpdates",
    "updateRepo",
    "uiSounds",
    "soundVolume",
    "discordRpc",
    "discordShowInstance",
    "themeBg",
    "themeAccent",
    "themeSecondary",
    "themeHighlight",
    "uiScale",
    "cornerStyle",
    "glow",
    "fontMode",
    "animations",
    "compactSidebar",
    "hiddenPages",
    "sidebarSide",
    "panelSide",
    "panelWidth",
    "showStatusBar",
    "customCss",
    "customHtml",
    "dnsMode",
    "dnsServers",
    "dnsDohUrl",
    "msClientId",
    "accountMode",
    "offlineName",
    "lastInstanceId",
    "language",
    "launcherImportSeen",
    "friendsServerUrl",
];

impl Settings {
    fn validate(&self) -> AppResult<()> {
        let bad = |message: &str| Err(AppError::new("SETTINGS_INVALID", message));

        if self.memory_min_mb < 256 || self.memory_min_mb > 65_536 {
            return bad("Минимум памяти: от 256 до 65536 МБ");
        }
        if self.memory_max_mb < 256 || self.memory_max_mb > 65_536 {
            return bad("Максимум памяти: от 256 до 65536 МБ");
        }
        if self.memory_min_mb > self.memory_max_mb {
            return bad("Минимум памяти не может быть больше максимума");
        }
        if !(320..=7680).contains(&self.width) || !(240..=4320).contains(&self.height) {
            return bad("Некорректный размер окна");
        }
        if self.sound_volume > 100 {
            return bad("Громкость: 0–100");
        }
        if !(80..=130).contains(&self.ui_scale) {
            return bad("Масштаб интерфейса: 80–130%");
        }
        if self.glow > 100 {
            return bad("Свечение: 0–100");
        }
        if !(260..=560).contains(&self.panel_width) {
            return bad("Ширина панели: 260–560 пикселей");
        }
        if !one_of(&self.close_behavior, &["quit", "minimize-while-playing"]) {
            return bad("Некорректное поведение при закрытии");
        }
        if !one_of(&self.corner_style, &["sharp", "normal", "round"]) {
            return bad("Некорректный стиль углов");
        }
        if !one_of(&self.font_mode, &["pixel", "clean"]) {
            return bad("Некорректный шрифт");
        }
        if !one_of(&self.sidebar_side, &["left", "right"]) {
            return bad("Некорректная сторона меню");
        }
        if !one_of(&self.panel_side, &["right", "left", "bottom", "hidden"]) {
            return bad("Некорректная сторона панели");
        }
        if !one_of(&self.dns_mode, &["system", "plain", "doh"]) {
            return bad("Некорректный режим DNS");
        }
        if !one_of(&self.account_mode, &["microsoft", "offline"]) {
            return bad("Некорректный режим аккаунта");
        }
        if !one_of(&self.language, &["en", "ru", "kk", "be"]) {
            return bad("Некорректный язык");
        }
        for (name, value) in [
            ("themeBg", &self.theme_bg),
            ("themeAccent", &self.theme_accent),
            ("themeSecondary", &self.theme_secondary),
            ("themeHighlight", &self.theme_highlight),
        ] {
            if !hex_color(value) {
                return Err(AppError::new("SETTINGS_INVALID", format!("{name}: нужен цвет #rrggbb")));
            }
        }
        for (name, value) in [
            ("instancesDir", &self.instances_dir),
            ("downloadsDir", &self.downloads_dir),
            ("javaPath", &self.java_path),
        ] {
            if !value.trim().is_empty() && !Path::new(value).is_absolute() {
                return Err(AppError::new("SETTINGS_INVALID", format!("{name}: путь должен быть абсолютным")));
            }
        }
        if !self.update_repo.is_empty() {
            let ok = self
                .update_repo
                .split_once('/')
                .map(|(owner, repo)| {
                    !owner.is_empty()
                        && !repo.is_empty()
                        && !repo.contains('/')
                        && owner
                            .chars()
                            .chain(repo.chars())
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                })
                .unwrap_or(false);
            if !ok {
                return bad("Репозиторий обновлений: формат «владелец/репозиторий»");
            }
        }
        if let Some(id) = &self.last_instance_id {
            if id.len() > 64 {
                return bad("Некорректный ID последнего инстанса");
            }
        }
        if !self.offline_name.is_empty() && !crate::paths::is_offline_name(&self.offline_name) {
            return bad("Ник: 3–16 символов, только A-Z, a-z, 0-9 и «_»");
        }
        Ok(())
    }

    /// Applies a partial update (the same shape the renderer sends to `settings:set`).
    pub fn apply_patch(&mut self, patch: &Value) -> AppResult<()> {
        let object = patch
            .as_object()
            .ok_or_else(|| AppError::new("SETTINGS_INVALID", "Ожидался объект настроек"))?;
        for key in object.keys() {
            if !PATCH_KEYS.contains(&key.as_str()) {
                return Err(AppError::new("SETTINGS_INVALID", format!("Неизвестная настройка: {key}")));
            }
        }
        let mut current = serde_json::to_value(&*self)?;
        let map: &mut Map<String, Value> = current
            .as_object_mut()
            .ok_or_else(|| AppError::new("SETTINGS_INVALID", "Некорректные настройки"))?;
        for (key, value) in object {
            map.insert(key.clone(), value.clone());
        }
        let mut next: Settings = serde_json::from_value(current)
            .map_err(|e| AppError::new("SETTINGS_INVALID", format!("Некорректные настройки: {e}")))?;
        next.friends_server_url = FRIENDS_SERVER.to_string();
        next.validate()?;
        *self = next;
        Ok(())
    }
}

/// Cached `settings.json` with atomic writes (like `src/main/settings.ts` + `storage.ts`).
pub struct SettingsStore {
    file: PathBuf,
    cache: Mutex<Option<Settings>>,
    write_lock: Mutex<()>,
}

impl SettingsStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            file: data_dir.join("settings.json"),
            cache: Mutex::new(None),
            write_lock: Mutex::new(()),
        }
    }

    fn load(&self) -> Settings {
        let stored: Value = fs::read(&self.file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null);
        let defaults = serde_json::to_value(Settings::default()).unwrap_or(Value::Null);
        let merged = match (defaults, stored) {
            (Value::Object(mut base), Value::Object(extra)) => {
                for (key, value) in extra {
                    base.insert(key, value);
                }
                Value::Object(base)
            }
            (defaults, _) => defaults,
        };
        let mut settings: Settings = serde_json::from_value(merged).unwrap_or_default();
        settings.friends_server_url = FRIENDS_SERVER.to_string();
        if settings.validate().is_err() {
            settings = Settings::default();
        }
        settings
    }

    pub fn get(&self) -> AppResult<Settings> {
        let mut guard = self.cache.lock().unwrap();
        if let Some(settings) = guard.as_ref() {
            return Ok(settings.clone());
        }
        let settings = self.load();
        *guard = Some(settings.clone());
        Ok(settings)
    }

    pub fn set(&self, patch: &Value) -> AppResult<Settings> {
        let _write = self.write_lock.lock().unwrap();
        let mut settings = self.get()?;
        settings.apply_patch(patch)?;
        self.save(&settings)?;
        *self.cache.lock().unwrap() = Some(settings.clone());
        Ok(settings)
    }

    fn save(&self, settings: &Settings) -> AppResult<()> {
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(settings)?;
        let tmp = self.file.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &self.file)?;
        Ok(())
    }
}

/// The same payload `auth:state` returns (`AuthState` in `src/shared/types.ts`).
#[allow(dead_code)]
pub fn auth_state(settings: &Settings) -> Value {
    let offline = settings.account_mode == "offline";
    let name_ok = offline && crate::paths::is_offline_name(&settings.offline_name);
    json!({
        "mode": if offline { "offline" } else { "microsoft" },
        "loggedIn": name_ok,
        "name": if name_ok { Value::String(settings.offline_name.clone()) } else { Value::Null },
        "uuid": if name_ok {
            Value::String(crate::offline::uuid(&settings.offline_name))
        } else {
            Value::Null
        },
        "configured": false,
        "encryptionAvailable": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_locked_to_the_friends_server() {
        let settings = Settings::default();
        settings.validate().unwrap();
        assert_eq!(settings.friends_server_url, FRIENDS_SERVER);
        assert_eq!(settings.language, "ru");
        assert_eq!(settings.theme_bg, "#020d18");
    }

    #[test]
    fn patch_merges_and_validates() {
        let mut settings = Settings::default();
        settings.apply_patch(&json!({ "memoryMaxMb": 8192, "uiSounds": false })).unwrap();
        assert_eq!(settings.memory_max_mb, 8192);
        assert!(!settings.ui_sounds);
        assert!(settings.apply_patch(&json!({ "nope": 1 })).is_err());
        assert!(settings.apply_patch(&json!({ "soundVolume": 101 })).is_err());
        assert!(settings.apply_patch(&json!({ "memoryMinMb": 9000, "memoryMaxMb": 1000 })).is_err());
        assert!(settings.apply_patch(&json!({ "instancesDir": "relative/path" })).is_err());
    }

    #[test]
    fn the_friends_server_cannot_be_changed() {
        let mut settings = Settings::default();
        settings.apply_patch(&json!({ "friendsServerUrl": "https://evil.example" })).unwrap();
        assert_eq!(settings.friends_server_url, FRIENDS_SERVER);
    }

    #[test]
    fn settings_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path());
        store.set(&json!({ "memoryMaxMb": 6000, "offlineName": "Steve" })).unwrap();
        let again = SettingsStore::new(dir.path());
        let loaded = again.get().unwrap();
        assert_eq!(loaded.memory_max_mb, 6000);
        assert_eq!(loaded.offline_name, "Steve");
        // a corrupted file falls back to defaults instead of crashing
        fs::write(dir.path().join("settings.json"), "{not json").unwrap();
        let broken = SettingsStore::new(dir.path());
        assert_eq!(broken.get().unwrap().memory_max_mb, 4096);
    }

    #[test]
    fn auth_state_offline() {
        let mut settings = Settings::default();
        settings.account_mode = "offline".to_string();
        settings.offline_name = "Steve".to_string();
        let state = auth_state(&settings);
        assert_eq!(state["mode"], "offline");
        assert_eq!(state["loggedIn"], true);
        assert_eq!(state["name"], "Steve");
        assert_eq!(state["uuid"].as_str().unwrap().len(), 36);
    }
}
