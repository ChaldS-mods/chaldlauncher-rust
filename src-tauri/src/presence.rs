//! Статус в Discord: что игрок делает в лаунчере. Порт `src/main/presence.ts`.

use std::sync::Mutex;

use serde_json::{json, Value};

use crate::config::DEFAULT_DISCORD_CLIENT_ID;
use crate::discord::DiscordRpc;
use crate::errors::{AppError, AppResult};

pub const PAGES: &[&str] = &[
    "home", "instances", "versions", "mods", "resourcepacks", "wardrobe", "friends", "ai", "modrinth", "console", "mojang", "settings",
];

/// Имя картинки, загруженной в приложение Discord (Rich Presence → Art Assets).
const LOGO_ASSET: &str = "logo";
const DOWNLOAD_URL: &str = "https://github.com/ChaldS-mods/ChaldLauncher/releases/latest";

fn page_label(lang: &str, page: &str) -> &'static str {
    match (lang, page) {
        ("en", "versions") => "Browsing Minecraft versions",
        ("en", "mods") => "Managing mods",
        ("en", "resourcepacks") => "Managing resource packs",
        ("en", "wardrobe") => "In the wardrobe",
        ("en", "friends") => "With friends",
        ("en", "ai") => "Chatting with Chaldi",
        ("en", "modrinth") => "Browsing mods (CF+MR)",
        ("en", "console") => "Viewing the console",
        ("en", "mojang") => "Browsing other Mojang games",
        ("en", "settings") => "In settings",
        ("en", _) => "In the main menu",

        ("kk", "versions") => "Minecraft нұсқасын таңдауда",
        ("kk", "mods") => "Модтарды баптауда",
        ("kk", "resourcepacks") => "Ресурс пакеттерін баптауда",
        ("kk", "wardrobe") => "Гардеробта",
        ("kk", "friends") => "Достарда",
        ("kk", "ai") => "Чалдимен сөйлесуде",
        ("kk", "modrinth") => "Мод іздеуде CF+MR",
        ("kk", "console") => "Консольді қарауда",
        ("kk", "mojang") => "Mojang-тың басқа ойындарын қарауда",
        ("kk", "settings") => "Баптауларда",
        ("kk", _) => "Негізгі мәзірде",

        ("be", "versions") => "Выбірае версію Minecraft",
        ("be", "mods") => "Наладжвае моды",
        ("be", "resourcepacks") => "Наладжвае рэсурспакі",
        ("be", "wardrobe") => "У гардэробе",
        ("be", "friends") => "У сябрах",
        ("be", "ai") => "Размаўляе з Чалди",
        ("be", "modrinth") => "Шукае моды CF+MR",
        ("be", "console") => "Глядзіць кансоль",
        ("be", "mojang") => "Праглядае іншыя гульні Mojang",
        ("be", "settings") => "У наладах",
        ("be", _) => "У галоўным меню",

        (_, "versions") => "Выбирает версию Minecraft",
        (_, "mods") => "Настраивает моды",
        (_, "resourcepacks") => "Настраивает ресурспаки",
        (_, "wardrobe") => "В гардеробе",
        (_, "friends") => "В друзьях",
        (_, "ai") => "Общается с Чалди",
        (_, "modrinth") => "Ищет моды CF+MR",
        (_, "console") => "Смотрит консоль",
        (_, "mojang") => "Смотрит другие игры Mojang",
        (_, "settings") => "В настройках",
        (_, _) => "В главном меню",
    }
}

fn playing_text(lang: &str, name: Option<&str>) -> String {
    match (lang, name) {
        ("en", Some(n)) => format!("Playing “{n}”"),
        ("en", None) => "Playing Minecraft".to_string(),
        ("kk", Some(n)) => format!("«{n}» ойнауда"),
        ("kk", None) => "Minecraft ойнауда".to_string(),
        ("be", Some(n)) => format!("Гуляе ў «{n}»"),
        ("be", None) => "Гуляе ў Minecraft".to_string(),
        (_, Some(n)) => format!("Играет в «{n}»"),
        (_, None) => "Играет в Minecraft".to_string(),
    }
}

fn download_label(lang: &str) -> &'static str {
    match lang {
        "en" => "Download launcher",
        "kk" => "Лаунчерді жүктеу",
        "be" => "Спампаваць лаўнчар",
        _ => "Скачать лаунчер",
    }
}

