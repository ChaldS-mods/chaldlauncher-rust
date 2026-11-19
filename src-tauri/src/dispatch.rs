use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::{json, Value};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

use crate::errors::{fail, ok, AppError, AppResult};
use crate::{archive, chaldpack, content, curseforge, dns, friends, game, installer, launcher_import, modrinth, mojang, net, update, versions, versionchange};
use crate::instances;
use crate::java;
use crate::mem;
use crate::paths;
use crate::state::AppState;

fn platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

fn as_str<'a>(arg: &'a Value, key: &str) -> AppResult<&'a str> {
    arg.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::new("INVALID_ARGS", format!("Некорректное поле {key}")))
}

fn not_implemented(channel: &str) -> AppError {
    AppError::new(
        "NOT_IMPLEMENTED",
        format!("{channel}: эта часть ещё не перенесена на Rust"),
    )
}

fn window_control(app: &tauri::AppHandle, action: &str) -> AppResult<()> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| AppError::new("WINDOW", "Главное окно не создано"))?;
    match action {
        "minimize" => window.minimize()?,
        "maximize" => {
            if window.is_maximized()? {
                window.unmaximize()?;
            } else {
                window.maximize()?;
            }
        }
        "close" => window.close()?,
        _ => return Err(AppError::new("INVALID_ARGS", "Неизвестное действие с окном")),
    }
    Ok(())
}

fn open_instance_folder(state: &AppState, arg: &Value) -> AppResult<()> {
    let id = as_str(arg, "id")?;
    if !paths::is_instance_id(id) {
        return Err(AppError::new("INVALID_ARGS", "Некорректный ID инстанса"));
    }
    let settings = state.settings.get()?;
    let root = paths::instances_root(&state.data_dir, &settings.instances_dir);
    let dir = paths::resolve_inside(&root, &[id])?;
    open::that(&dir)?;
    Ok(())
}

/// All hosts the launcher is allowed to talk to (`src/main/net.ts`).
#[allow(dead_code)]
fn allowed_hosts() -> Vec<&'static str> {
    vec![
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
    ]
}

