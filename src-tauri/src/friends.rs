//! Клиент сервера друзей (порт `src/main/friends.ts`). Токен хранится в `friends-token.txt`, как и в Electron-версии.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::{json, Value};

use crate::errors::{AppError, AppResult};
use crate::net;

const SERVER_URL: &str = "https://chaldserver.dynmap.xyz";
const API: &str = "/chaldfriends/v1";
const HEARTBEAT_SECS: u64 = 45;

/// Данные аккаунта лаунчера, нужные серверу друзей.
#[derive(Debug, Clone)]
pub struct Account {
    pub mode: String,
    pub logged_in: bool,
    pub uuid: Option<String>,
    pub name: Option<String>,
}

impl Account {
    pub fn from_state(state: &Value) -> Self {
        let text = |key: &str| state.get(key).and_then(Value::as_str).map(str::to_string);
        Self {
            mode: text("mode").unwrap_or_default(),
            logged_in: state.get("loggedIn").and_then(Value::as_bool).unwrap_or(false),
            uuid: text("uuid"),
            name: text("name"),
        }
    }

    fn identity(&self) -> AppResult<(String, String)> {
        match (self.logged_in, &self.uuid, &self.name) {
            (true, Some(uuid), Some(name)) => Ok((uuid.clone(), name.clone())),
            _ => Err(AppError::new(
                "FRIENDS_AUTH",
                if self.mode == "offline" {
                    "Укажи ник в настройках аккаунта (режим Offline), затем зарегистрируй Chald ID."
                } else {
                    "Войди через Microsoft или включи offline-ник, чтобы зарегистрировать Chald ID."
                },
            )),
        }
    }
}

pub struct FriendsService {
    dir: PathBuf,
    token: Mutex<Option<String>>,
    /// Поколение таймера присутствия: смена номера останавливает старый таймер.
    generation: Arc<AtomicU64>,
    presence: Arc<Mutex<(String, String)>>,
}

