# ChaldLauncher 1.1.0 — переход на Rust (Tauri)

Версия 1.1.0 переписывается с Electron на **Tauri 2 + Rust**. Интерфейс остаётся прежним:
React-приложение из `src/renderer` не переписывается, его просто загружает Rust-хост вместо Electron.
Бэкап состояния до переезда: `../chaldlauncher 1.1.0-backup-2026-11-13`.

## Что где лежит

| Путь | Что это |
|---|---|
| `src/renderer/` | React-интерфейс (общий для Electron и Tauri) |
| `src/renderer/tauri-shim.ts` | мост `window.chald.invoke/on` → Tauri-команды и события |
| `src/main/`, `src/preload/` | старый Electron main/preload — **референс**, в Tauri-сборку не входит |
| `src/shared/` | типы, схемы, i18n |
| `src-tauri/` | новый Rust-бэкенд (Tauri) |
| `src-tauri/src/dispatch.rs` | диспетчер каналов `IpcContract` (`app:info`, `settings:get`, …) |
| `src-tauri/src/instances.rs` | чтение/создание/правка инстансов (`instance.json`, `.minecraft/`) |
| `src-tauri/src/settings.rs` | `settings.json`, дефолты, валидация, `auth:state` |
| `src-tauri/src/paths.rs` | data-dir, shared-папки, защита путей, slug/id |
| `src-tauri/src/mem.rs`, `offline.rs` | ОЗУ, offline-UUID |

## Запуск

```bash
# один раз: Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

cd "chaldlauncher 1.1.0"
npm install            # зависимости фронта + @tauri-apps/api и CLI

npm run dev:tauri      # Vite + Rust-хост (Tauri dev)
npm run build:tauri    # сборка приложения Tauri
```

Отдельно, если нужно:

```bash
npm run dev:web                                  # только Vite на :5173
cargo run --manifest-path src-tauri/Cargo.toml   # только Rust-хост (нужен запущенный Vite)
```

## Как работает мост

`window.chald` в рендерере прежний:

- `window.chald.invoke(channel, arg)` → Tauri-команда `chald_dispatch` → `src-tauri/src/dispatch.rs`;
  ответ — тот же конверт `{ ok: true, data }` / `{ ok: false, error }`, что и у Electron.
- события (`op:progress`, `game:log`, …) Rust шлёт как `chald:event` с `{ channel, payload }`;
  шим разносит их по подписчикам.

Поэтому страницы, стор и `api.ts` менять не пришлось.

## Что уже перенесено

- `app:info`, `window:control`, `settings:get/set`, `system:memory`, `presence:page`, `net:*`
- `auth:*` — офлайн и вход Microsoft (`auth.rs`)
- `instances:*` — list/create/update/duplicate/remove/worlds/select/openFolder/install/cancelInstall
- `versions:minecraft`, `versions:loaders`, `java:detect`; установка Minecraft, Fabric/Quilt/Forge/NeoForge (`installer.rs`, `forge.rs`)
- `game:launch`, `game:stop`, `game:logs` (`game.rs`, `launch_args.rs`)
- `content:list/toggle/remove` (`content.rs`)
- `modrinth:search/project/versions/openPage/install/installed/installPack/cancelPack` (`modrinth.rs`) — в т.ч. установка `.mrpack` сборок (скачивание, план, файлы/overrides, создание инстанса и автоустановка)
- все `friends:*` — друзья, чаты, группы, звонки-сигналы, Chald ID (`friends.rs`); у лицензии сессия создаётся автоматически, без регистрации
- Discord Rich Presence: `presence:page`, статус в игре и меню (`discord.rs`, `presence.rs`)
- `dialog:pickDirectory` (`tauri-plugin-dialog`), `content:import/add/applyArchive` (`archive.rs`): импорт `.jar` и `.zip`, разбор архивов, подтверждение по токену, защита от path traversal, никакой перезаписи
- `net:testDns` (`dns.rs`): системный резолвер, прямые UDP-запросы и DNS-over-HTTPS (RFC 8484)
- `update:check` (`update.rs`): проверка релизов GitHub, выбор установщика под Windows/Linux
- `instances:changeVersion` (`versionchange.rs`): смена версии/загрузчика, бэкап миров, отключение старых модов
- `wardrobe:*` (`wardrobe.rs`): локальные скины, определение classic/slim, скины и плащи аккаунта Microsoft, прогрессивные превью плащей
- `launchers:detect`, `launchers:pickFolder`, `launchers:import`, `launchers:cancel` (`launcher_import.rs`): поиск Prism/PolyMC/MultiMC, CurseForge, официального лаунчера и Modrinth (`profile.json` и современный `app.db`, читается из копии), разбор версий/загрузчиков, копирование файлов с прогрессом/отменой, создание инстансов и запуск установки
- `chaldpack:inspect`, `chaldpack:import`, `chaldpack:export` (`chaldpack.rs`): `.chaldlauncher` — проверка манифеста, импорт (скачивание файлов с CDN Modrinth/CurseForge, распаковка overrides, индексы, создание инстанса и автоустановка) и экспорт (упаковка файлов игры в архив)
- `curseforge:search/project/files/install/installPack/installed/openPage` (`curseforge.rs`): поиск и информация о проектах/файлах CurseForge, установка модов/ресурспаков/шейдеров/датапаков с индексом `.chald-curseforge.json`, установка `.zip` modpack-сборок (manifest.json, загрузка модов, overrides, создание инстанса и автоустановка)
- `update:download`, `update:install` (`update.rs`): фоновое скачивание обновления с `op:progress`, проверка размера/SHA-256, установка (Windows — запуск установщика, Linux AppImage — замена файла и перезапуск), проверка запущенной игры; попытка автоустановки при закрытии
- оверлей (`overlay.rs`, `overlay:*` IPC, события `overlay:mode`/`overlay:startCall`, глобальный хоткей): отдельное прозрачное always-on-top окно (режимы closed/hud/open), переключение, состояние звонка и запуск звонка из основного окна
- `mojang:status`, `mojang:launch`, `mojang:store` (`mojang.rs`): игры Mojang на Windows — обнаружение Bedrock/официального лаунчера (Appx/PowerShell и exe), запуск Bedrock (в т.ч. с добавлением внешнего сервера), запуск Dungeons/Legends через лаунчер, открытие Microsoft Store

Тесты: `cargo test --lib` — 70 проходят; `installer-rs` — 9 проходят.

## Что ещё не перенесено

(пока ничего)

## Совместимость данных

Rust-версия использует ту же папку, что и Electron: `<config>/ChaldLauncher`
(Linux: `~/.config/ChaldLauncher`, Windows: `%APPDATA%\ChaldLauncher`).
`CHALD_PROFILE=test2` даёт отдельный профиль `ChaldLauncher-test2`, как раньше.
Инстансы, настройки и аккаунт читаются существующие — можно тестировать на своих данных.

## Важные отличия от Electron-сборки

- CSP в `tauri.conf.json` пока `null` (для разработки); перед релизом нужно ограничить, как в `src/main/index.ts`.
- Окно без системной рамки (`decorations: false`), управление — через `window:control`.
- Пока не настроены подпись, автообновление и установщик `.chpk` — это отдельный этап.
