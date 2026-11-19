use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use keyring::Entry;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::DEFAULT_MS_CLIENT_ID;
use crate::errors::{AppError, AppResult};
use crate::game;
use crate::launch_args::LaunchAuth;
use crate::net;
use crate::offline;
use crate::paths;
use crate::settings::Settings;

const SCOPE: &str = "XboxLive.signin offline_access";
const TENANT: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    refresh_token: String,
    uuid: String,
    name: String,
    client_id: String,
}

#[derive(Debug, Clone)]
struct Session {
    access_token: String,
    /// seconds since UNIX epoch
    expires_at: u64,
    uuid: String,
    name: String,
}

#[derive(Debug, Default, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    expires_in: u64,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn random_b64(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buffer);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}

fn challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn format_uuid(id: &str) -> String {
    let clean = id.replace('-', "");
    if clean.len() == 32 {
        format!(
            "{}-{}-{}-{}-{}",
            &clean[0..8],
            &clean[8..12],
            &clean[12..16],
            &clean[16..20],
            &clean[20..32]
        )
    } else {
        id.to_string()
    }
}

fn xbox_message(code: Option<i64>) -> Option<&'static str> {
    match code {
        Some(2148916233) => Some("У этого аккаунта Microsoft нет профиля Xbox. Создай его на xbox.com."),
        Some(2148916238) => Some("Детский аккаунт: добавь его в семейную группу Microsoft, чтобы играть."),
        Some(2148916235) => Some("Xbox Live недоступен в твоей стране."),
        _ => None,
    }
}

fn outage(status: u16, host: &str) -> AppError {
    AppError::new(
        "AUTH_UNAVAILABLE",
        format!("Серверы входа сейчас недоступны (HTTP {status}, {host}). Попробуй позже."),
    )
}

async fn token_request(form: Vec<(String, String)>, purpose: &str) -> AppResult<TokenResponse> {
    let (status, text) = net::post_form_raw(&format!("{TENANT}/token"), &form, purpose).await?;
    let parsed: TokenResponse = serde_json::from_str(&text).unwrap_or_default();
    if !parsed.access_token.is_empty() {
        return Ok(parsed);
    }
    if status >= 500 || status == 429 {
        return Err(outage(status, "login.microsoftonline.com"));
    }
    if let Some(error) = parsed.error.as_deref() {
        let message = parsed.error_description.clone().unwrap_or_else(|| error.to_string());
        return Err(AppError::new("AUTH_DENIED", message));
    }
    Err(AppError::new("AUTH_HTTP", format!("Microsoft не выдал токен (HTTP {status})")))
}

async fn xbox_post(url: &str, body: &Value, purpose: &str) -> AppResult<Value> {
    let (status, text) = net::post_json_raw(url, body, purpose, None).await?;
    if status >= 500 || status == 429 {
        return Err(outage(status, "xboxlive.com"));
    }
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status != 200 {
        let code = value.get("XErr").and_then(Value::as_i64);
        return Err(AppError::new(
            "AUTH_HTTP",
            xbox_message(code).map(str::to_string).unwrap_or_else(|| format!("Ошибка авторизации (HTTP {status})")),
        ));
    }
    Ok(value)
}