async fn dispatch(app: &tauri::AppHandle, state: &AppState, channel: &str, arg: Value) -> AppResult<Value> {
    match channel {
        "app:info" => {
            let settings = state.settings.get()?;
            let instances_dir = paths::instances_root(&state.data_dir, &settings.instances_dir);
            Ok(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "platform": platform(),
                "arch": std::env::consts::ARCH,
                "dataDir": state.data_dir.to_string_lossy(),
                "instancesDir": instances_dir.to_string_lossy(),
                "sharedDir": paths::shared_root(&state.data_dir).to_string_lossy(),
                "encryptionAvailable": false,
            }))
        }

        "window:control" => {
            let action = arg
                .as_str()
                .ok_or_else(|| AppError::new("INVALID_ARGS", "Ожидалось действие с окном"))?;
            window_control(app, action)?;
            Ok(Value::Null)
        }

        "settings:get" => Ok(serde_json::to_value(state.settings.get()?)?),
        "settings:set" => {
            let next = state.settings.set(&arg)?;
            state.presence.configure(next.discord_rpc, next.discord_show_instance, &next.language);
            Ok(serde_json::to_value(next)?)
        }

        "auth:state" => Ok(state.auth.state(&state.settings.get()?)),
        "auth:login" => {
            let method = arg.get("method").and_then(Value::as_str).unwrap_or("browser").to_string();
            state.auth.login(&method, app).await?;
            state.settings.set(&json!({ "accountMode": "microsoft" }))?;
            let auth = state.auth.state(&state.settings.get()?);
            game::emit(app, "auth:changed", auth.clone());
            Ok(auth)
        }
        "auth:cancel" => {
            state.auth.cancel();
            Ok(Value::Null)
        }
        "auth:logout" => {
            let auth = state.auth.logout(&state.settings.get()?)?;
            game::emit(app, "auth:changed", auth.clone());
            Ok(auth)
        }
        "auth:setOffline" => {
            let name = as_str(&arg, "name")?.trim().to_string();
            if !paths::is_offline_name(&name) {
                return Err(AppError::new(
                    "OFFLINE_NAME_INVALID",
                    "Ник: 3–16 символов, только A-Z, a-z, 0-9 и «_»",
                ));
            }
            state.settings.set(&json!({ "accountMode": "offline", "offlineName": name }))?;
            let auth = state.auth.state(&state.settings.get()?);
            game::emit(app, "auth:changed", auth.clone());
            Ok(auth)
        }

        "instances:list" => {
            let settings = state.settings.get()?;
            let running: std::collections::HashSet<String> = state.games.running_ids().into_iter().collect();
            Ok(serde_json::to_value(instances::list(&state.data_dir, &settings, &running)?)?)
        }
        "instances:create" => {
            let settings = state.settings.get()?;
            let running: std::collections::HashSet<String> = state.games.running_ids().into_iter().collect();
            Ok(serde_json::to_value(instances::create(&state.data_dir, &settings, &running, &arg)?)?)
        }
        "instances:update" => {
            let settings = state.settings.get()?;
            let running: std::collections::HashSet<String> = state.games.running_ids().into_iter().collect();
            Ok(serde_json::to_value(instances::update(&state.data_dir, &settings, &running, &arg)?)?)
        }
        "instances:duplicate" => {
            let settings = state.settings.get()?;
            let running: std::collections::HashSet<String> = state.games.running_ids().into_iter().collect();
            Ok(serde_json::to_value(instances::duplicate(&state.data_dir, &settings, &running, &arg)?)?)
        }
        "instances:remove" => {
            let settings = state.settings.get()?;
            instances::remove(&state.data_dir, &settings, &arg)?;
            Ok(Value::Null)
        }
        "instances:worlds" => {
            let settings = state.settings.get()?;
            Ok(serde_json::to_value(instances::worlds(&state.data_dir, &settings, &arg)?)?)
        }
        "instances:select" => {
            let id = arg.get("id").cloned().unwrap_or(Value::Null);
            state.settings.set(&json!({ "lastInstanceId": id }))?;
            Ok(Value::Null)
        }
        "instances:openFolder" => {
            open_instance_folder(state, &arg)?;
            Ok(Value::Null)
        }
        "instances:install" => {
            let id = as_str(&arg, "id")?.to_string();
            install_instance(app, state, &id, None).await?;
            Ok(Value::Null)
        }
        "instances:repairContent" => {
            let id = as_str(&arg, "id")?.to_string();
            let app_handle = app.clone();
            tauri::async_runtime::spawn(async move {
                let state = app_handle.state::<AppState>();
                let op_id = format!("repair:{id}");
                game::emit(
                    &app_handle,
                    "op:progress",
                    json!({
                        "opId": op_id,
                        "kind": "install",
                        "instanceId": id,
                        "label": "Проверяю файлы модов и ресурсов…",
                        "fraction": Value::Null,
                        "status": "running",
                        "message": Value::Null,
                    }),
                );
                let settings = match state.settings.get() {
                    Ok(s) => s,
                    Err(e) => {
                        game::emit(
                            &app_handle,
                            "op:progress",
                            json!({
                                "opId": op_id,
                                "kind": "install",
                                "instanceId": id,
                                "label": "Не удалось проверить файлы",
                                "fraction": Value::Null,
                                "status": "error",
                                "message": e.message,
                            }),
                        );
                        return;
                    }
                };
                let mut repaired = 0usize;
                repaired += modrinth::repair_tracked(&state.data_dir, &settings, &app_handle, &id)
                    .await
                    .unwrap_or(0);
                repaired += curseforge::repair_tracked(&state.data_dir, &settings, &app_handle, &id)
                    .await
                    .unwrap_or(0);
                let label = if repaired > 0 {
                    format!("Восстановлено файлов: {repaired}")
                } else {
                    "Отсутствующих файлов модпака не найдено".to_string()
                };
                game::emit(
                    &app_handle,
                    "op:progress",
                    json!({
                        "opId": op_id,
                        "kind": "install",
                        "instanceId": id,
                        "label": label,
                        "fraction": 1.0,
                        "status": "done",
                        "message": Value::Null,
                    }),
                );
            });
            Ok(Value::Null)
        }
        "instances:changeVersion" => {
            let id = as_str(&arg, "id")?.to_string();
            let mc_version = as_str(&arg, "mcVersion")?.to_string();
            let loader = arg
                .get("loader")
                .ok_or_else(|| AppError::new("INVALID_ARGS", "Некорректное поле loader"))?;
            let loader_type = as_str(loader, "type")?;
            if !["vanilla", "fabric", "quilt", "forge", "neoforge"].contains(&loader_type) {
                return Err(AppError::new("INVALID_ARGS", "Неизвестный загрузчик"));
            }
            let loader_version = loader
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_string);
            if loader_type != "vanilla" && loader_version.is_none() {
                return Err(AppError::new("INVALID_ARGS", "Выбери версию загрузчика"));
            }
            let settings = state.settings.get()?;
            let meta = instances::read_meta_at(&state.data_dir, &settings, &id)?;
            let target = versionchange::VersionTarget {
                mc_version: mc_version.clone(),
                loader_type: loader_type.to_string(),
                loader_version: if loader_type == "vanilla" { None } else { loader_version },
            };
            if meta.installed && target.matches(&versionchange::VersionTarget::from_meta(&meta)) {
                return Err(AppError::new(
                    "VERSION_UNCHANGED",
                    "Версия Minecraft и загрузчик те же: менять нечего",
                ));
            }
            let change = VersionChange {
                target,
                backup_saves: arg.get("backupSaves").and_then(Value::as_bool).unwrap_or(false),
                disable_mods: arg.get("disableMods").and_then(Value::as_bool).unwrap_or(false),
                old_mc_version: meta.mc_version.clone(),
                kind: versionchange::kind_after_change(&meta.kind, loader_type),
            };
            install_instance(app, state, &id, Some(change)).await?;
            Ok(Value::Null)
        }
        "instances:cancelInstall" => {
            let id = as_str(&arg, "id")?;
            if let Some(flag) = state.installs.lock().unwrap().get(id).cloned() {
                flag.store(true, Ordering::SeqCst);
            }
            Ok(Value::Null)
        }
        "instances:setCover" => Ok(Value::Null),

        "system:memory" => {
            let (total, free) = mem::system_memory();
            Ok(json!({ "totalMb": total, "freeMb": free }))
        }

        "dialog:pickDirectory" => {
            let title = arg.get("title").and_then(Value::as_str).unwrap_or("Выбери папку");
            let picked = app.dialog().file().set_title(title).blocking_pick_folder();
            Ok(picked
                .and_then(|path| path.into_path().ok())
                .map(|path| json!(path.to_string_lossy()))
                .unwrap_or(Value::Null))
        }
        "presence:page" => {
            state.presence.set_page(arg.as_str().unwrap_or(""))?;
            Ok(Value::Null)
        }

        "update:check" => {
            let settings = state.settings.get()?;
            update::check(
                &state.data_dir,
                &state.updater,
                &settings.update_repo,
                settings.check_updates,
            )
            .await
        }
        "update:download" => {
            let settings = state.settings.get()?;
            update::start_download(
                app.clone(),
                state.data_dir.clone(),
                state.updater.clone(),
                settings.auto_install_updates,
            )?;
            Ok(Value::Null)
        }
        "update:install" => {
            let running = state.games.running_count();
            update::install(app, &state.data_dir, &state.updater, running)?;
            Ok(Value::Null)
        }

        "net:log" => Ok(Value::Array(net::net_log())),
        "net:clear" => {
            net::clear_net_log();
            Ok(Value::Null)
        }
        "net:allowedHosts" => Ok(json!(net::allowed_hosts())),
        "net:testDns" => {
            let mode = arg.get("dnsMode").and_then(Value::as_str).unwrap_or("system");
            let servers = arg.get("dnsServers").and_then(Value::as_str).unwrap_or("");
            let doh_url = arg.get("dnsDohUrl").and_then(Value::as_str).unwrap_or("");
            dns::test_dns(mode, servers, doh_url).await
        }

        "versions:minecraft" => {
            let refresh = arg.get("refresh").and_then(Value::as_bool).unwrap_or(false);
            Ok(serde_json::to_value(versions::minecraft(&state.data_dir, refresh).await?)?)
        }
        "versions:loaders" => {
            let loader = as_str(&arg, "loader")?;
            let mc = as_str(&arg, "mcVersion")?;
            Ok(serde_json::to_value(versions::loader_versions(loader, mc).await?)?)
        }
        "java:detect" => Ok(serde_json::to_value(java::detect(&paths::runtime_dir(&state.data_dir)))?),
        "game:launch" => {
            let id = as_str(&arg, "id")?;
            let settings = state.settings.get()?;
            let auth = state.auth.launch_auth(&settings).await?;
            state.games.launch(app, &state.data_dir, &settings, id, auth)?;
            Ok(Value::Null)
        }
        "game:stop" => {
            state.games.stop(as_str(&arg, "id")?);
            Ok(Value::Null)
        }
        "game:logs" => {
            let id = as_str(&arg, "id")?;
            Ok(json!(state.games.logs(id)))
        }

        "launchers:detect" => {
            let settings = state.settings.get()?;
            let mut pending = state.pending_imports.lock().unwrap();
            let sources = launcher_import::detect_launchers(&state.data_dir, &settings, &mut pending);
            Ok(serde_json::to_value(sources)?)
        }
        "launchers:pickFolder" => {
            let picked = app
                .dialog()
                .file()
                .set_title("Папка лаунчера или инстанса")
                .blocking_pick_folder();
            let Some(path) = picked.and_then(|path| path.into_path().ok()) else {
                return Ok(Value::Null);
            };
            let settings = state.settings.get()?;
            let mut pending = state.pending_imports.lock().unwrap();
            let source = launcher_import::inspect_folder(&path, &state.data_dir, &settings, &mut pending);
            Ok(serde_json::to_value(source)?)
        }
        "launchers:import" => {
            let tokens = arg
                .get("tokens")
                .and_then(Value::as_array)
                .ok_or_else(|| AppError::new("INVALID_ARGS", "Некорректные данные"))?;
            let mut uniq = Vec::new();
            for token in tokens {
                if let Some(token) = token.as_str() {
                    if !uniq.iter().any(|existing: &String| existing == token) {
                        uniq.push(token.to_string());
                    }
                }
            }
            let items = {
                let mut pending = state.pending_imports.lock().unwrap();
                uniq.into_iter()
                    .filter_map(|token| pending.remove(&token))
                    .collect::<Vec<_>>()
            };
            if items.is_empty() {
                return Err(AppError::new(
                    "IMPORT_EXPIRED",
                    "Список устарел или нечего импортировать. Открой импорт заново.",
                ));
            }
            let batch = format!("{:012x}", rand::random::<u64>() & 0x0000_ffff_ffff_ffff);
            let key = format!("import-{batch}");
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            state.import_batches.lock().unwrap().insert(key.clone(), cancel.clone());
            let app_handle = app.clone();
            let batch_for_task = batch.clone();
            tauri::async_runtime::spawn(async move {
                let state = app_handle.state::<AppState>();
                let result = launcher_import::run_import_batch(
                    &app_handle,
                    &state,
                    &key,
                    &batch_for_task,
                    items,
                    cancel.clone(),
                )
                .await;
                state.import_batches.lock().unwrap().remove(&key);
                if let Ok(created) = result {
                    for id in created {
                        let _ = install_instance(&app_handle, &state, &id, None).await;
                    }
                }
            });
            Ok(json!({ "batch": batch }))
        }
        "launchers:cancel" => {
            let batch = as_str(&arg, "batch")?;
            if let Some(flag) = state.import_batches.lock().unwrap().get(&format!("import-{batch}")) {
                flag.store(true, Ordering::SeqCst);
            }
            Ok(Value::Null)
        }
        "chaldpack:inspect" => {
            let picked = app
                .dialog()
                .file()
                .set_title("Импорт инстанса")
                .add_filter("Инстанс ChaldLauncher", &["chaldlauncher"])
                .blocking_pick_file();
            let Some(file) = picked.and_then(|path| path.into_path().ok()) else {
                return Ok(Value::Null);
            };
            let (manifest, summary) = chaldpack::inspect_pack(&file)?;
            let token = format!("{:016x}", rand::random::<u64>());
            {
                let mut pending = state.pending_chaldpacks.lock().unwrap();
                if pending.len() >= 5 {
                    if let Some(key) = pending.keys().next().cloned() {
                        pending.remove(&key);
                    }
                }
                pending.insert(
                    token.clone(),
                    chaldpack::PendingChaldPack { file, manifest },
                );
            }
            Ok(json!({ "token": token, "summary": summary }))
        }
        "chaldpack:import" => {
            let token = as_str(&arg, "token")?.to_string();
            let pending = state
                .pending_chaldpacks
                .lock()
                .unwrap()
                .remove(&token)
                .ok_or_else(|| AppError::new("PACK_EXPIRED", "Файл нужно выбрать заново"))?;
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            state
                .chaldpack_imports
                .lock()
                .unwrap()
                .insert(token.clone(), cancel.clone());
            let app_handle = app.clone();
            tauri::async_runtime::spawn(async move {
                let state = app_handle.state::<AppState>();
                let token_for_run = token.clone();
                let result = chaldpack::run_import(&app_handle, &state, &token_for_run, pending, cancel.clone()).await;
                state.chaldpack_imports.lock().unwrap().remove(&token);
                if let Ok(Some(id)) = result {
                    let _ = install_instance(&app_handle, &state, &id, None).await;
                }
            });
            Ok(Value::Null)
        }
        "chaldpack:export" => {
            let id = as_str(&arg, "id")?.to_string();
            let include_worlds = arg.get("includeWorlds").and_then(Value::as_bool).unwrap_or(false);
            let settings = state.settings.get()?;
            let meta = instances::read_meta_at(&state.data_dir, &settings, &id)?;
            let safe_name: String = meta
                .name
                .chars()
                .map(|c| if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
                .collect::<String>()
                .trim()
                .to_string();
            let default_name = if safe_name.is_empty() { "instance".to_string() } else { safe_name };
            let picked = app
                .dialog()
                .file()
                .set_title("Экспорт инстанса")
                .set_file_name(format!("{default_name}.chaldlauncher"))
                .add_filter("Инстанс ChaldLauncher", &["chaldlauncher"])
                .blocking_save_file();
            let Some(path) = picked.and_then(|path| path.into_path().ok()) else {
                return Ok(Value::Null);
            };
            let path = if path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("chaldlauncher"))
                .unwrap_or(false)
            {
                path
            } else {
                path.with_extension("chaldlauncher")
            };
            let (result, file) = chaldpack::export_instance(app, state, &id, include_worlds, &path).await?;
            Ok(json!({
                "downloads": result.downloads,
                "identified": result.identified,
                "bundledFiles": result.bundled_files,
                "bundledBytes": result.bundled_bytes,
                "notices": result.notices,
                "file": file,
            }))
        }

        c if c.starts_with("friends:") => {
            let settings = state.settings.get()?;
            let account = friends::Account::from_state(&state.auth.state(&settings));
            match state.friends.handle(c, &arg, &account).await {
                Some(result) => result,
                None => Err(not_implemented(c)),
            }
        }

        "wardrobe:list" => state.wardrobe.list(),
        "wardrobe:import" => {
            let picked = app
                .dialog()
                .file()
                .set_title("Добавить скины")
                .add_filter("Скины (PNG)", &["png"])
                .blocking_pick_files();
            let Some(files) = picked else {
                return Ok(json!({ "added": [], "problems": [] }));
            };
            let paths: Vec<String> = files
                .into_iter()
                .filter_map(|path| path.into_path().ok())
                .map(|path| path.to_string_lossy().to_string())
                .collect();
            state.wardrobe.import_files(&paths)
        }
        "wardrobe:update" => {
            let patch = arg
                .get("patch")
                .cloned()
                .ok_or_else(|| AppError::new("INVALID_ARGS", "Некорректное поле patch"))?;
            state.wardrobe.update(as_str(&arg, "id")?, &patch)
        }
        "wardrobe:remove" => {
            if !arg.get("confirm").and_then(Value::as_bool).unwrap_or(false) {
                return Err(AppError::new("CONFIRM_REQUIRED", "Удаление нужно подтвердить"));
            }
            state.wardrobe.remove(as_str(&arg, "id")?)?;
            Ok(Value::Null)
        }
        "wardrobe:current" => {
            let settings = state.settings.get()?;
            let token = state.auth.account_token(&settings).await?;
            state.wardrobe.current(&token).await
        }
        "wardrobe:saveCurrent" => {
            let settings = state.settings.get()?;
            let token = state.auth.account_token(&settings).await?;
            state.wardrobe.save_current(&token, as_str(&arg, "name")?).await
        }
        "wardrobe:apply" => {
            let settings = state.settings.get()?;
            let token = state.auth.account_token(&settings).await?;
            state.wardrobe.apply(&token, as_str(&arg, "id")?).await
        }
        "wardrobe:reset" => {
            let settings = state.settings.get()?;
            let token = state.auth.account_token(&settings).await?;
            state.wardrobe.reset(&token).await?;
            Ok(Value::Null)
        }
        "wardrobe:listCapes" => {
            let settings = state.settings.get()?;
            let token = state.auth.account_token(&settings).await?;
            state.wardrobe.list_capes(&token, app).await
        }
        "wardrobe:equipCape" => {
            let settings = state.settings.get()?;
            let token = state.auth.account_token(&settings).await?;
            state.wardrobe.equip_cape(&token, as_str(&arg, "id")?, app).await
        }
        "wardrobe:unequipCape" => {
            let settings = state.settings.get()?;
            let token = state.auth.account_token(&settings).await?;
            state.wardrobe.unequip_cape(&token, app).await
        }

        "content:list" => {
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, as_str(&arg, "id")?)?;
            Ok(Value::Array(content::list(&game_dir, as_str(&arg, "kind")?)?))
        }
        "content:toggle" => {
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, as_str(&arg, "id")?)?;
            content::toggle(&game_dir, as_str(&arg, "kind")?, as_str(&arg, "file")?)?;
            Ok(Value::Null)
        }
        "content:remove" => {
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, as_str(&arg, "id")?)?;
            let confirm = arg.get("confirm").and_then(Value::as_bool).unwrap_or(false);
            content::remove(&game_dir, as_str(&arg, "kind")?, as_str(&arg, "file")?, confirm)?;
            Ok(Value::Null)
        }
        "content:import" => {
            let id = as_str(&arg, "id")?;
            let kind = as_str(&arg, "kind")?;
            if state.games.running_ids().iter().any(|running| running == id) {
                return Err(AppError::new("BUSY", "Останови игру, чтобы менять файлы"));
            }
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, id)?;
            let ext = if kind == "mods" { "jar" } else { "zip" };
            let title = if kind == "mods" { "Импорт модов" } else { "Импорт ресурспаков" };
            let picked = app
                .dialog()
                .file()
                .set_title(title)
                .add_filter(ext.to_uppercase(), &[ext])
                .blocking_pick_files();
            let Some(files) = picked else { return Ok(json!(0)) };
            let sources: Vec<String> = files
                .into_iter()
                .filter_map(|path| path.into_path().ok())
                .map(|path| path.to_string_lossy().to_string())
                .collect();
            Ok(json!(content::import(&game_dir, kind, &sources)?))
        }
        "content:add" => {
            let id = as_str(&arg, "id")?;
            if state.games.running_ids().iter().any(|running| running == id) {
                return Err(AppError::new("BUSY", "Останови игру, чтобы менять файлы"));
            }
            let given = arg.get("paths").and_then(Value::as_array).map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            });
            let paths = match given {
                Some(paths) => Some(paths),
                None => app
                    .dialog()
                    .file()
                    .set_title("Добавить моды")
                    .add_filter("Моды (.jar) и архивы (.zip)", &["jar", "zip"])
                    .blocking_pick_files()
                    .map(|files| {
                        files
                            .into_iter()
                            .filter_map(|path| path.into_path().ok())
                            .map(|path| path.to_string_lossy().to_string())
                            .collect()
                    }),
            };
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, id)?;
            let mut pending = state.pending_archives.lock().unwrap();
            archive::add_content(&game_dir, paths.as_deref(), &mut pending, id)
        }
        "content:applyArchive" => {
            let id = as_str(&arg, "id")?;
            let token = as_str(&arg, "token")?;
            let mode = as_str(&arg, "mode")?;
            if state.games.running_ids().iter().any(|running| running == id) {
                return Err(AppError::new("BUSY", "Останови игру, чтобы менять файлы"));
            }
            let entry = state.pending_archives.lock().unwrap().remove(token);
            let Some((owner, file)) = entry.filter(|(owner, _)| owner == id) else {
                return Err(AppError::new("ARCHIVE_EXPIRED", "Список устарел. Добавь архив заново."));
            };
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &owner)?;
            archive::apply_archive(&file, &game_dir, mode)
        }
        "content:openFolder" => {
            let id = as_str(&arg, "id")?;
            let kind = as_str(&arg, "kind")?;
            if kind != "mods" && kind != "resourcepacks" {
                return Err(AppError::new("INVALID_ARGS", "Неизвестный тип контента"));
            }
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, id)?;
            let dir = game_dir.join(if kind == "mods" { "mods" } else { "resourcepacks" });
            std::fs::create_dir_all(&dir)?;
            open::that(&dir)?;
            Ok(Value::Null)
        }

        "modrinth:search" => modrinth::search(&arg).await,
        "modrinth:project" => modrinth::project(as_str(&arg, "projectId")?).await,
        "modrinth:versions" => {
            let mc = arg.get("mcVersion").and_then(Value::as_str);
            let loader = arg.get("loader").and_then(Value::as_str);
            modrinth::versions(as_str(&arg, "projectId")?, mc, loader).await
        }
        "modrinth:openPage" => {
            modrinth::open_page(as_str(&arg, "type")?, as_str(&arg, "slug")?)?;
            Ok(Value::Null)
        }
        "modrinth:installed" => {
            let settings = state.settings.get()?;
            let list = modrinth::installed(&state.data_dir, &settings, as_str(&arg, "id")?, as_str(&arg, "kind")?).await?;
            Ok(json!(list))
        }
        "modrinth:install" => {
            let settings = state.settings.get()?;
            let version_id = arg.get("versionId").and_then(Value::as_str);
            let world = arg.get("worldName").and_then(Value::as_str);
            let names = modrinth::install(
                &state.data_dir,
                &settings,
                app,
                as_str(&arg, "id")?,
                as_str(&arg, "projectId")?,
                as_str(&arg, "kind")?,
                version_id,
                world,
            )
            .await?;
            Ok(json!(names))
        }
        "modrinth:installPack" => {
            let project_id = as_str(&arg, "projectId")?.to_string();
            let version_id = arg.get("versionId").and_then(Value::as_str).map(str::to_string);
            {
                let mut packs = state.modrinth_packs.lock().unwrap();
                if packs.contains_key(&project_id) {
                    return Err(AppError::new("ALREADY_INSTALLING", "Эта сборка уже устанавливается"));
                }
                packs.insert(project_id.clone(), Arc::new(std::sync::atomic::AtomicBool::new(false)));
            }
            let cancel = state.modrinth_packs.lock().unwrap().get(&project_id).cloned().unwrap();
            let app_handle = app.clone();
            let pid = project_id.clone();
            tauri::async_runtime::spawn(async move {
                let state = app_handle.state::<AppState>();
                let result = modrinth::install_pack(&app_handle, &state, &pid, version_id.as_deref(), cancel).await;
                state.modrinth_packs.lock().unwrap().remove(&pid);
                if let Ok(Some(id)) = result {
                    let _ = install_instance(&app_handle, &state, &id, None).await;
                }
            });
            Ok(Value::Null)
        }
        "modrinth:cancelPack" => {
            let project_id = as_str(&arg, "projectId")?;
            modrinth::cancel_pack(state, project_id);
            Ok(Value::Null)
        }
        "curseforge:search" => curseforge::search(&arg).await,
        "curseforge:project" => {
            let mod_id = arg
                .get("modId")
                .and_then(Value::as_i64)
                .ok_or_else(|| AppError::new("INVALID_ARGS", "Некорректное поле modId"))?;
            curseforge::project(mod_id).await
        }
        "curseforge:files" => {
            let mod_id = arg
                .get("modId")
                .and_then(Value::as_i64)
                .ok_or_else(|| AppError::new("INVALID_ARGS", "Некорректное поле modId"))?;
            let mc_version = arg.get("mcVersion").and_then(Value::as_str);
            let loader = arg.get("loader").and_then(Value::as_str);
            curseforge::files(mod_id, mc_version, loader).await
        }
        "curseforge:install" => {
            let settings = state.settings.get()?;
            let mod_id = arg
                .get("modId")
                .and_then(Value::as_i64)
                .ok_or_else(|| AppError::new("INVALID_ARGS", "Некорректное поле modId"))?;
            let file_id = arg.get("fileId").and_then(Value::as_i64);
            let world_name = arg.get("worldName").and_then(Value::as_str);
            let name = curseforge::install_file(
                &state.data_dir,
                &settings,
                app,
                as_str(&arg, "id")?,
                mod_id,
                as_str(&arg, "kind")?,
                file_id,
                world_name,
            )
            .await?;
            Ok(json!(name))
        }
        "curseforge:installed" => {
            let settings = state.settings.get()?;
            let ids = curseforge::installed(
                &state.data_dir,
                &settings,
                as_str(&arg, "id")?,
                as_str(&arg, "kind")?,
            )
            .await?;
            Ok(json!(ids))
        }
        "curseforge:openPage" => {
            curseforge::open_page(as_str(&arg, "slug")?)?;
            Ok(Value::Null)
        }
        "curseforge:installPack" => {
            let mod_id = arg
                .get("modId")
                .and_then(Value::as_i64)
                .ok_or_else(|| AppError::new("INVALID_ARGS", "Некорректное поле modId"))?;
            let file_id = arg.get("fileId").and_then(Value::as_i64);
            let key = curseforge::pack_key(mod_id, file_id);
            {
                let mut packs = state.curseforge_packs.lock().unwrap();
                if packs.contains_key(&key) {
                    return Err(AppError::new("ALREADY_INSTALLING", "Эта сборка уже устанавливается"));
                }
                packs.insert(key.clone(), Arc::new(std::sync::atomic::AtomicBool::new(false)));
            }
            let cancel = state
                .curseforge_packs
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .unwrap();
            let app_handle = app.clone();
            let key_for_task = key.clone();
            tauri::async_runtime::spawn(async move {
                let state = app_handle.state::<AppState>();
                let result = curseforge::install_pack(&app_handle, &state, mod_id, file_id, cancel).await;
                state.curseforge_packs.lock().unwrap().remove(&key_for_task);
                if let Ok(Some(id)) = result {
                    let _ = install_instance(&app_handle, &state, &id, None).await;
                }
            });
            Ok(Value::Null)
        }

        "overlay:toggle" => Ok(json!(state.overlay.toggle(app))),
        "overlay:show" => Ok(json!(state.overlay.show_window(app))),
        "overlay:callState" => {
            let phase = arg.get("phase").and_then(Value::as_str).unwrap_or("idle");
            Ok(json!(state.overlay.set_call_phase(app, phase)))
        }
        "overlay:startCall" => {
            let user_id = arg.get("userId").and_then(Value::as_i64).unwrap_or(0);
            let name = arg.get("name").and_then(Value::as_str).unwrap_or("");
            let mc_name = arg.get("mcName").and_then(Value::as_str).unwrap_or("");
            let group_id = arg.get("groupId").and_then(Value::as_i64);
            let member_ids = arg
                .get("memberIds")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_i64())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Ok(json!(state.overlay.start_call(
                app,
                user_id,
                name,
                mc_name,
                group_id,
                member_ids,
            )))
        }

        "mojang:status" => Ok(json!(mojang::status().await?)),
        "mojang:launch" => {
            let game = as_str(&arg, "game")?;
            let server = match arg.get("server") {
                None | Some(Value::Null) => None,
                Some(v) => {
                    let obj = v.as_object().ok_or_else(|| {
                        AppError::new("INVALID_ARGS", "Некорректное поле server")
                    })?;
                    let name = obj
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| AppError::new("INVALID_ARGS", "Некорректное поле server.name"))?;
                    let host = obj
                        .get("host")
                        .and_then(Value::as_str)
                        .ok_or_else(|| AppError::new("INVALID_ARGS", "Некорректное поле server.host"))?;
                    let port = obj
                        .get("port")
                        .and_then(Value::as_i64)
                        .ok_or_else(|| AppError::new("INVALID_ARGS", "Некорректное поле server.port"))?;
                    Some((name.to_string(), host.to_string(), port))
                }
            };
            mojang::launch(game, server).await?;
            Ok(Value::Null)
        }
        "mojang:store" => {
            mojang::open_store(as_str(&arg, "game")?).await?;
            Ok(Value::Null)
        }

        _ => Err(not_implemented(channel)),
    }
}