pub fn is_chald_login(value: &str) -> bool {
    (3..=20).contains(&value.chars().count()) && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl FriendsService {
    pub fn new(data_dir: &Path) -> Self {
        let token_file = data_dir.join("friends-token.txt");
        let mut token = std::fs::read_to_string(&token_file).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        // Аккаунты до схемы «логин = Chald ID = ник» один раз сбрасываем: нужно зарегистрироваться заново.
        let marker = data_dir.join("friends-account.v2");
        if !marker.exists() {
            if token.is_some() {
                token = None;
                let _ = std::fs::write(&token_file, "");
                let _ = std::fs::write(data_dir.join("friends-legacy-reset.flag"), "1");
            }
            let _ = std::fs::create_dir_all(data_dir);
            let _ = std::fs::write(&marker, "2");
        }
        Self {
            dir: data_dir.to_path_buf(),
            token: Mutex::new(token),
            generation: Arc::new(AtomicU64::new(0)),
            presence: Arc::new(Mutex::new(("launcher".to_string(), String::new()))),
        }
    }

    pub fn has_token(&self) -> bool {
        self.token.lock().unwrap().is_some()
    }

    /// The friends token is the only identity the AI API trusts. Never fall back to another account or a local key.
    pub fn auth_token(&self) -> AppResult<String> {
        self.token()
            .ok_or_else(|| AppError::new("FRIENDS_AUTH", "Сначала войди в Chald ID — лимит Чалди привязан к аккаунту."))
    }

    fn token(&self) -> Option<String> {
        self.token.lock().unwrap().clone()
    }

    fn set_token(&self, token: Option<&str>) {
        *self.token.lock().unwrap() = token.map(str::to_string);
        let _ = std::fs::write(self.dir.join("friends-token.txt"), token.unwrap_or(""));
    }

    pub fn consume_legacy_reset(&self) -> bool {
        let flag = self.dir.join("friends-legacy-reset.flag");
        if !flag.exists() {
            return false;
        }
        std::fs::remove_file(flag).is_ok()
    }

    pub fn clear_session(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.set_token(None);
    }

    /// Запоминает активность и сразу отправляет её серверу (дальше её шлёт таймер).
    pub fn set_presence(&self, status: &str, activity: &str) {
        *self.presence.lock().unwrap() = (status.to_string(), activity.to_string());
        let Some(token) = self.token() else { return };
        let body = json!({ "status": status, "activity": activity.chars().take(64).collect::<String>() });
        tauri::async_runtime::spawn(async move {
            let _ = net::request_json_raw("POST", &format!("{SERVER_URL}{API}/presence/heartbeat"), Some(&body), Some(&token), "Chald Friends: heartbeat").await;
        });
    }

    /// Запускает периодическую отправку присутствия (раз в 45 с), пока жив токен.
    pub fn start_heartbeat(&self) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let Some(token) = self.token() else { return };
        let counter = self.generation.clone();
        let presence = self.presence.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                if counter.load(Ordering::SeqCst) != generation {
                    return;
                }
                let (status, activity) = presence.lock().unwrap().clone();
                let body = json!({ "status": status, "activity": activity.chars().take(64).collect::<String>() });
                let _ = net::request_json_raw("POST", &format!("{SERVER_URL}{API}/presence/heartbeat"), Some(&body), Some(&token), "Chald Friends: heartbeat").await;
                tokio::time::sleep(std::time::Duration::from_secs(HEARTBEAT_SECS)).await;
            }
        });
    }

    async fn request(&self, method: &str, path: &str, body: Option<Value>, auth: bool) -> AppResult<Value> {
        let bearer = if auth {
            Some(self.token().ok_or_else(|| AppError::new("FRIENDS_AUTH", "Сначала войди в Chald — нужен Chald ID."))?)
        } else {
            None
        };
        let url = format!("{SERVER_URL}{API}{path}");
        let (status, text) = net::request_json_raw(method, &url, body.as_ref(), bearer.as_deref(), &format!("Chald Friends: {path}")).await?;
        let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(data);
        }
        let detail = data
            .get("detail")
            .or_else(|| data.get("error"))
            .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
            .unwrap_or_else(|| text.chars().take(200).collect());
        // A rate-limited AI reply carries the remaining quota in its JSON body; surface it to the user.
        let mut detailed = detail;
        if status == 429 && data.get("limit").is_some() {
            let limit = data.get("limit").and_then(Value::as_u64).unwrap_or(0);
            let remaining = data.get("remaining").and_then(Value::as_u64).unwrap_or(0);
            let reset = data.get("resetInSec").and_then(Value::as_u64).unwrap_or(0);
            detailed = format!("{detailed} (лимит Чалди: {remaining}/{limit}, сброс через {reset} с)");
        }
        Err(match status {
            401 => AppError::new("FRIENDS_AUTH", "Сессия Chald истекла. Обнови вход."),
            404 => AppError::new("FRIENDS_NOT_FOUND", if detailed.is_empty() { "Не найдено".to_string() } else { detailed.clone() }),
            409 => AppError::new("FRIENDS_CONFLICT", if detailed.is_empty() { "Конфликт".to_string() } else { detailed.clone() }),
            s if s >= 500 => AppError::new("FRIENDS_DOWN", "Сервер друзей недоступен. Попробуй позже."),
            s => AppError::new("FRIENDS_HTTP", if detailed.is_empty() { format!("Ошибка сервера друзей (HTTP {s})") } else { detailed }),
        })
    }

    fn adopt(&self, data: &Value) {
        if let Some(token) = data.pointer("/user/token").and_then(Value::as_str) {
            self.set_token(Some(token));
        }
        self.start_heartbeat();
    }

    async fn avatar_data(&self, user_id: i64) -> Option<Value> {
        let url = format!("{SERVER_URL}{API}/avatars/{user_id}");
        let (_, bytes) = net::get_bytes(&url, "Chald Friends avatar", 512 * 1024).await.ok()?;
        if bytes.len() < 32 {
            return None;
        }
        Some(json!(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))))
    }

    /// Agent call: the server owns the model and charges THIS account once per user turn.
    pub async fn ai_chat(&self, messages: &Value, tools: Option<&Value>, turn_id: Option<&str>) -> AppResult<Value> {
        let mut body = json!({ "messages": messages });
        if let Some(tools) = tools {
            body["tools"] = tools.clone();
        }
        if let Some(turn_id) = turn_id.filter(|id| !id.is_empty()) {
            body["turnId"] = json!(turn_id);
        }
        self.request("POST", "/ai/chat", Some(body), true).await
    }

    pub async fn ai_status(&self) -> AppResult<Value> {
        self.request("GET", "/ai/status", None, true).await
    }

    /// Обрабатывает канал `friends:*`. `None` — канал не принадлежит друзьям.
    pub async fn handle(&self, channel: &str, arg: &Value, account: &Account) -> Option<AppResult<Value>> {
        if !channel.starts_with("friends:") {
            return None;
        }
        Some(self.route(channel, arg, account).await)
    }

    async fn route(&self, channel: &str, arg: &Value, account: &Account) -> AppResult<Value> {
        let int = |key: &str| -> AppResult<i64> {
            arg.get(key)
                .and_then(Value::as_i64)
                .filter(|v| *v > 0)
                .ok_or_else(|| AppError::new("INVALID_ARGS", format!("Некорректное поле {key}")))
        };
        let text = |key: &str| arg.get(key).and_then(Value::as_str);
        let list_of = |value: &Value, key: &str| -> Value {
            Value::Array(value.get(key).and_then(Value::as_array).map(|a| a.iter().filter(|v| !v.is_null()).cloned().collect()).unwrap_or_default())
        };
        let after = arg.get("afterId").and_then(Value::as_i64).unwrap_or(0).max(0);

        match channel {
            "friends:hasSession" => Ok(json!(self.has_token())),
            "friends:legacyReset" => Ok(json!(self.consume_legacy_reset())),
            "friends:logout" => {
                self.clear_session();
                Ok(json!(true))
            }
            "friends:session" => {
                let (uuid, name) = account.identity()?;
                let chald_id = text("chaldId").filter(|v| !v.is_empty());
                let data = self.request("POST", "/auth/session", Some(json!({ "mcUuid": uuid, "mcName": name, "chaldId": chald_id })), false).await?;
                self.adopt(&data);
                Ok(data)
            }
            "friends:register" => {
                let password = text("password").unwrap_or("");
                if !(6..=72).contains(&password.chars().count()) {
                    return Err(AppError::new("INVALID_ARGS", "Пароль: от 6 до 72 символов"));
                }
                let (uuid, name) = account.identity()?;
                if !is_chald_login(&name) {
                    return Err(AppError::new("FRIENDS_AUTH", "Ник должен быть из 3–20 символов (латиница, цифры, «_»), чтобы стать Chald ID."));
                }
                let body = json!({ "login": name, "password": password, "mcUuid": uuid, "mcName": name, "chaldId": name });
                let data = self.request("POST", "/auth/register", Some(body), false).await?;
                self.adopt(&data);
                Ok(data)
            }
            "friends:login" => {
                let login = text("login").unwrap_or("").trim().trim_start_matches('@').to_string();
                let password = text("password").unwrap_or("");
                if !is_chald_login(&login) {
                    return Err(AppError::new("INVALID_ARGS", "Логин (он же Chald ID): 3–20 символов, латиница, цифры и «_»"));
                }
                if !(6..=72).contains(&password.chars().count()) {
                    return Err(AppError::new("INVALID_ARGS", "Пароль: от 6 до 72 символов"));
                }
                let data = self.request("POST", "/auth/login", Some(json!({ "login": login, "password": password })), false).await?;
                self.adopt(&data);
                Ok(data)
            }
            "friends:me" => Ok(self.request("GET", "/me", None, true).await?.get("user").cloned().unwrap_or(Value::Null)),
            "friends:updateMe" => {
                let mut patch = serde_json::Map::new();
                for key in ["displayName", "bio", "chaldId"] {
                    if let Some(value) = arg.get(key).filter(|v| v.is_string()) {
                        patch.insert(key.to_string(), value.clone());
                    }
                }
                Ok(self.request("PATCH", "/me", Some(Value::Object(patch)), true).await?.get("user").cloned().unwrap_or(Value::Null))
            }
            "friends:search" => {
                let q = url::form_urlencoded::byte_serialize(text("q").unwrap_or("").trim().as_bytes()).collect::<String>();
                let data = self.request("GET", &format!("/users/search?q={q}"), None, true).await?;
                Ok(list_of(&data, "users"))
            }
            "friends:list" => {
                let data = self.request("GET", "/friends", None, true).await?;
                Ok(json!({ "friends": list_of(&data, "friends"), "incoming": list_of(&data, "incoming"), "outgoing": list_of(&data, "outgoing") }))
            }
            "friends:request" => self.request("POST", "/friends/request", Some(arg.clone()), true).await,
            "friends:accept" => self.request("POST", &format!("/friends/accept/{}", int("requestId")?), None, true).await,
            "friends:reject" => self.request("POST", &format!("/friends/reject/{}", int("requestId")?), None, true).await,
            "friends:remove" => self.request("DELETE", &format!("/friends/{}", int("userId")?), None, true).await,
            "friends:inbox" => {
                let data = self.request("GET", "/messages/inbox", None, true).await?;
                let conversations: Vec<Value> = data
                    .get("conversations")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter(|c| c.get("user").map(|u| !u.is_null()).unwrap_or(false)).cloned().collect())
                    .unwrap_or_default();
                Ok(json!({ "conversations": conversations }))
            }
            "friends:thread" => {
                let data = self.request("GET", &format!("/messages/with/{}?afterId={after}&limit=80", int("peerId")?), None, true).await?;
                Ok(json!({ "messages": list_of(&data, "messages"), "peer": data.get("peer").cloned().unwrap_or(Value::Null) }))
            }
            "friends:send" => {
                let body = json!({ "toUserId": int("toUserId")?, "body": text("body").unwrap_or(""), "kind": text("kind").unwrap_or("text"), "meta": arg.get("meta").cloned() });
                self.request("POST", "/messages", Some(body), true).await
            }
            "friends:invite" => {
                let body = json!({ "toUserId": int("toUserId")?, "instanceName": text("instanceName").unwrap_or(""), "address": text("address").unwrap_or(""), "note": text("note").unwrap_or("") });
                self.request("POST", "/invites", Some(body), true).await
            }
            "friends:invites" => self.request("GET", "/invites", None, true).await,
            "friends:inviteAccept" => self.request("POST", &format!("/invites/{}/accept", int("inviteId")?), None, true).await,
            "friends:inviteDecline" => self.request("POST", &format!("/invites/{}/decline", int("inviteId")?), None, true).await,
            "friends:callSignal" => {
                let mut body = json!({ "toUserId": int("toUserId")?, "kind": text("kind").unwrap_or(""), "payload": text("payload").unwrap_or("") });
                if let Some(group) = arg.get("groupId").and_then(Value::as_i64).filter(|g| *g > 0) {
                    body["groupId"] = json!(group);
                }
                self.request("POST", "/calls/signal", Some(body), true).await
            }
            "friends:callPoll" => self.request("GET", "/calls/signals", None, true).await,
            "friends:uploadAvatar" => {
                let data_url = text("dataUrl").unwrap_or("");
                if !data_url.starts_with("data:image/png") {
                    return Err(AppError::new("AVATAR_TYPE", "Нужен PNG"));
                }
                if !(32..=200_000).contains(&data_url.len()) {
                    return Err(AppError::new("INVALID_ARGS", "Некорректный размер иконки"));
                }
                self.request("POST", "/me/avatar", Some(json!({ "dataUrl": data_url })), true).await?;
                Ok(json!(true))
            }
            "friends:avatarData" => Ok(self.avatar_data(int("userId")?).await.unwrap_or(Value::Null)),
            "friends:groups" => match self.request("GET", "/groups", None, true).await {
                Ok(data) => {
                    let groups: Vec<Value> = data.get("groups").and_then(Value::as_array).map(|a| a.iter().filter(|g| g.get("id").is_some()).cloned().collect()).unwrap_or_default();
                    Ok(json!(groups))
                }
                // старые серверы без групп: пустой список, страница не ломается
                Err(error) if error.code == "FRIENDS_NOT_FOUND" || error.code == "FRIENDS_HTTP" => Ok(json!([])),
                Err(error) => Err(error),
            },
            "friends:groupCreate" => {
                let name: String = text("name").unwrap_or("").trim().chars().take(64).collect();
                let members: Vec<i64> = arg.get("memberIds").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_i64).collect()).unwrap_or_default();
                Ok(self.request("POST", "/groups", Some(json!({ "name": name, "memberIds": members })), true).await?.get("group").cloned().unwrap_or(Value::Null))
            }
            "friends:groupGet" => Ok(self.request("GET", &format!("/groups/{}", int("groupId")?), None, true).await?.get("group").cloned().unwrap_or(Value::Null)),
            "friends:groupAddMember" => {
                let body = json!({ "userId": int("userId")? });
                Ok(self.request("POST", &format!("/groups/{}/members", int("groupId")?), Some(body), true).await?.get("group").cloned().unwrap_or(Value::Null))
            }
            "friends:groupRemoveMember" => self.request("DELETE", &format!("/groups/{}/members/{}", int("groupId")?, int("userId")?), None, true).await,
            "friends:groupUpdate" => {
                let name: String = text("name").unwrap_or("").trim().chars().take(64).collect();
                Ok(self.request("PATCH", &format!("/groups/{}", int("groupId")?), Some(json!({ "name": name })), true).await?.get("group").cloned().unwrap_or(Value::Null))
            }
            "friends:groupDelete" => self.request("DELETE", &format!("/groups/{}", int("groupId")?), None, true).await,
            "friends:groupThread" => {
                let data = self.request("GET", &format!("/groups/{}/messages?afterId={after}&limit=80", int("groupId")?), None, true).await?;
                Ok(json!({ "messages": list_of(&data, "messages"), "group": data.get("group").cloned().unwrap_or(Value::Null) }))
            }
            "friends:groupSend" => {
                let body = json!({ "body": text("body").unwrap_or(""), "kind": text("kind").unwrap_or("text"), "meta": arg.get("meta").cloned() });
                self.request("POST", &format!("/groups/{}/messages", int("groupId")?), Some(body), true).await
            }
            other => Err(AppError::not_implemented(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chald_logins() {
        assert!(is_chald_login("Steve_1"));
        assert!(!is_chald_login("ab"));
        assert!(!is_chald_login("bad name"));
        assert!(!is_chald_login("this_login_is_way_too_long"));
    }

    #[test]
    fn legacy_token_is_reset_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("friends-token.txt"), "old-token").unwrap();
        let first = FriendsService::new(dir.path());
        assert!(!first.has_token());
        assert!(first.consume_legacy_reset());
        assert!(!first.consume_legacy_reset());
        first.set_token(Some("new"));
        let second = FriendsService::new(dir.path());
        assert!(second.has_token());
        assert!(!second.consume_legacy_reset());
    }

    #[test]
    fn identity_needs_a_logged_in_account() {
        let offline = Account { mode: "offline".into(), logged_in: false, uuid: None, name: None };
        assert_eq!(offline.identity().unwrap_err().code, "FRIENDS_AUTH");
        let ok = Account { mode: "microsoft".into(), logged_in: true, uuid: Some("u".into()), name: Some("Steve".into()) };
        assert_eq!(ok.identity().unwrap().1, "Steve");
    }
}