fn loader_name(loader: &str) -> Option<&'static str> {
    match loader {
        "fabric" => Some("Fabric"),
        "quilt" => Some("Quilt"),
        "forge" => Some("Forge"),
        "neoforge" => Some("NeoForge"),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct Playing {
    pub id: String,
    pub name: String,
    pub mc_version: String,
    pub loader: String,
    /// мс с начала эпохи
    pub since_ms: i64,
}

pub fn build_activity(page: &str, playing: Option<&Playing>, show_instance: bool, app_version: &str, started_ms: i64, lang: &str) -> Value {
    let lang = if ["en", "ru", "kk", "be"].contains(&lang) { lang } else { "ru" };
    let assets = json!({ "large_image": LOGO_ASSET, "large_text": format!("Chald Launcher {app_version}") });
    let buttons = json!([{ "label": download_label(lang), "url": DOWNLOAD_URL }]);
    if let Some(p) = playing {
        let loader = loader_name(&p.loader).map(|n| format!(" · {n}")).unwrap_or_default();
        return json!({
            "details": playing_text(lang, if show_instance { Some(p.name.as_str()) } else { None }),
            "state": format!("Minecraft {}{}", p.mc_version, loader),
            "timestamps": { "start": p.since_ms / 1000 },
            "assets": assets,
            "buttons": buttons,
        });
    }
    json!({
        "details": page_label(lang, page),
        "state": format!("Chald Launcher {app_version}"),
        "timestamps": { "start": started_ms / 1000 },
        "assets": assets,
        "buttons": buttons,
    })
}

struct Inner {
    rpc: Option<DiscordRpc>,
    enabled: bool,
    show_instance: bool,
    language: String,
    page: String,
    playing: Vec<Playing>,
}

pub struct PresenceService {
    inner: Mutex<Inner>,
    started_ms: i64,
}

impl PresenceService {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner { rpc: None, enabled: false, show_instance: true, language: "ru".into(), page: "instances".into(), playing: Vec::new() }),
            started_ms: chrono::Utc::now().timestamp_millis(),
        }
    }

    fn push(&self, inner: &Inner) {
        if !inner.enabled {
            return;
        }
        if let Some(rpc) = &inner.rpc {
            rpc.set_activity(Some(build_activity(
                &inner.page,
                inner.playing.last(),
                inner.show_instance,
                env!("CARGO_PKG_VERSION"),
                self.started_ms,
                &inner.language,
            )));
        }
    }

    pub fn configure(&self, enabled: bool, show_instance: bool, language: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.show_instance = show_instance;
        if ["en", "ru", "kk", "be"].contains(&language) {
            inner.language = language.to_string();
        }
        let on = enabled && !DEFAULT_DISCORD_CLIENT_ID.is_empty();
        if on && inner.rpc.is_none() {
            inner.rpc = Some(DiscordRpc::start(DEFAULT_DISCORD_CLIENT_ID.to_string(), std::process::id()));
        } else if !on {
            if let Some(rpc) = inner.rpc.take() {
                rpc.stop();
            }
        }
        inner.enabled = on;
        self.push(&inner);
    }

    pub fn set_page(&self, page: &str) -> AppResult<()> {
        if !PAGES.contains(&page) {
            return Err(AppError::new("INVALID_ARGS", "Неизвестная страница"));
        }
        let mut inner = self.inner.lock().unwrap();
        inner.page = page.to_string();
        self.push(&inner);
        Ok(())
    }

    pub fn game_started(&self, info: Playing) {
        let mut inner = self.inner.lock().unwrap();
        inner.playing.retain(|p| p.id != info.id);
        inner.playing.push(info);
        self.push(&inner);
    }

    pub fn game_stopped(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.playing.iter().any(|p| p.id == id) {
            return;
        }
        inner.playing.retain(|p| p.id != id);
        self.push(&inner);
    }

    pub fn dispose(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(rpc) = inner.rpc.take() {
            rpc.stop();
        }
        inner.enabled = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_and_game_activity() {
        let menu = build_activity("wardrobe", None, true, "1.1.0", 5_000, "ru");
        assert_eq!(menu["details"], "В гардеробе");
        assert_eq!(menu["state"], "Chald Launcher 1.1.0");
        assert_eq!(menu["timestamps"]["start"], 5);

        let game = Playing { id: "a".into(), name: "Моя сборка".into(), mc_version: "1.20.1".into(), loader: "fabric".into(), since_ms: 9_000 };
        let shown = build_activity("home", Some(&game), true, "1.1.0", 0, "ru");
        assert_eq!(shown["details"], "Играет в «Моя сборка»");
        assert_eq!(shown["state"], "Minecraft 1.20.1 · Fabric");
        let hidden = build_activity("home", Some(&game), false, "1.1.0", 0, "en");
        assert_eq!(hidden["details"], "Playing Minecraft");
        let vanilla = Playing { loader: "vanilla".into(), ..game };
        assert_eq!(build_activity("home", Some(&vanilla), true, "1.1.0", 0, "xx")["state"], "Minecraft 1.20.1");
    }

    #[test]
    fn page_names_are_validated() {
        let service = PresenceService::new();
        assert!(service.set_page("friends").is_ok());
        assert!(service.set_page("../etc").is_err());
    }
}