pub(crate) struct VersionChange {
    target: versionchange::VersionTarget,
    backup_saves: bool,
    disable_mods: bool,
    old_mc_version: String,
    kind: String,
}

pub(crate) async fn install_instance(
    app: &tauri::AppHandle,
    state: &AppState,
    id: &str,
    change: Option<VersionChange>,
) -> AppResult<()> {
    let settings = state.settings.get()?;
    let meta = instances::read_meta_at(&state.data_dir, &settings, id)?;
    if state.installs.lock().unwrap().contains_key(id) {
        return Err(AppError::new("ALREADY_INSTALLING", "Установка уже идёт"));
    }
    if state.games.running_ids().iter().any(|running| running == id) {
        return Err(AppError::new("ALREADY_RUNNING", "Останови игру перед установкой"));
    }
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    state.installs.lock().unwrap().insert(id.to_string(), cancel.clone());
    let target = match &change {
        Some(change) => installer::InstallTarget {
            mc_version: change.target.mc_version.clone(),
            loader_type: change.target.loader_type.clone(),
            loader_version: change.target.loader_version.clone(),
        },
        None => installer::InstallTarget {
            mc_version: meta.mc_version.clone(),
            loader_type: meta.loader.r#type.clone(),
            loader_version: meta.loader.version.clone(),
        },
    };
    let op_id = format!("install:{id}");
    let app_handle = app.clone();
    let instance_id = id.to_string();
    let last_bucket = std::sync::atomic::AtomicI32::new(-1);
    let emit = move |label: &str, fraction: Option<f64>| {
        let bucket = fraction.map(|value| (value * 100.0) as i32).unwrap_or(-1);
        if last_bucket.swap(bucket, Ordering::Relaxed) == bucket {
            return;
        }
        game::emit(
            &app_handle,
            "op:progress",
            json!({
                "opId": op_id,
                "kind": "install",
                "instanceId": instance_id,
                "label": label,
                "fraction": fraction,
                "status": "running",
                "message": null,
            }),
        );
    };
    let result = crate::download::with_tracker(
        app,
        &format!("install:{id}"),
        "install",
        Some(id),
        installer::install_version(&target, &state.data_dir, &emit, &cancel),
    )
    .await;
    state.installs.lock().unwrap().remove(id);
    match result {
        Ok(version_id) => {
            if let Some(change) = &change {
                let game_dir = instances::game_dir_of(&state.data_dir, &settings, id)?;
                if change.backup_saves {
                    emit("Копирую миры в резервную копию…", None);
                    versionchange::backup_saves(&game_dir, &change.old_mc_version)?;
                }
                if change.disable_mods {
                    emit("Отключаю моды старой версии…", None);
                    versionchange::disable_mods(&game_dir)?;
                }
                instances::apply_version(
                    &state.data_dir,
                    &settings,
                    id,
                    &change.target.mc_version,
                    change.target.loader(),
                    &change.kind,
                    &version_id,
                )?;
            } else {
                instances::patch_installed(&state.data_dir, &settings, id, &version_id)?;
            }
            game::emit(
                app,
                "op:progress",
                json!({
                    "opId": format!("install:{id}"),
                    "kind": "install",
                    "instanceId": id,
                    "label": if change.is_some() { "Версия изменена" } else { "Установка завершена" },
                    "fraction": 1.0,
                    "status": "done",
                    "message": null,
                }),
            );
            Ok(())
        }
        Err(error) => {
            let cancelled = cancel.load(Ordering::SeqCst);
            game::emit(
                app,
                "op:progress",
                json!({
                    "opId": format!("install:{id}"),
                    "kind": "install",
                    "instanceId": id,
                    "label": if cancelled { "Установка отменена" } else { "Ошибка установки" },
                    "fraction": Value::Null,
                    "status": if cancelled { "cancelled" } else { "error" },
                    "message": if cancelled { Value::Null } else { json!(error.message) },
                }),
            );
            Ok(())
        }
    }
}

/// `window.chald.invoke(channel, arg)` → `IpcResult` (the same envelope as the Electron `ipc.ts`).
#[tauri::command]
pub async fn chald_dispatch(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    channel: String,
    arg: Option<Value>,
) -> Result<Value, String> {
    Ok(match dispatch(&app, &state, &channel, arg.unwrap_or(Value::Null)).await {
        Ok(data) => ok(data),
        Err(error) => fail(&error),
    })
}
