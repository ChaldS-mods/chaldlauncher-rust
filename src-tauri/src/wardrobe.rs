//! Гардероб: локальные скины, скины и плащи аккаунта Microsoft. Порт `src/main/wardrobe.ts`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::errors::{AppError, AppResult};
use crate::net;

const MAX_SKINS: usize = 100;
const MAX_SKIN_BYTES: usize = 64 * 1024;
const MAX_CAPE_BYTES: u64 = 256 * 1024;
const API: &str = "https://api.minecraftservices.com";
const TEXTURES_HOST: &str = "textures.minecraft.net";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    id: String,
    name: String,
    variant: String,
    added_at: String,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Index {
    version: u32,
    skins: Vec<Stored>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkinEntry {
    pub id: String,
    pub name: String,
    pub variant: String,
    pub added_at: String,
    pub data_url: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkinImportResult {
    pub added: Vec<SkinEntry>,
    pub problems: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSkin {
    pub variant: String,
    pub data_url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountCape {
    id: String,
    alias: String,
    url: String,
    state: String,
    data_url: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapeListResult {
    capes: Vec<AccountCape>,
    active_id: Option<String>,
}

fn data_url(png: &[u8]) -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

fn is_id(id: &str) -> bool {
    id.len() == 12 && id.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn clean_skin_name(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control() && *c != '\u{7f}')
        .collect::<String>()
        .trim()
        .chars()
        .take(32)
        .collect()
}

struct Decoded {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

fn decode_png(png: &[u8]) -> Option<Decoded> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    decoder.set_limits(png::Limits { bytes: 4 * 1024 * 1024 });
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buffer).ok()?;
    let (width, height) = (info.width, info.height);
    let pixels = &buffer[..info.buffer_size()];
    let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
    match info.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(pixels),
        png::ColorType::Rgb => {
            for chunk in pixels.chunks_exact(3) {
                rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for chunk in pixels.chunks_exact(2) {
                rgba.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
        }
        png::ColorType::Grayscale => {
            for value in pixels {
                rgba.extend_from_slice(&[*value, *value, *value, 255]);
            }
        }
        png::ColorType::Indexed => return None,
    }
    Some(Decoded { width, height, rgba })
}

fn column_empty(rgba: &[u8], width: usize, x: usize, y0: usize, y1: usize) -> bool {
    (y0..=y1).all(|y| rgba.get((y * width + x) * 4 + 3).copied().unwrap_or(255) == 0)
}

/// Throws a user-readable error unless this is a PNG skin of an accepted size. Returns the detected model.
pub fn inspect_skin(png: &[u8]) -> AppResult<String> {
    if png.len() > MAX_SKIN_BYTES {
        return Err(AppError::new(
            "SKIN_INVALID",
            format!("Файл слишком большой (больше {} КБ): это не похоже на скин.", MAX_SKIN_BYTES / 1024),
        ));
    }
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    decoder.set_limits(png::Limits { bytes: 4 * 1024 * 1024 });
    let reader = decoder
        .read_info()
        .map_err(|_| AppError::new("SKIN_INVALID", "Это не PNG-файл."))?;
    let info = reader.info();
    if !((info.width == 64 && (info.height == 64 || info.height == 32))) {
        return Err(AppError::new(
            "SKIN_INVALID",
            format!(
                "Размер скина должен быть 64×64 или 64×32, а у файла {}×{}.",
                info.width, info.height
            ),
        ));
    }
    let Some(decoded) = decode_png(png) else {
        return Ok("classic".to_string());
    };
    if decoded.width != 64 || decoded.height != 64 {
        return Ok("classic".to_string());
    }
    let slim = column_empty(&decoded.rgba, 64, 47, 20, 31) && column_empty(&decoded.rgba, 64, 39, 52, 63);
    Ok(if slim { "slim" } else { "classic" }.to_string())
}

fn textures_url(raw: &str) -> AppResult<String> {
    let mut url = url::Url::parse(raw)
        .map_err(|_| AppError::new("SKIN_URL", "Mojang вернул некорректную ссылку на скин"))?;
    if url.host_str() != Some(TEXTURES_HOST) {
        return Err(AppError::new(
            "SKIN_URL",
            "Ссылка на скин ведёт не на сервер Minecraft: не загружаю",
        ));
    }
    let _ = url.set_scheme("https");
    let _ = url.set_port(None);
    Ok(url.to_string())
}

fn api_error(status: u16, text: &str, cape: bool) -> AppError {
    let detail = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| {
            value
                .get("errorMessage")
                .or_else(|| value.get("error"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    if status == 401 || status == 403 {
        return AppError::new(
            "SKIN_AUTH",
            "Mojang не принял сессию. Выйди и снова войди в аккаунт Microsoft.",
        );
    }
    if status == 429 {
        return AppError::new(
            "SKIN_RATE",
            if cape {
                "Mojang ограничивает частоту смены плаща. Подожди минуту и попробуй снова."
            } else {
                "Mojang ограничивает частоту смены скина. Подожди минуту и попробуй снова."
            },
        );
    }
    if status == 400 {
        return AppError::new(
            if cape { "CAPE_REJECTED" } else { "SKIN_REJECTED" },
            if cape {
                format!("Mojang не принял плащ{suffix}. Возможно, аккаунт им не владеет.")
            } else {
                format!("Mojang не принял файл скина{suffix}.")
            },
        );
    }
    AppError::new(
        if cape { "CAPE_HTTP" } else { "SKIN_HTTP" },
        if cape {
            format!("Не удалось сменить плащ (HTTP {status}{suffix}).")
        } else {
            format!("Не удалось сменить скин (HTTP {status}{suffix}).")
        },
    )
}

pub struct WardrobeService {
    dir: PathBuf,
    lock: Mutex<()>,
}

impl WardrobeService {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("wardrobe"),
            lock: Mutex::new(()),
        }
    }

    fn index_file(&self) -> PathBuf {
        self.dir.join("index.json")
    }

    fn png_file(&self, id: &str) -> AppResult<PathBuf> {
        if !is_id(id) {
            return Err(AppError::new("SKIN_ID", "Некорректный идентификатор скина"));
        }
        Ok(self.dir.join(format!("{id}.png")))
    }

    fn read_index(&self) -> Index {
        let Ok(bytes) = fs::read(self.index_file()) else {
            return Index { version: 1, skins: Vec::new() };
        };
        let Ok(index) = serde_json::from_slice::<Index>(&bytes) else {
            return Index { version: 1, skins: Vec::new() };
        };
        Index {
            version: 1,
            skins: index
                .skins
                .into_iter()
                .filter(|skin| {
                    is_id(&skin.id)
                        && (skin.variant == "classic" || skin.variant == "slim")
                        && !skin.name.is_empty()
                })
                .collect(),
        }
    }

    fn write_index(&self, index: &Index) -> AppResult<()> {
        fs::create_dir_all(&self.dir)?;
        let file = self.index_file();
        let tmp = self.dir.join("index.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(index)?)?;
        fs::rename(tmp, file)?;
        Ok(())
    }

    fn view(&self, stored: &Stored) -> Option<SkinEntry> {
        let png = fs::read(self.png_file(&stored.id).ok()?).ok()?;
        Some(SkinEntry {
            id: stored.id.clone(),
            name: stored.name.clone(),
            variant: stored.variant.clone(),
            added_at: stored.added_at.clone(),
            data_url: data_url(&png),
        })
    }

    pub fn list(&self) -> AppResult<Value> {
        let index = self.read_index();
        let mut views: Vec<SkinEntry> = index.skins.iter().filter_map(|skin| self.view(skin)).collect();
        views.sort_by(|a, b| b.added_at.cmp(&a.added_at));
        Ok(serde_json::to_value(views)?)
    }

    fn add_png(&self, png: &[u8], name: &str, variant: Option<&str>) -> AppResult<(SkinEntry, bool)> {
        let _guard = self.lock.lock().unwrap();
        let detected = inspect_skin(png)?;
        let sha256 = format!("{:x}", Sha256::digest(png));
        let mut index = self.read_index();
        if let Some(same) = index.skins.iter().find(|skin| skin.sha256 == sha256) {
            if let Some(view) = self.view(same) {
                return Ok((view, true));
            }
        }
        if index.skins.len() >= MAX_SKINS {
            return Err(AppError::new(
                "SKIN_LIMIT",
                format!("В гардеробе уже {MAX_SKINS} скинов. Удали ненужные."),
            ));
        }
        let mut id = format!("{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff);
        while self.png_file(&id)?.exists() {
            id = format!("{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff);
        }
        fs::create_dir_all(&self.dir)?;
        fs::write(self.png_file(&id)?, png)?;
        let clean = clean_skin_name(name);
        let stored = Stored {
            id: id.clone(),
            name: if clean.is_empty() { "Скин".to_string() } else { clean },
            variant: variant.unwrap_or(&detected).to_string(),
            added_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            sha256,
        };
        let entry = SkinEntry {
            id,
            name: stored.name.clone(),
            variant: stored.variant.clone(),
            added_at: stored.added_at.clone(),
            data_url: data_url(png),
        };
        index.skins.push(stored);
        self.write_index(&index)?;
        Ok((entry, false))
    }

    pub fn import_files(&self, files: &[String]) -> AppResult<Value> {
        let mut added = Vec::new();
        let mut problems = Vec::new();
        for file in files {
            let path = Path::new(file);
            let base = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| file.clone());
            let result = (|| -> AppResult<(SkinEntry, bool)> {
                let meta = fs::metadata(path)?;
                if meta.len() as usize > MAX_SKIN_BYTES {
                    return Err(AppError::new(
                        "SKIN_INVALID",
                        format!(
                            "Файл слишком большой (больше {} КБ): это не похоже на скин.",
                            MAX_SKIN_BYTES / 1024
                        ),
                    ));
                }
                let png = fs::read(path)?;
                let name = base
                    .strip_suffix(".png")
                    .or_else(|| base.strip_suffix(".PNG"))
                    .unwrap_or(&base);
                self.add_png(&png, name, None)
            })();
            match result {
                Ok((entry, true)) => {
                    problems.push(format!("«{base}»: такой скин уже есть в гардеробе («{}»)", entry.name))
                }
                Ok((entry, false)) => added.push(entry),
                Err(error) => problems.push(format!("«{base}»: {}", error.message)),
            }
        }
        Ok(json!({ "added": added, "problems": problems }))
    }

    pub fn update(&self, id: &str, patch: &Value) -> AppResult<Value> {
        let _guard = self.lock.lock().unwrap();
        self.png_file(id)?;
        let mut index = self.read_index();
        let skin = index
            .skins
            .iter_mut()
            .find(|skin| skin.id == id)
            .ok_or_else(|| AppError::new("SKIN_NOT_FOUND", "Скин не найден"))?;
        if let Some(name) = patch.get("name").and_then(Value::as_str) {
            let clean = clean_skin_name(name);
            if clean.is_empty() {
                return Err(AppError::new("SKIN_NAME", "Введи название скина"));
            }
            skin.name = clean;
        }
        if let Some(variant) = patch.get("variant").and_then(Value::as_str) {
            if !["classic", "slim"].contains(&variant) {
                return Err(AppError::new("INVALID_ARGS", "Неизвестный вариант скина"));
            }
            skin.variant = variant.to_string();
        }
        let stored = skin.clone();
        self.write_index(&index)?;
        self.view(&stored)
            .map(|view| serde_json::to_value(view).unwrap_or(Value::Null))
            .ok_or_else(|| AppError::new("SKIN_NOT_FOUND", "Файл скина пропал с диска"))
    }

    pub fn remove(&self, id: &str) -> AppResult<()> {
        let _guard = self.lock.lock().unwrap();
        let file = self.png_file(id)?;
        let mut index = self.read_index();
        index.skins.retain(|skin| skin.id != id);
        self.write_index(&index)?;
        let _ = fs::remove_file(file);
        Ok(())
    }

    async fn fetch_profile(&self, token: &str) -> AppResult<Value> {
        let (status, text) = net::get_raw(
            &format!("{API}/minecraft/profile"),
            "Гардероб: профиль Minecraft",
            Some(token),
        )
        .await?;
        if !(200..300).contains(&status) {
            return Err(api_error(status, &text, false));
        }
        serde_json::from_str(&text).map_err(|error| AppError::new("JSON", error.to_string()))
    }

    fn pick_active_skin(profile: &Value) -> Option<(String, String)> {
        let skins = profile.get("skins")?.as_array()?;
        let skin = skins.iter().find(|skin| {
            skin.get("state").and_then(Value::as_str) == Some("ACTIVE")
                && skin.get("url").and_then(Value::as_str).is_some()
        })?;
        Some((
            skin.get("url")?.as_str()?.to_string(),
            if skin.get("variant").and_then(Value::as_str) == Some("SLIM") {
                "slim".to_string()
            } else {
                "classic".to_string()
            },
        ))
    }

    async fn download_current(&self, token: &str) -> AppResult<Option<(Vec<u8>, String)>> {
        let profile = self.fetch_profile(token).await?;
        let Some((url, variant)) = Self::pick_active_skin(&profile) else {
            return Ok(None);
        };
        let (_, png) = net::get_bytes(
            &textures_url(&url)?,
            "Гардероб: текущий скин аккаунта",
            MAX_SKIN_BYTES as u64,
        )
        .await?;
        inspect_skin(&png)?;
        Ok(Some((png, variant)))
    }

    pub async fn current(&self, token: &str) -> AppResult<Value> {
        Ok(match self.download_current(token).await? {
            Some((png, variant)) => json!({ "variant": variant, "dataUrl": data_url(&png) }),
            None => Value::Null,
        })
    }

    pub async fn save_current(&self, token: &str, name: &str) -> AppResult<Value> {
        let Some((png, variant)) = self.download_current(token).await? else {
            return Err(AppError::new(
                "SKIN_NONE",
                "На аккаунте нет собственного скина: нечего сохранять.",
            ));
        };
        let (entry, duplicate) = self.add_png(&png, name, Some(&variant))?;
        Ok(if duplicate {
            json!({ "added": [], "problems": [format!("Такой скин уже есть в гардеробе («{}»)", entry.name)] })
        } else {
            json!({ "added": [entry], "problems": [] })
        })
    }

    pub async fn apply(&self, token: &str, id: &str) -> AppResult<Value> {
        let stored = self
            .read_index()
            .skins
            .into_iter()
            .find(|skin| skin.id == id)
            .ok_or_else(|| AppError::new("SKIN_NOT_FOUND", "Скин не найден"))?;
        let png = fs::read(self.png_file(id)?)
            .map_err(|_| AppError::new("SKIN_NOT_FOUND", "Файл скина пропал с диска"))?;
        let (status, text) = net::post_skin_raw(
            &format!("{API}/minecraft/profile/skins"),
            token,
            &stored.variant,
            png.clone(),
            "Гардероб: смена скина",
        )
        .await?;
        if !(200..300).contains(&status) {
            return Err(api_error(status, &text, false));
        }
        Ok(json!({ "variant": stored.variant, "dataUrl": data_url(&png) }))
    }

    pub async fn reset(&self, token: &str) -> AppResult<()> {
        let (status, text) = net::request_json_raw(
            "DELETE",
            &format!("{API}/minecraft/profile/skins/active"),
            None,
            Some(token),
            "Гардероб: сброс скина",
        )
        .await?;
        if !(200..300).contains(&status) {
            return Err(api_error(status, &text, false));
        }
        Ok(())
    }

    fn pick_capes(profile: &Value) -> Vec<(String, String, String, String)> {
        let Some(capes) = profile.get("capes").and_then(Value::as_array) else {
            return Vec::new();
        };
        capes
            .iter()
            .filter_map(|cape| {
                let id = cape.get("id")?.as_str()?.to_string();
                let url = cape.get("url")?.as_str()?.to_string();
                let alias = cape
                    .get("alias")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|alias| !alias.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| id.chars().take(8).collect());
                let state = if cape.get("state").and_then(Value::as_str) == Some("ACTIVE") {
                    "ACTIVE"
                } else {
                    "INACTIVE"
                };
                Some((id, alias, url, state.to_string()))
            })
            .collect()
    }

    /// Returns the list immediately with empty previews; textures load in the background.
    pub async fn list_capes(&self, token: &str, app: &tauri::AppHandle) -> AppResult<Value> {
        let profile = self.fetch_profile(token).await?;
        let mut capes = Vec::new();
        for (id, alias, raw_url, state) in Self::pick_capes(&profile) {
            let url = textures_url(&raw_url)?;
            capes.push(AccountCape {
                id,
                alias,
                url,
                state,
                data_url: String::new(),
            });
        }
        let active_id = capes
            .iter()
            .find(|cape| cape.state == "ACTIVE")
            .map(|cape| cape.id.clone());

        for cape in &capes {
            let app = app.clone();
            let id = cape.id.clone();
            let url = cape.url.clone();
            tokio::spawn(async move {
                if let Ok((_, png)) = net::get_bytes(&url, "Гардероб: превью плаща", MAX_CAPE_BYTES).await {
                    crate::game::emit(
                        &app,
                        "wardrobe:capePreview",
                        json!({ "id": id, "dataUrl": data_url(&png) }),
                    );
                }
            });
        }

        Ok(json!({ "capes": capes, "activeId": active_id }))
    }

    pub async fn equip_cape(&self, token: &str, id: &str, app: &tauri::AppHandle) -> AppResult<Value> {
        if id.is_empty() || id.len() > 64 {
            return Err(AppError::new("CAPE_ID", "Некорректный идентификатор плаща"));
        }
        let (status, text) = net::request_json_raw(
            "PUT",
            &format!("{API}/minecraft/profile/capes/active"),
            Some(&json!({ "capeId": id })),
            Some(token),
            "Гардероб: надеть плащ",
        )
        .await?;
        if !(200..300).contains(&status) {
            return Err(api_error(status, &text, true));
        }
        self.list_capes(token, app).await
    }

    pub async fn unequip_cape(&self, token: &str, app: &tauri::AppHandle) -> AppResult<Value> {
        let (status, text) = net::request_json_raw(
            "DELETE",
            &format!("{API}/minecraft/profile/capes/active"),
            None,
            Some(token),
            "Гардероб: снять плащ",
        )
        .await?;
        if !(200..300).contains(&status) {
            return Err(api_error(status, &text, true));
        }
        self.list_capes(token, app).await
    }

    /// Picks a skin from the wardrobe for `apply` (public lookup used by tests / dispatch).
    #[allow(dead_code)]
    pub fn stored_variant(&self, id: &str) -> Option<String> {
        self.read_index()
            .skins
            .into_iter()
            .find(|skin| skin.id == id)
            .map(|skin| skin.variant)
    }
}

/// Skin ids are exactly 12 hex characters, so they can never contain a path separator.
#[allow(dead_code)]
pub fn skin_id_is_valid(id: &str) -> bool {
    is_id(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_64(alpha_column: Option<usize>) -> Vec<u8> {
        let mut rgba = vec![255u8; 64 * 64 * 4];
        if let Some(column) = alpha_column {
            for y in 20..=31 {
                rgba[(y * 64 + column) * 4 + 3] = 0;
            }
            for y in 52..=63 {
                rgba[(y * 64 + column - 8) * 4 + 3] = 0;
            }
        }
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, 64, 64);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&rgba).unwrap();
        }
        out
    }

    #[test]
    fn names_are_cleaned_and_limited() {
        assert_eq!(clean_skin_name("  my skin \n"), "my skin");
        assert_eq!(clean_skin_name("a\u{0}b").len(), 2);
        assert_eq!(clean_skin_name(&"x".repeat(50)).len(), 32);
    }

    #[test]
    fn detects_sizes_and_slim_arms() {
        let classic = png_64(None);
        assert_eq!(inspect_skin(&classic).unwrap(), "classic");
        let slim = png_64(Some(47));
        assert_eq!(inspect_skin(&slim).unwrap(), "slim");
        assert!(inspect_skin(b"not a png").is_err());
    }

    #[test]
    fn textures_are_only_from_minecraft() {
        assert_eq!(
            textures_url("http://textures.minecraft.net/texture/abc").unwrap(),
            "https://textures.minecraft.net/texture/abc"
        );
        assert!(textures_url("https://evil.example/texture").is_err());
        assert!(textures_url("not a url").is_err());
    }

    #[test]
    fn wardrobe_adds_dedupes_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let service = WardrobeService::new(dir.path());
        let png = png_64(None);
        let (entry, duplicate) = service.add_png(&png, "Test", None).unwrap();
        assert!(!duplicate);
        assert!(is_id(&entry.id));
        let (same, duplicate) = service.add_png(&png, "Again", None).unwrap();
        assert!(duplicate);
        assert_eq!(same.id, entry.id);
        assert_eq!(service.list().unwrap().as_array().unwrap().len(), 1);
        service.update(&entry.id, &json!({ "name": "New", "variant": "slim" })).unwrap();
        assert_eq!(service.stored_variant(&entry.id).unwrap(), "slim");
        service.remove(&entry.id).unwrap();
        assert!(service.list().unwrap().as_array().unwrap().is_empty());
    }

    #[test]
    fn rejects_bad_ids() {
        let dir = tempfile::tempdir().unwrap();
        let service = WardrobeService::new(dir.path());
        assert!(service.remove("../evil").is_err());
        assert!(!skin_id_is_valid("../evil"));
        assert!(skin_id_is_valid("aabbccddeeff"));
    }
}