async fn minecraft_login(ms_token: &str) -> AppResult<Session> {
    let xbl = xbox_post(
        "https://user.auth.xboxlive.com/user/authenticate",
        &json!({
            "Properties": { "AuthMethod": "RPS", "SiteName": "user.auth.xboxlive.com", "RpsTicket": format!("d={ms_token}") },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT"
        }),
        "Xbox Live: вход",
    )
    .await?;
    let xbl_token = xbl
        .get("Token")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::new("AUTH_HTTP", "Xbox Live не вернул токен"))?;

    let xsts = xbox_post(
        "https://xsts.auth.xboxlive.com/xsts/authorize",
        &json!({
            "Properties": { "SandboxId": "RETAIL", "UserTokens": [xbl_token] },
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT"
        }),
        "Xbox Live: XSTS",
    )
    .await?;
    let xsts_token = xsts
        .get("Token")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::new("AUTH_HTTP", "Xbox Live не вернул XSTS-токен"))?;
    let uhs = xsts
        .pointer("/DisplayClaims/xui/0/uhs")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::new("AUTH_HTTP", "Xbox Live не вернул идентификатор пользователя"))?;

    let (status, text) = net::post_json_raw(
        "https://api.minecraftservices.com/authentication/login_with_xbox",
        &json!({ "identityToken": format!("XBL3.0 x={uhs};{xsts_token}") }),
        "Minecraft: вход",
        None,
    )
    .await?;
    if status >= 500 || status == 429 {
        return Err(outage(status, "api.minecraftservices.com"));
    }
    let login: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status != 200 {
        return Err(AppError::new("AUTH_HTTP", format!("Не удалось войти в Minecraft (HTTP {status})")));
    }
    let access_token = login
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::new("AUTH_HTTP", "Minecraft не вернул токен"))?
        .to_string();
    let expires_in = login.get("expires_in").and_then(Value::as_u64).unwrap_or(86_400);

    let (status, text) = net::get_raw(
        "https://api.minecraftservices.com/minecraft/profile",
        "Minecraft: профиль",
        Some(&access_token),
    )
    .await?;
    if status == 404 {
        return Err(AppError::new("AUTH_NO_GAME", "На этом аккаунте нет Minecraft Java Edition. Игра должна быть куплена."));
    }
    if status >= 500 {
        return Err(outage(status, "api.minecraftservices.com"));
    }
    if status != 200 {
        return Err(AppError::new("AUTH_HTTP", format!("Не удалось получить профиль Minecraft (HTTP {status})")));
    }
    let profile: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let name = profile
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::new("AUTH_HTTP", "В профиле Minecraft нет ника"))?
        .to_string();
    let uuid = format_uuid(
        profile
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::new("AUTH_HTTP", "В профиле Minecraft нет UUID"))?,
    );
    Ok(Session { access_token, expires_at: now_secs() + expires_in, uuid, name })
}

fn msa(session: Session) -> LaunchAuth {
    LaunchAuth {
        name: session.name,
        uuid: session.uuid,
        access_token: session.access_token,
        user_type: "msa".to_string(),
        offline_fallback: false,
    }
}

/// Microsoft sign-in and the stored refresh token.
/// Stored in the system's secure credential storage (Keychain/Credential Manager/Secret Service)
/// instead of a plaintext JSON file. Legacy `auth.json` is migrated on first load and removed.
pub struct AuthService {
    /// Legacy plaintext file path, kept only for migration/fallback.
    file: PathBuf,
    data_dir: PathBuf,
    stored: Mutex<Option<Stored>>,
    session: Mutex<Option<Session>>,
    loaded: Mutex<bool>,
    cancel: Arc<AtomicBool>,
}

impl AuthService {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            file: data_dir.join("auth.json"),
            data_dir: data_dir.to_path_buf(),
            stored: Mutex::new(None),
            session: Mutex::new(None),
            loaded: Mutex::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    fn keyring_entry(&self) -> Option<Entry> {
        let profile = self
            .data_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("default");
        Entry::new("studio.chald.launcher", &format!("microsoft-auth:{profile}")).ok()
    }

    fn load_from_keyring(&self) -> Option<Stored> {
        let entry = self.keyring_entry()?;
        let secret = entry.get_password().ok()?;
        serde_json::from_str::<Stored>(&secret).ok()
    }

    fn save_to_keyring(&self, stored: &Stored) -> Result<(), String> {
        let entry = self.keyring_entry().ok_or_else(|| "keyring entry unavailable".to_string())?;
        let secret = serde_json::to_string(stored).map_err(|e| e.to_string())?;
        entry.set_password(&secret).map_err(|e| e.to_string())
    }

    fn delete_from_keyring(&self) {
        if let Some(entry) = self.keyring_entry() {
            let _ = entry.delete_credential();
        }
    }

