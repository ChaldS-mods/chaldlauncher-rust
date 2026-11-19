use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use futures_util::stream::{self, StreamExt};
use futures_util::pin_mut;
use serde::Deserialize;

use crate::download::{download_file, is_file_valid, DownloadSpec};
use crate::errors::{AppError, AppResult};
use crate::paths;
use crate::version_json::{self, current_platform, resolve_libraries, RuleContext, VersionJson};
use crate::versions;

pub struct InstallTarget {
    pub mc_version: String,
    pub loader_type: String,
    pub loader_version: Option<String>,
}

pub type Reporter<'a> = &'a (dyn Fn(&str, Option<f64>) + Sync);

/// Installs Minecraft and, if requested, the loader profile. Returns the version id used for launch.
pub async fn install_version(
    target: &InstallTarget,
    data: &Path,
    report: Reporter<'_>,
    cancel: &AtomicBool,
) -> AppResult<String> {
    match target.loader_type.as_str() {
        "vanilla" => {
            install_vanilla(&target.mc_version, data, report, cancel).await?;
            Ok(target.mc_version.clone())
        }
        "fabric" | "quilt" => {
            let loader_version = target
                .loader_version
                .clone()
                .ok_or_else(|| AppError::new("LOADER_INCOMPATIBLE", "Не выбрана версия загрузчика"))?;
            report("Проверка загрузчика…", None);
            versions::assert_loader_compatible(&target.loader_type, &target.mc_version, &loader_version).await?;
            install_vanilla(&target.mc_version, data, report, cancel).await?;

            report("Загрузка профиля загрузчика…", Some(0.95));
            let profile_url =
                versions::profile_url(&target.loader_type, &target.mc_version, &loader_version);
            let profile: VersionJson = crate::net::fetch_json(&profile_url, "Профиль загрузчика").await?;
            if profile.id.is_empty() {
                return Err(AppError::new("LOADER_INCOMPATIBLE", "Загрузчик вернул профиль без ID"));
            }
            let dir = paths::versions_dir(data).join(&profile.id);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join(format!("{}.json", profile.id)),
                serde_json::to_vec_pretty(&profile)?,
            )?;
            install_libraries(&profile, data, report, cancel, 0.96, 0.99).await?;
            Ok(profile.id)
        }
        "forge" | "neoforge" => {
            let loader_version = target
                .loader_version
                .clone()
                .ok_or_else(|| AppError::new("LOADER_INCOMPATIBLE", "Не выбрана версия загрузчика"))?;
            report("Проверка загрузчика…", None);
            versions::assert_loader_compatible(&target.loader_type, &target.mc_version, &loader_version).await?;
            install_vanilla(&target.mc_version, data, report, cancel).await?;
            let vanilla = version_json::load_resolved_version(data, &target.mc_version)?;
            let required = vanilla
                .java_version
                .as_ref()
                .and_then(|java| java.major_version)
                .unwrap_or(8);
            let java_path = crate::java::ensure_java("", required, &paths::runtime_dir(data))?;
            let version_id = crate::forge::install_forge_like(
                &target.loader_type,
                &target.mc_version,
                &loader_version,
                data,
                &java_path,
                report,
                cancel,
            )
            .await?;
            let resolved = version_json::load_resolved_version(data, &version_id)?;
            install_libraries(&resolved, data, report, cancel, 0.99, 1.0).await?;
            Ok(version_id)
        }
        other => Err(AppError::new(
            "LOADER_NOT_IMPLEMENTED",
            format!("{other}: установка этого загрузчика ещё не перенесена на Rust"),
        )),
    }
}

async fn install_vanilla(
    mc_version: &str,
    data: &Path,
    report: Reporter<'_>,
    cancel: &AtomicBool,
) -> AppResult<()> {
    if cancel.load(Ordering::SeqCst) {
        return Err(AppError::new("CANCELLED", "Установка отменена"));
    }
    report("Получение метаданных версии…", Some(0.01));
    let entry = versions::find(data, mc_version).await?;
    let version_dir = paths::versions_dir(data).join(&entry.id);
    std::fs::create_dir_all(&version_dir)?;
    let json_path = version_dir.join(format!("{}.json", entry.id));
    if !is_file_valid(&json_path, None, Some(&entry.sha1), None) {
        download_file(
            &DownloadSpec {
                url: entry.url.clone(),
                dest: json_path.clone(),
                sha1: Some(entry.sha1.clone()),
                sha256: None,
                size: None,
                purpose: format!("Описание Minecraft {}", entry.id),
            },
            |_| {},
        )
        .await?;
    }
    let json: VersionJson = serde_json::from_str(&std::fs::read_to_string(&json_path)?)?;

    report("Скачивание клиента Minecraft…", Some(0.05));
    if let Some(client) = json.downloads.as_ref().and_then(|downloads| downloads.client.as_ref()) {
        let jar = version_dir.join(format!("{}.jar", entry.id));
        if !is_file_valid(&jar, client.size, client.sha1.as_deref(), None) {
            download_file(
                &DownloadSpec {
                    url: client.url.clone(),
                    dest: jar,
                    sha1: client.sha1.clone(),
                    sha256: None,
                    size: client.size,
                    purpose: format!("Клиент Minecraft {}", entry.id),
                },
                |_| {},
            )
            .await?;
        }
    }

    report("Скачивание библиотек…", Some(0.10));
    install_libraries(&json, data, report, cancel, 0.10, 0.30).await?;

    report("Скачивание ресурсов игры…", Some(0.30));
    install_assets(&json, data, report, cancel, 0.30, 0.95).await?;
    report("Установка завершена", Some(1.0));
    Ok(())
}

