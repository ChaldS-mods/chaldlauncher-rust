use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use reqwest::redirect::Policy;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::errors::{AppError, AppResult};

/// Every host the launcher is allowed to talk to (the same list as the Electron `net.ts`).
pub const ALLOWED_HOSTS: &[&str] = &[
    "piston-meta.mojang.com",
    "piston-data.mojang.com",
    "launchermeta.mojang.com",
    "libraries.minecraft.net",
    "resources.download.minecraft.net",
    "meta.fabricmc.net",
    "maven.fabricmc.net",
    "meta.quiltmc.org",
    "maven.quiltmc.org",
    "maven.minecraftforge.net",
    "files.minecraftforge.net",
    "maven.neoforged.net",
    "repo1.maven.org",
    "api.adoptium.net",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "api.github.com",
    "api.modrinth.com",
    "cdn.modrinth.com",
    "media.modrinth.com",
    "api.curseforge.com",
    "edge.forgecdn.net",
    "media.forgecdn.net",
    "mediafilez.forgecdn.net",
    "raw.githubusercontent.com",
    "gitlab.com",
    "login.microsoftonline.com",
    "user.auth.xboxlive.com",
    "xsts.auth.xboxlive.com",
    "api.minecraftservices.com",
    "textures.minecraft.net",
    "127.0.0.1",
    "localhost",
    "chaldserver.dynmap.xyz",
];

const FRIENDS_HTTP: &[&str] = &["127.0.0.1", "localhost", "chaldserver.dynmap.xyz"];
const MAX_LOG: usize = 500;

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(90))
            .user_agent("ChaldStudio/ChaldLauncher/1.1.0 (Minecraft launcher)")
            .build()
            .expect("HTTP client")
    })
}

fn log_store() -> &'static Mutex<Vec<Value>> {
    static LOG: OnceLock<Mutex<Vec<Value>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(Vec::new()))
}

fn next_id() -> u64 {
    static ID: AtomicU64 = AtomicU64::new(0);
    ID.fetch_add(1, Ordering::SeqCst) + 1
}

fn push(entry: Value) {
    let mut entries = log_store().lock().unwrap();
    entries.push(entry);
    if entries.len() > MAX_LOG {
        entries.remove(0);
    }
}

pub fn net_log() -> Vec<Value> {
    log_store().lock().unwrap().clone()
}

pub fn clear_net_log() {
    log_store().lock().unwrap().clear();
}

pub fn allowed_hosts() -> &'static [&'static str] {
    ALLOWED_HOSTS
}

fn check_url(url: &reqwest::Url) -> AppResult<()> {
    let host = url.host_str().unwrap_or("");
    let friends_http = url.scheme() == "http" && FRIENDS_HTTP.contains(&host);
    if url.scheme() != "https" && !friends_http {
        return Err(AppError::new("NET_BLOCKED", format!("Заблокирован небезопасный протокол: {}", url.scheme())));
    }
    if !ALLOWED_HOSTS.contains(&host) && !FRIENDS_HTTP.contains(&host) {
        return Err(AppError::new("NET_BLOCKED", format!("Хост не в списке разрешённых: {host}")));
    }
    Ok(())
}

pub struct NetResponse {
    pub response: reqwest::Response,
    pub url: reqwest::Url,
}

pub async fn net_request(url: &str, purpose: &str) -> AppResult<NetResponse> {
    let mut current = reqwest::Url::parse(url).map_err(|e| AppError::new("NET_URL", e.to_string()))?;
    for _ in 0..6 {
        check_url(&current)?;
        let sent = client().get(current.clone()).send().await;
        match sent {
            Ok(response) => {
                let status = response.status().as_u16();
                push(json!({
                    "id": next_id(),
                    "time": chrono::Utc::now().to_rfc3339(),
                    "method": "GET",
                    "host": current.host_str().unwrap_or(""),
                    "path": current.path(),
                    "status": status,
                    "bytes": response.content_length(),
                    "purpose": purpose,
                    "error": null,
                }));
                if (300..400).contains(&status) {
                    if let Some(location) = response
                        .headers()
                        .get(reqwest::header::LOCATION)
                        .and_then(|value| value.to_str().ok())
                    {
                        current = current
                            .join(location)
                            .map_err(|e| AppError::new("NET_URL", e.to_string()))?;
                        continue;
                    }
                }
                return Ok(NetResponse { response, url: current });
            }
            Err(error) => {
                push(json!({
                    "id": next_id(),
                    "time": chrono::Utc::now().to_rfc3339(),
                    "method": "GET",
                    "host": current.host_str().unwrap_or(""),
                    "path": current.path(),
                    "status": null,
                    "bytes": null,
                    "purpose": purpose,
                    "error": error.to_string(),
                }));
                return Err(AppError::new(
                    "NET_FAILED",
                    format!("Нет связи с {}: {error}. Проверь интернет и повтори.", current.host_str().unwrap_or("")),
                ));
            }
        }
    }
    Err(AppError::new("NET_REDIRECTS", "Слишком много перенаправлений"))
}