    fn load(&self) {
        let mut loaded = self.loaded.lock().unwrap();
        if *loaded {
            return;
        }
        *loaded = true;

        if let Some(stored) = self.load_from_keyring() {
            *self.stored.lock().unwrap() = Some(stored);
            let _ = std::fs::remove_file(&self.file);
            return;
        }

        // Migrate legacy plaintext auth.json -> secure storage.
        if let Ok(bytes) = std::fs::read(&self.file) {
            if let Ok(stored) = serde_json::from_slice::<Stored>(&bytes) {
                *self.stored.lock().unwrap() = Some(stored.clone());
                if self.save_to_keyring(&stored).is_ok() {
                    let _ = std::fs::remove_file(&self.file);
                }
            }
        }
    }

    fn persist(&self, stored: &Stored) -> AppResult<()> {
        if self.save_to_keyring(stored).is_ok() {
            let _ = std::fs::remove_file(&self.file);
            return Ok(());
        }
        // Fallback: keep working if secure storage is unavailable.
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(stored)?;
        let tmp = self.file.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.file)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.file, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn state(&self, settings: &Settings) -> Value {
        self.load();
        let offline_mode = settings.account_mode == "offline";
        let offline_ok = offline_mode && paths::is_offline_name(&settings.offline_name);
        let stored = self.stored.lock().unwrap().clone();
        let (name, uuid, logged_in) = if offline_mode {
            (
                if offline_ok { Some(settings.offline_name.clone()) } else { None },
                if offline_ok { Some(offline::uuid(&settings.offline_name)) } else { None },
                offline_ok,
            )
        } else {
            (
                stored.as_ref().map(|stored| stored.name.clone()),
                stored.as_ref().map(|stored| stored.uuid.clone()),
                stored.is_some(),
            )
        };
        json!({
            "mode": if offline_mode { "offline" } else { "microsoft" },
            "loggedIn": logged_in,
            "name": name,
            "uuid": uuid,
            "configured": !DEFAULT_MS_CLIENT_ID.is_empty(),
            "encryptionAvailable": false,
        })
    }

    pub async fn login(&self, method: &str, app: &tauri::AppHandle) -> AppResult<()> {
        if DEFAULT_MS_CLIENT_ID.is_empty() {
            return Err(AppError::new(
                "AUTH_NOT_CONFIGURED",
                "Не указан Azure Client ID: без него вход через Microsoft невозможен.",
            ));
        }
        self.cancel.store(false, Ordering::SeqCst);
        let tokens = if method == "code" {
            self.device_flow(DEFAULT_MS_CLIENT_ID, app).await?
        } else {
            self.browser_flow(DEFAULT_MS_CLIENT_ID).await?
        };
        let session = minecraft_login(&tokens.access_token).await?;
        let stored = Stored {
            refresh_token: tokens.refresh_token.clone().unwrap_or_default(),
            uuid: session.uuid.clone(),
            name: session.name.clone(),
            client_id: DEFAULT_MS_CLIENT_ID.to_string(),
        };
        *self.session.lock().unwrap() = Some(session);
        *self.stored.lock().unwrap() = Some(stored.clone());
        self.persist(&stored)?;
        Ok(())
    }

    async fn refresh(&self, stored: &Stored) -> AppResult<Session> {
        let tokens = token_request(
            vec![
                ("client_id".to_string(), stored.client_id.clone()),
                ("grant_type".to_string(), "refresh_token".to_string()),
                ("refresh_token".to_string(), stored.refresh_token.clone()),
                ("scope".to_string(), SCOPE.to_string()),
            ],
            "Вход Microsoft: обновление сессии",
        )
        .await?;
        let session = minecraft_login(&tokens.access_token).await?;
        if let Some(refresh) = tokens.refresh_token.clone() {
            let mut next = stored.clone();
            next.refresh_token = refresh;
            *self.stored.lock().unwrap() = Some(next.clone());
            let _ = self.persist(&next);
        }
        Ok(session)
    }

    /// Minecraft access token for account APIs (wardrobe). Microsoft account only, refreshes when needed.
    pub async fn account_token(&self, settings: &Settings) -> AppResult<String> {
        if settings.account_mode == "offline" {
            return Err(AppError::new(
                "WARDROBE_NEEDS_MS",
                "Скин на аккаунте можно менять только после входа через Microsoft (офлайн-режим скин не поддерживает).",
            ));
        }
        self.load();
        if let Some(session) = self.session.lock().unwrap().clone() {
            if session.expires_at > now_secs() + 300 {
                return Ok(session.access_token);
            }
        }
        let stored = self.stored.lock().unwrap().clone().ok_or_else(|| {
            AppError::new(
                "WARDROBE_NEEDS_MS",
                "Скин на аккаунте можно менять только после входа через Microsoft (офлайн-режим скин не поддерживает).",
            )
        })?;
        let session = self.refresh(&stored).await?;
        let token = session.access_token.clone();
        *self.session.lock().unwrap() = Some(session);
        Ok(token)
    }

