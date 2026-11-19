use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::{AppError, AppResult};
use crate::net::{fetch_json, fetch_text};

pub const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestVersion {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    pub sha1: String,
    #[serde(rename = "releaseTime")]
    pub release_time: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Manifest {
    versions: Vec<ManifestVersion>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McVersionEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub release_time: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoaderVersionEntry {
    pub version: String,
    pub stable: bool,
    pub recommended: bool,
}

fn cache_file(data: &Path) -> PathBuf {
    data.join("cache").join("version_manifest.json")
}

pub async fn manifest(data: &Path, refresh: bool) -> AppResult<Manifest> {
    let file = cache_file(data);
    if !refresh {
        if let Ok(text) = std::fs::read_to_string(&file) {
            if let Ok(parsed) = serde_json::from_str::<Manifest>(&text) {
                return Ok(parsed);
            }
        }
    }
    match fetch_json::<Manifest>(MANIFEST_URL, "Список версий Minecraft").await {
        Ok(parsed) => {
            if let Some(parent) = file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(bytes) = serde_json::to_vec(&parsed) {
                let temp = file.with_extension("json.tmp");
                if std::fs::write(&temp, bytes).is_ok() {
                    let _ = std::fs::rename(&temp, &file);
                }
            }
            Ok(parsed)
        }
        Err(error) => {
            if let Ok(text) = std::fs::read_to_string(&file) {
                if let Ok(parsed) = serde_json::from_str::<Manifest>(&text) {
                    return Ok(parsed);
                }
            }
            Err(AppError::new(
                "VERSIONS_UNAVAILABLE",
                format!("Не удалось получить список версий: {error}"),
            ))
        }
    }
}

pub async fn minecraft(data: &Path, refresh: bool) -> AppResult<Vec<McVersionEntry>> {
    let manifest = manifest(data, refresh).await?;
    Ok(manifest
        .versions
        .into_iter()
        .map(|version| McVersionEntry { id: version.id, kind: version.kind, release_time: version.release_time })
        .collect())
}

pub async fn find(data: &Path, id: &str) -> AppResult<ManifestVersion> {
    let manifest = manifest(data, false).await?;
    manifest
        .versions
        .into_iter()
        .find(|version| version.id == id)
        .ok_or_else(|| AppError::new("VERSION_UNKNOWN", format!("Версия Minecraft {id} не найдена в официальном списке")))
}

fn numeric(part: &str) -> Option<u64> {
    part.parse::<u64>().ok()
}

/// Numeric, segment-wise comparison ("52.0.9" < "52.0.40"); a suffix like "-beta" sorts below the plain release.
pub fn compare_versions(a: &str, b: &str) -> i32 {
    let split = |value: &str| -> Vec<String> { value.split(['.', '-']).map(str::to_string).collect() };
    let pa = split(a);
    let pb = split(b);
    let length = pa.len().max(pb.len());
    for index in 0..length {
        match (pa.get(index), pb.get(index)) {
            (None, Some(y)) => return if numeric(y).is_some() { -1 } else { 1 },
            (Some(x), None) => return if numeric(x).is_some() { 1 } else { -1 },
            (Some(x), Some(y)) => match (numeric(x), numeric(y)) {
                (Some(x), Some(y)) if x != y => return if x > y { 1 } else { -1 },
                (Some(_), None) => return 1,
                (None, Some(_)) => return -1,
                (Some(_), Some(_)) => {}
                (None, None) => {
                    let order = x.cmp(y);
                    if order != std::cmp::Ordering::Equal {
                        return if order == std::cmp::Ordering::Greater { 1 } else { -1 };
                    }
                }
            },
            (None, None) => {}
        }
    }
    0
}

pub fn profile_url(loader: &str, mc_version: &str, loader_version: &str) -> String {
    let base = if loader == "quilt" {
        "https://meta.quiltmc.org/v3/versions/loader"
    } else {
        "https://meta.fabricmc.net/v2/versions/loader"
    };
    format!("{base}/{mc_version}/{loader_version}/profile/json")
}

#[derive(Debug, Deserialize)]
struct ProfileEntry {
    loader: ProfileLoader,
}

#[derive(Debug, Deserialize)]
struct ProfileLoader {
    version: String,
    stable: Option<bool>,
}

async fn profile_loaders(loader: &str, mc_version: &str) -> AppResult<Vec<LoaderVersionEntry>> {
    let base = if loader == "quilt" {
        "https://meta.quiltmc.org/v3/versions/loader"
    } else {
        "https://meta.fabricmc.net/v2/versions/loader"
    };
    let list: Vec<ProfileEntry> = fetch_json(&format!("{base}/{mc_version}"), "Версии загрузчика").await?;
    Ok(list
        .into_iter()
        .map(|entry| LoaderVersionEntry {
            stable: entry.loader.stable.unwrap_or_else(|| {
                !entry.loader.version.contains("-alpha")
                    && !entry.loader.version.contains("-beta")
                    && !entry.loader.version.contains("-rc")
                    && !entry.loader.version.contains("-pre")
            }),
            version: entry.loader.version,
            recommended: false,
        })
        .collect())
}

const FORGE_METADATA: &str = "https://maven.minecraftforge.net/net/minecraftforge/forge/maven-metadata.xml";
const FORGE_PROMOS: &str = "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json";
const NEOFORGE_VERSIONS: &str =
    "https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/neoforge";

pub fn forge_supports_mc(mc: &str) -> bool {
    if let Some(rest) = mc.strip_prefix("1.") {
        let minor: u32 = rest.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0);
        return minor >= 13;
    }
    let major: u32 = mc.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0);
    major >= 26
}