fn http_error(status: reqwest::StatusCode, url: &reqwest::Url) -> AppError {
    AppError::new(
        "HTTP_ERROR",
        format!("HTTP {} от {}", status.as_u16(), url.host_str().unwrap_or("")),
    )
}

pub async fn fetch_json<T: DeserializeOwned>(url: &str, purpose: &str) -> AppResult<T> {
    let NetResponse { response, url } = net_request(url, purpose).await?;
    if !response.status().is_success() {
        return Err(http_error(response.status(), &url));
    }
    let text = response
        .text()
        .await
        .map_err(|e| AppError::new("NET_FAILED", e.to_string()))?;
    serde_json::from_str(&text)
        .map_err(|e| AppError::new("HTTP_JSON", format!("Некорректный ответ {}: {e}", url.host_str().unwrap_or(""))))
}

pub async fn fetch_text(url: &str, purpose: &str) -> AppResult<String> {
    let NetResponse { response, url } = net_request(url, purpose).await?;
    if !response.status().is_success() {
        return Err(http_error(response.status(), &url));
    }
    response.text().await.map_err(|e| AppError::new("NET_FAILED", e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> reqwest::Url {
        reqwest::Url::parse(value).unwrap()
    }

    #[test]
    fn only_allowed_hosts_and_https() {
        assert!(check_url(&url("https://piston-meta.mojang.com/x")).is_ok());
        assert!(check_url(&url("http://piston-meta.mojang.com/x")).is_err());
        assert!(check_url(&url("https://evil.example/x")).is_err());
        assert!(check_url(&url("http://localhost:25913/api")).is_ok());
    }
}

async fn post_raw(
    url: &str,
    body: Option<String>,
    json: Option<&serde_json::Value>,
    bearer: Option<&str>,
    purpose: &str,
) -> AppResult<(u16, String)> {
    let current = reqwest::Url::parse(url).map_err(|e| AppError::new("NET_URL", e.to_string()))?;
    check_url(&current)?;
    let mut request = client().post(current.clone());
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body);
    } else if let Some(json) = json {
        request = request.json(json);
    }
    let response = request.send().await.map_err(|error| {
        AppError::new(
            "NET_FAILED",
            format!("Нет связи с {}: {error}. Проверь интернет и повтори.", current.host_str().unwrap_or("")),
        )
    })?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|e| AppError::new("NET_FAILED", e.to_string()))?;
    push(json!({
        "id": next_id(),
        "time": chrono::Utc::now().to_rfc3339(),
        "method": "POST",
        "host": current.host_str().unwrap_or(""),
        "path": current.path(),
        "status": status,
        "bytes": text.len(),
        "purpose": purpose,
        "error": null,
    }));
    Ok((status, text))
}

pub async fn post_form_raw(url: &str, form: &[(String, String)], purpose: &str) -> AppResult<(u16, String)> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().map(|(key, value)| (key.as_str(), value.as_str())))
        .finish();
    post_raw(url, Some(body), None, None, purpose).await
}

pub async fn post_json_raw(
    url: &str,
    body: &serde_json::Value,
    purpose: &str,
    bearer: Option<&str>,
) -> AppResult<(u16, String)> {
    post_raw(url, None, Some(body), bearer, purpose).await
}

pub async fn get_raw(url: &str, purpose: &str, bearer: Option<&str>) -> AppResult<(u16, String)> {
    let current = reqwest::Url::parse(url).map_err(|e| AppError::new("NET_URL", e.to_string()))?;
    check_url(&current)?;
    let mut request = client().get(current.clone());
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|error| {
        AppError::new(
            "NET_FAILED",
            format!("Нет связи с {}: {error}. Проверь интернет и повтори.", current.host_str().unwrap_or("")),
        )
    })?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|e| AppError::new("NET_FAILED", e.to_string()))?;
    push(json!({
        "id": next_id(),
        "time": chrono::Utc::now().to_rfc3339(),
        "method": "GET",
        "host": current.host_str().unwrap_or(""),
        "path": current.path(),
        "status": status,
        "bytes": text.len(),
        "purpose": purpose,
        "error": null,
    }));
    Ok((status, text))
}