    pub async fn launch_auth(&self, settings: &Settings) -> AppResult<LaunchAuth> {
        if settings.account_mode == "offline" {
            if !paths::is_offline_name(&settings.offline_name) {
                return Err(AppError::new("AUTH_REQUIRED", "Укажи ник для офлайн-режима в настройках."));
            }
            return Ok(LaunchAuth {
                name: settings.offline_name.clone(),
                uuid: offline::uuid(&settings.offline_name),
                access_token: "0".to_string(),
                user_type: "legacy".to_string(),
                offline_fallback: false,
            });
        }
        self.load();
        if let Some(session) = self.session.lock().unwrap().clone() {
            if session.expires_at > now_secs() + 300 {
                return Ok(msa(session));
            }
        }
        let stored = self
            .stored
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| AppError::new("AUTH_REQUIRED", "Войди в аккаунт Microsoft, чтобы запустить игру."))?;
        match self.refresh(&stored).await {
            Ok(session) => {
                *self.session.lock().unwrap() = Some(session.clone());
                Ok(msa(session))
            }
            Err(error) => {
                let network = matches!(error.code.as_str(), "NET_FAILED" | "AUTH_UNAVAILABLE");
                if network && !stored.uuid.is_empty() && !stored.name.is_empty() {
                    Ok(LaunchAuth {
                        name: stored.name,
                        uuid: stored.uuid,
                        access_token: "0".to_string(),
                        user_type: "legacy".to_string(),
                        offline_fallback: true,
                    })
                } else {
                    Err(error)
                }
            }
        }
    }

    pub fn logout(&self, settings: &Settings) -> AppResult<Value> {
        *self.session.lock().unwrap() = None;
        *self.stored.lock().unwrap() = None;
        self.delete_from_keyring();
        let _ = std::fs::remove_file(&self.file);
        Ok(self.state(settings))
    }

    async fn device_flow(&self, client_id: &str, app: &tauri::AppHandle) -> AppResult<TokenResponse> {
        let (status, text) = net::post_form_raw(
            &format!("{TENANT}/devicecode"),
            &[
                ("client_id".to_string(), client_id.to_string()),
                ("scope".to_string(), SCOPE.to_string()),
            ],
            "Вход Microsoft: код устройства",
        )
        .await?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status != 200 {
            return Err(AppError::new(
                "AUTH_HTTP",
                value
                    .get("error_description")
                    .and_then(Value::as_str)
                    .unwrap_or("Microsoft отклонил запрос входа")
                    .to_string(),
            ));
        }
        let device_code = value.get("device_code").and_then(Value::as_str).unwrap_or("").to_string();
        let user_code = value.get("user_code").and_then(Value::as_str).unwrap_or("").to_string();
        let verification_uri = value.get("verification_uri").and_then(Value::as_str).unwrap_or("").to_string();
        let expires_in = value.get("expires_in").and_then(Value::as_u64).unwrap_or(900);
        let interval = value.get("interval").and_then(Value::as_u64).unwrap_or(5).max(2);

        game::emit(
            app,
            "auth:deviceCode",
            json!({
                "userCode": user_code,
                "verificationUri": verification_uri,
                "expiresInSec": expires_in,
            }),
        );
        if verification_uri.starts_with("https://microsoft.com/")
            || verification_uri.starts_with("https://www.microsoft.com/")
            || verification_uri.starts_with("https://login.live.com/")
        {
            let _ = open::that(&verification_uri);
        }

        let started = now_secs();
        let mut wait = interval;
        while now_secs() < started + expires_in {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            if self.cancel.load(Ordering::SeqCst) {
                return Err(AppError::new("AUTH_CANCELLED", "Вход отменён"));
            }
            let (status, text) = net::post_form_raw(
                &format!("{TENANT}/token"),
                &[
                    ("grant_type".to_string(), "urn:ietf:params:oauth:grant-type:device_code".to_string()),
                    ("client_id".to_string(), client_id.to_string()),
                    ("device_code".to_string(), device_code.clone()),
                ],
                "Вход Microsoft: ожидание подтверждения",
            )
            .await?;
            let parsed: TokenResponse = serde_json::from_str(&text).unwrap_or_default();
            if !parsed.access_token.is_empty() {
                return Ok(parsed);
            }
            match parsed.error.as_deref() {
                Some("authorization_pending") => {}
                Some("slow_down") => wait += 5,
                Some("authorization_declined") => {
                    return Err(AppError::new("AUTH_DENIED", "Вход отклонён"));
                }
                Some(other) => {
                    return Err(AppError::new(
                        "AUTH_DENIED",
                        parsed.error_description.unwrap_or_else(|| other.to_string()),
                    ));
                }
                None if status >= 500 || status == 429 => return Err(outage(status, "login.microsoftonline.com")),
                None => {}
            }
        }
        Err(AppError::new("AUTH_EXPIRED", "Время ожидания входа истекло"))
    }

    async fn browser_flow(&self, client_id: &str) -> AppResult<TokenResponse> {
        let verifier = random_b64(48);
        let challenge = challenge(&verifier);
        let state = random_b64(16);

        let server = tiny_http::Server::http("127.0.0.1:0")
            .map_err(|e| AppError::new("AUTH_LOOPBACK", format!("Не удалось открыть локальный порт для входа: {e}")))?;
        let port = server
            .server_addr()
            .to_ip()
            .map(|address| address.port())
            .ok_or_else(|| AppError::new("AUTH_LOOPBACK", "Не удалось определить порт для входа"))?;
        let redirect_uri = format!("http://localhost:{port}");

        let mut auth_url = url::Url::parse(&format!("{TENANT}/authorize"))
            .map_err(|e| AppError::new("AUTH_LOOPBACK", e.to_string()))?;
        auth_url
            .query_pairs_mut()
            .append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", SCOPE)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("prompt", "select_account");
        open::that(auth_url.as_str())
            .map_err(|e| AppError::new("AUTH_BROWSER", format!("Не удалось открыть браузер: {e}")))?;

        let cancel = self.cancel.clone();
        let expected_state = state.clone();
        let code = tauri::async_runtime::spawn_blocking(move || -> AppResult<String> {
            let deadline = SystemTime::now() + Duration::from_secs(300);
            loop {
                if cancel.load(Ordering::SeqCst) {
                    return Err(AppError::new("AUTH_CANCELLED", "Вход отменён"));
                }
                if SystemTime::now() > deadline {
                    return Err(AppError::new("AUTH_EXPIRED", "Время ожидания входа истекло"));
                }
                let request = match server.recv_timeout(Duration::from_millis(250)) {
                    Ok(Some(request)) => request,
                    Ok(None) => continue,
                    Err(_) => continue,
                };
                let raw = format!("http://localhost{}", request.url());
                let Ok(parsed) = url::Url::parse(&raw) else {
                    respond(request, &result_page(false, "Некорректный запрос", "Эту ссылку лаунчер не ждал. Вернись в ChaldLauncher и повтори вход."));
                    continue;
                };
                if parsed.path() != "/" {
                    respond(request, &result_page(false, "Некорректный запрос", "Эту ссылку лаунчер не ждал. Вернись в ChaldLauncher и повтори вход."));
                    continue;
                }
                let params: std::collections::HashMap<String, String> =
                    parsed.query_pairs().into_owned().collect();
                if params.get("state") != Some(&expected_state) {
                    respond(request, &result_page(false, "Неверный запрос", "Сессия входа не совпала. Вернись в ChaldLauncher и повтори вход."));
                    continue;
                }
                if let Some(error) = params.get("error") {
                    respond(request, &result_page(false, "Вход не выполнен", "Microsoft отклонил вход. Закрой вкладку и попробуй ещё раз в лаунчере."));
                    return Err(AppError::new(
                        "AUTH_DENIED",
                        params.get("error_description").cloned().unwrap_or_else(|| error.clone()),
                    ));
                }
                if let Some(code) = params.get("code") {
                    respond(request, &result_page(true, "Вход выполнен", "Аккаунт Microsoft подключён к ChaldLauncher. Эту вкладку можно закрыть и вернуться в лаунчер."));
                    return Ok(code.clone());
                }
                respond(request, &result_page(false, "Нет кода входа", "Microsoft не прислал код. Вернись в ChaldLauncher и повтори вход."));
            }
        })
        .await
        .map_err(|e| AppError::new("AUTH_LOOPBACK", e.to_string()))??;

        token_request(
            vec![
                ("grant_type".to_string(), "authorization_code".to_string()),
                ("client_id".to_string(), client_id.to_string()),
                ("code".to_string(), code),
                ("redirect_uri".to_string(), redirect_uri),
                ("code_verifier".to_string(), verifier),
                ("scope".to_string(), SCOPE.to_string()),
            ],
            "Вход Microsoft: обмен кода",
        )
        .await
    }
}