pub fn neoforge_prefix(mc: &str) -> Option<String> {
    if let Some(rest) = mc.strip_prefix("1.") {
        let mut parts = rest.split('.');
        let minor: u32 = parts.next()?.parse().ok()?;
        let patch: u32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        return if minor >= 21 || (minor == 20 && patch >= 2) {
            Some(format!("{minor}.{patch}."))
        } else {
            None
        };
    }
    let mut parts = mc.split('.');
    let year: u32 = parts.next()?.parse().ok()?;
    if year >= 26 {
        Some(if mc.split('.').count() >= 3 { format!("{mc}.") } else { format!("{mc}.0.") })
    } else {
        None
    }
}

pub fn parse_forge_metadata(xml: &str, mc: &str) -> Vec<String> {
    let prefix = format!("{mc}-");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<version>") {
        let after = &rest[start + "<version>".len()..];
        let Some(end) = after.find("</version>") else { break };
        let full = after[..end].trim();
        if let Some(version) = full.strip_prefix(&prefix) {
            out.push(version.to_string());
        }
        rest = &after[end + "</version>".len()..];
    }
    out
}

#[derive(Debug, Deserialize)]
struct Promos {
    promos: HashMap<String, String>,
}

async fn forge_loaders(mc_version: &str) -> AppResult<Vec<LoaderVersionEntry>> {
    if !forge_supports_mc(mc_version) {
        return Ok(Vec::new());
    }
    let xml = fetch_text(FORGE_METADATA, "Версии Forge").await?;
    let mut versions = parse_forge_metadata(&xml, mc_version);
    versions.sort_by(|a, b| compare_versions(b, a).cmp(&0));
    let recommended = fetch_json::<Promos>(FORGE_PROMOS, "Рекомендуемые версии Forge")
        .await
        .ok()
        .and_then(|promos| promos.promos.get(&format!("{mc_version}-recommended")).cloned());
    Ok(versions
        .into_iter()
        .map(|version| LoaderVersionEntry {
            recommended: recommended.as_deref() == Some(version.as_str()),
            stable: true,
            version,
        })
        .collect())
}

async fn neoforge_loaders(mc_version: &str) -> AppResult<Vec<LoaderVersionEntry>> {
    let Some(prefix) = neoforge_prefix(mc_version) else { return Ok(Vec::new()) };
    #[derive(Deserialize)]
    struct Response {
        versions: Vec<String>,
    }
    let data: Response = fetch_json(NEOFORGE_VERSIONS, "Версии NeoForge").await?;
    let mut versions: Vec<String> = data.versions.into_iter().filter(|v| v.starts_with(&prefix)).collect();
    versions.sort_by(|a, b| compare_versions(b, a).cmp(&0));
    Ok(versions
        .into_iter()
        .map(|version| {
            let stable = !version.contains("-alpha")
                && !version.contains("-beta")
                && !version.contains("-rc")
                && !version.contains("-snapshot");
            LoaderVersionEntry { version, stable, recommended: false }
        })
        .collect())
}

pub async fn loader_versions(loader: &str, mc_version: &str) -> AppResult<Vec<LoaderVersionEntry>> {
    match loader {
        "fabric" | "quilt" => profile_loaders(loader, mc_version).await,
        "forge" => forge_loaders(mc_version).await,
        "neoforge" => neoforge_loaders(mc_version).await,
        _ => Ok(Vec::new()),
    }
}

/// Throws when the loader (or this exact version of it) does not exist for the Minecraft version.
pub async fn assert_loader_compatible(loader: &str, mc_version: &str, loader_version: &str) -> AppResult<()> {
    let title = match loader {
        "neoforge" => "NeoForge",
        "fabric" => "Fabric",
        "quilt" => "Quilt",
        "forge" => "Forge",
        other => other,
    };
    if loader == "forge" && !forge_supports_mc(mc_version) {
        return Err(AppError::new("LOADER_INCOMPATIBLE", "Forge поддерживается для Minecraft 1.13 и новее"));
    }
    let list = loader_versions(loader, mc_version).await?;
    if list.is_empty() {
        return Err(AppError::new("LOADER_INCOMPATIBLE", format!("{title} не поддерживает Minecraft {mc_version}")));
    }
    if !list.iter().any(|entry| entry.version == loader_version) {
        return Err(AppError::new(
            "LOADER_INCOMPATIBLE",
            format!("{title} {loader_version} не подходит для Minecraft {mc_version}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_numerically() {
        assert!(compare_versions("52.0.40", "52.0.9") > 0);
        assert!(compare_versions("0.16.9", "0.16.10") < 0);
        assert!(compare_versions("21.1.200", "21.1.200") == 0);
        assert!(compare_versions("1.0.0-beta", "1.0.0") < 0);
    }

    #[test]
    fn parses_forge_metadata() {
        let xml = "<metadata><versioning><versions><version>1.21.1-52.0.40</version><version>1.21.1-52.0.9</version><version>1.20.1-47.2.0</version></versions></versioning></metadata>";
        assert_eq!(parse_forge_metadata(xml, "1.21.1"), vec!["52.0.40", "52.0.9"]);
    }

    #[test]
    fn neoforge_prefixes() {
        assert_eq!(neoforge_prefix("1.21.1").as_deref(), Some("21.1."));
        assert_eq!(neoforge_prefix("1.20.1"), None);
        assert_eq!(neoforge_prefix("26.1").as_deref(), Some("26.1.0."));
    }
}