async fn install_libraries(
    json: &VersionJson,
    data: &Path,
    report: Reporter<'_>,
    cancel: &AtomicBool,
    from: f64,
    to: f64,
) -> AppResult<()> {
    let ctx = RuleContext { platform: current_platform(), features: HashMap::new() };
    let libraries = resolve_libraries(json, &ctx)?;
    let libraries_dir = paths::libraries_dir(data);
    let jobs: Vec<DownloadSpec> = libraries
        .into_iter()
        .filter_map(|library| {
            if library.url.is_empty() {
                return None;
            }
            Some(DownloadSpec {
                url: library.url,
                dest: libraries_dir.join(library.path.replace('/', std::path::MAIN_SEPARATOR_STR)),
                sha1: library.sha1,
                sha256: None,
                size: library.size,
                purpose: format!("Библиотека {}", library.name),
            })
        })
        .collect();
    run_jobs(jobs, report, cancel, "Библиотеки", from, to).await
}

#[derive(Debug, Deserialize)]
struct AssetIndexFile {
    #[serde(default)]
    objects: HashMap<String, AssetObject>,
}

#[derive(Debug, Deserialize)]
struct AssetObject {
    hash: String,
    size: u64,
}

async fn install_assets(
    json: &VersionJson,
    data: &Path,
    report: Reporter<'_>,
    cancel: &AtomicBool,
    from: f64,
    to: f64,
) -> AppResult<()> {
    let Some(index) = json.asset_index.as_ref() else { return Ok(()) };
    let assets_dir = paths::assets_dir(data);
    let index_path = assets_dir.join("indexes").join(format!("{}.json", index.id));
    if !is_file_valid(&index_path, index.size, index.sha1.as_deref(), None) {
        download_file(
            &DownloadSpec {
                url: index.url.clone(),
                dest: index_path.clone(),
                sha1: index.sha1.clone(),
                sha256: None,
                size: index.size,
                purpose: format!("Индекс ресурсов {}", index.id),
            },
            |_| {},
        )
        .await?;
    }
    let parsed: AssetIndexFile = serde_json::from_str(&std::fs::read_to_string(&index_path)?)?;
    let objects_dir = assets_dir.join("objects");
    let jobs: Vec<DownloadSpec> = parsed
        .objects
        .values()
        .filter_map(|object| {
            if object.hash.len() < 2 {
                return None;
            }
            let prefix = &object.hash[..2];
            Some(DownloadSpec {
                url: format!("https://resources.download.minecraft.net/{prefix}/{}", object.hash),
                dest: objects_dir.join(prefix).join(&object.hash),
                sha1: Some(object.hash.clone()),
                sha256: None,
                size: Some(object.size),
                purpose: "Ресурсы игры".to_string(),
            })
        })
        .collect();
    run_jobs(jobs, report, cancel, "Ресурсы игры", from, to).await
}

async fn run_jobs(
    jobs: Vec<DownloadSpec>,
    report: Reporter<'_>,
    cancel: &AtomicBool,
    label: &str,
    from: f64,
    to: f64,
) -> AppResult<()> {
    let total = jobs.len();
    if total == 0 {
        return Ok(());
    }
    crate::download::expect_files(total as u64, jobs.iter().map(|job| job.size.unwrap_or(0)).sum());
    let done = Arc::new(AtomicUsize::new(0));
    let stream = stream::iter(jobs.into_iter().map(|spec| {
        let done = done.clone();
        async move {
            if cancel.load(Ordering::SeqCst) {
                return Err(AppError::new("CANCELLED", "Установка отменена"));
            }
            download_file(&spec, |_| {}).await?;
            let current = done.fetch_add(1, Ordering::SeqCst) + 1;
            report(
                &format!("{label} ({current}/{total})…"),
                Some(from + (to - from) * (current as f64 / total as f64)),
            );
            Ok::<(), AppError>(())
        }
    }))
    .buffer_unordered(16);
    pin_mut!(stream);
    while let Some(result) = stream.next().await {
        result?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_urls_are_built_from_the_hash() {
        let object = AssetObject { hash: "0123456789abcdef0123456789abcdef01234567".to_string(), size: 10 };
        let prefix = &object.hash[..2];
        assert_eq!(prefix, "01");
        assert_eq!(
            format!("https://resources.download.minecraft.net/{prefix}/{}", object.hash),
            "https://resources.download.minecraft.net/01/0123456789abcdef0123456789abcdef01234567"
        );
    }
}