fn respond(request: tiny_http::Request, html: &str) {
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
    let _ = request.respond(tiny_http::Response::from_string(html).with_header(header));
}


/// Страница, которую видит игрок в браузере после входа Microsoft (самодостаточная, без внешних ресурсов).
fn result_page(success: bool, title: &str, text: &str) -> String {
    let (accent, glow, icon) = if success {
        ("#3ddc97", "61,220,151", "<path class=\"mark\" d=\"M18 33l9 9 19-21\"/>")
    } else {
        ("#ff5c6c", "255,92,108", "<path class=\"mark\" d=\"M21 21l22 22M43 21L21 43\"/>")
    };
    let hint = if success { "Окно лаунчера уже обновилось" } else { "Ничего страшного — можно повторить" };
    format!(
        r#"<!doctype html>
<html lang="ru"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>ChaldLauncher — {title}</title>
<style>
*{{box-sizing:border-box;margin:0}}
html,body{{height:100%}}
body{{display:grid;place-items:center;padding:24px;color:#e6f4ff;font:16px/1.5 system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;
background:radial-gradient(900px 500px at 50% -10%,rgba(0,172,255,.18),transparent 70%),radial-gradient(600px 400px at 85% 110%,rgba(0,213,255,.10),transparent 70%),#020d18;overflow:hidden}}
body:before{{content:"";position:fixed;inset:0;background-image:linear-gradient(rgba(0,172,255,.06) 1px,transparent 1px),linear-gradient(90deg,rgba(0,172,255,.06) 1px,transparent 1px);background-size:40px 40px;mask-image:radial-gradient(circle at 50% 40%,#000,transparent 75%);-webkit-mask-image:radial-gradient(circle at 50% 40%,#000,transparent 75%)}}
.card{{position:relative;width:min(440px,100%);padding:40px 32px 28px;text-align:center;border-radius:16px;
background:linear-gradient(180deg,#07243a,#041827);border:1px solid rgba(0,172,255,.45);box-shadow:0 20px 60px rgba(0,0,0,.55),0 0 40px rgba(0,172,255,.18);animation:up .5s ease-out both}}
.brand{{position:absolute;top:-1px;left:50%;transform:translateX(-50%);padding:4px 14px;border-radius:0 0 10px 10px;font:600 12px/1.6 ui-monospace,Consolas,monospace;letter-spacing:.22em;text-transform:uppercase;color:#012033;background:#00d5ff}}
.badge{{width:88px;height:88px;margin:8px auto 22px;border-radius:50%;display:grid;place-items:center;background:rgba({glow},.12);border:2px solid {accent};box-shadow:0 0 28px rgba({glow},.45);animation:pop .55s .15s cubic-bezier(.2,1.4,.4,1) both}}
.badge svg{{width:56px;height:56px;fill:none;stroke:{accent};stroke-width:5;stroke-linecap:round;stroke-linejoin:round}}
.mark{{stroke-dasharray:60;stroke-dashoffset:60;animation:draw .5s .55s ease-out forwards}}
h1{{font-size:26px;letter-spacing:.02em;margin-bottom:8px}}
p{{color:#8fb0c8;max-width:34ch;margin:0 auto}}
.hint{{margin-top:26px;padding-top:16px;border-top:1px solid rgba(0,172,255,.16);font:12px/1.4 ui-monospace,Consolas,monospace;letter-spacing:.08em;text-transform:uppercase;color:#5d7f98}}
@keyframes up{{from{{opacity:0;transform:translateY(14px)}}to{{opacity:1;transform:none}}}}
@keyframes pop{{from{{opacity:0;transform:scale(.4)}}to{{opacity:1;transform:none}}}}
@keyframes draw{{to{{stroke-dashoffset:0}}}}
@media (prefers-reduced-motion:reduce){{*{{animation:none!important}}.mark{{stroke-dashoffset:0}}}}
</style></head>
<body><main class="card">
<div class="brand">ChaldLauncher</div>
<div class="badge"><svg viewBox="0 0 64 64" aria-hidden="true">{icon}</svg></div>
<h1>{title}</h1>
<p>{text}</p>
<div class="hint">{hint}</div>
</main></body></html>"#,
        title = title,
        text = text,
        accent = accent,
        glow = glow,
        icon = icon,
        hint = hint,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_page_is_self_contained() {
        let ok = result_page(true, "Вход выполнен", "x");
        assert!(ok.contains("Вход выполнен") && ok.contains("#3ddc97"));
        assert!(!ok.contains("http://") && !ok.contains("https://"));
        assert!(result_page(false, "e", "x").contains("#ff5c6c"));
    }

    #[test]
    fn formats_uuids_and_codes() {
        assert_eq!(format_uuid("0123456789abcdef0123456789abcdef"), "01234567-89ab-cdef-0123-456789abcdef");
        assert_eq!(format_uuid("already-good"), "already-good");
        assert!(xbox_message(Some(2148916233)).unwrap().contains("Xbox"));
        assert!(xbox_message(Some(1)).is_none());
    }

    #[test]
    fn offline_state() {
        let dir = tempfile::tempdir().unwrap();
        let auth = AuthService::new(dir.path());
        let mut settings = Settings::default();
        settings.account_mode = "offline".to_string();
        settings.offline_name = "Steve".to_string();
        let state = auth.state(&settings);
        assert_eq!(state["mode"], "offline");
        assert_eq!(state["loggedIn"], true);
        assert_eq!(state["name"], "Steve");
        assert_eq!(state["uuid"].as_str().unwrap(), offline::uuid("Steve"));
    }

    #[test]
    fn microsoft_state_without_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let auth = AuthService::new(dir.path());
        let state = auth.state(&Settings::default());
        assert_eq!(state["mode"], "microsoft");
        assert_eq!(state["loggedIn"], false);
        assert_eq!(state["name"], Value::Null);
    }

    #[test]
    fn migrates_legacy_auth_json_to_secure_storage() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        let stored = Stored {
            refresh_token: "rt".to_string(),
            uuid: "u".to_string(),
            name: "Steve".to_string(),
            client_id: "cid".to_string(),
        };
        std::fs::write(&file, serde_json::to_vec_pretty(&stored).unwrap()).unwrap();
        let auth = AuthService::new(dir.path());
        auth.load();
        assert_eq!(auth.stored.lock().unwrap().as_ref().unwrap().name, "Steve");
        // If secure storage is available the plaintext file is removed; if not, fallback keeps it.
        if auth.load_from_keyring().is_some() {
            assert!(!file.exists());
        }
    }

    #[test]
    fn logout_clears_stored_auth() {
        let dir = tempfile::tempdir().unwrap();
        let auth = AuthService::new(dir.path());
        {
            let mut stored = auth.stored.lock().unwrap();
            *stored = Some(Stored {
                refresh_token: "rt".to_string(),
                uuid: "u".to_string(),
                name: "Steve".to_string(),
                client_id: "cid".to_string(),
            });
        }
        *auth.loaded.lock().unwrap() = true;
        let state = auth.logout(&Settings::default()).unwrap();
        assert_eq!(state["loggedIn"], false);
        assert!(auth.stored.lock().unwrap().is_none());
        assert!(auth.load_from_keyring().is_none());
        assert!(!dir.path().join("auth.json").exists());
    }
}