/// Запрос с любым методом и JSON-телом (для сервера друзей: PATCH/DELETE и т.д.). Возвращает (статус, текст).
pub async fn request_json_raw(
    method: &str,
    url: &str,
    body: Option<&serde_json::Value>,
    bearer: Option<&str>,
    purpose: &str,
) -> AppResult<(u16, String)> {
    let current = reqwest::Url::parse(url).map_err(|e| AppError::new("NET_URL", e.to_string()))?;
    check_url(&current)?;
    let verb = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| AppError::new("NET_URL", e.to_string()))?;
    let mut request = client().request(verb, current.clone()).header(reqwest::header::ACCEPT, "application/json");
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send().await.map_err(|error| {
        AppError::new(
            "NET_FAILED",
            format!("Нет связи с {}: {error}. Проверь интернет и повтори.", current.host_str().unwrap_or("")),
        )
    })?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|e| AppError::new("NET_FAILED", e.to_string()))?;
    push(json!({
        "id": next_id(),
        "time": chrono::Utc::now().to_rfc3339(),
        "method": method,
        "host": current.host_str().unwrap_or(""),
        "path": current.path(),
        "status": status,
        "bytes": text.len(),
        "purpose": purpose,
        "error": null,
    }));
    Ok((status, text))
}

/// GET с произвольными заголовками.
pub async fn get_with_headers(
    url: &str,
    purpose: &str,
    headers: &[(&str, &str)],
) -> AppResult<(u16, String)> {
    let current = reqwest::Url::parse(url).map_err(|e| AppError::new("NET_URL", e.to_string()))?;
    check_url(&current)?;
    let mut request = client().get(current.clone());
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.send().await.map_err(|error| {
        AppError::new(
            "NET_FAILED",
            format!(
                "Нет связи с {}: {error}. Проверь интернет и повтори.",
                current.host_str().unwrap_or("")
            ),
        )
    })?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|e| AppError::new("NET_FAILED", e.to_string()))?;
    push(json!({
        "id": next_id(),
        "time": chrono::Utc::now().to_rfc3339(),
        "method": "GET",
        "host": current.host_str().unwrap_or(""),
        "path": current.path(),
        "status": status,
        "bytes": text.len(),
        "purpose": purpose,
        "error": null,
    }));
    Ok((status, text))
}

pub async fn fetch_json_with_headers<T: DeserializeOwned>(
    url: &str,
    purpose: &str,
    headers: &[(&str, &str)],
) -> AppResult<T> {
    let (status, text) = get_with_headers(url, purpose, headers).await?;
    if !(200..300).contains(&status) {
        return Err(AppError::new(
            "HTTP_ERROR",
            format!("HTTP {status} от {}", reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default()),
        ));
    }
    serde_json::from_str(&text).map_err(|e| {
        AppError::new(
            "HTTP_JSON",
            format!(
                "Некорректный ответ {}: {e}",
                reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default()
            ),
        )
    })
}

/// Загрузка скина: multipart-форма `variant` + `file` (Mojang принимает только так).
pub async fn post_skin_raw(
    url: &str,
    bearer: &str,
    variant: &str,
    png: Vec<u8>,
    purpose: &str,
) -> AppResult<(u16, String)> {
    let current = reqwest::Url::parse(url).map_err(|e| AppError::new("NET_URL", e.to_string()))?;
    check_url(&current)?;
    let part = reqwest::multipart::Part::bytes(png)
        .file_name("skin.png")
        .mime_str("image/png")
        .map_err(|e| AppError::new("NET_URL", e.to_string()))?;
    let form = reqwest::multipart::Form::new()
        .text("variant", variant.to_string())
        .part("file", part);
    let response = client()
        .post(current.clone())
        .bearer_auth(bearer)
        .header(reqwest::header::ACCEPT, "application/json")
        .multipart(form)
        .send()
        .await
        .map_err(|error| {
            AppError::new(
                "NET_FAILED",
                format!("Нет связи с {}: {error}. Проверь интернет и повтори.", current.host_str().unwrap_or("")),
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|e| AppError::new("NET_FAILED", e.to_string()))?;
    push(json!({
        "id": next_id(),
        "time": chrono::Utc::now().to_rfc3339(),
        "method": "POST",
        "host": current.host_str().unwrap_or(""),
        "path": current.path(),
        "status": status,
        "bytes": text.len(),
        "purpose": purpose,
        "error": null,
    }));
    Ok((status, text))
}

pub async fn get_bytes(url: &str, purpose: &str, limit: u64) -> AppResult<(String, Vec<u8>)> {
    let NetResponse { response, url } = net_request(url, purpose).await?;
    if !response.status().is_success() {
        return Err(http_error(response.status(), &url));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if let Some(length) = response.content_length() {
        if length > limit {
            return Err(AppError::new("IMAGE_TOO_BIG", "Файл слишком большой"));
        }
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| AppError::new("NET_FAILED", e.to_string()))?;
    if bytes.len() as u64 > limit {
        return Err(AppError::new("IMAGE_TOO_BIG", "Файл слишком большой"));
    }
    Ok((content_type, bytes.to_vec()))
}
