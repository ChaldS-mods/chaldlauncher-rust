use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::chaldpack::PendingChaldPack;
use crate::launcher_import::PendingImport;

use crate::auth::AuthService;
use crate::friends::FriendsService;
use crate::game::GameManager;
use crate::overlay::OverlayManager;
use crate::paths;
use crate::presence::PresenceService;
use crate::settings::SettingsStore;
use crate::update::UpdaterRuntime;
use crate::wardrobe::WardrobeService;

pub struct AppState {
    pub data_dir: PathBuf,
    pub settings: SettingsStore,
    pub auth: AuthService,
    /// Requests the launcher made (the "Сетевые запросы" tab); the Rust net layer will fill it.
    #[allow(dead_code)]
    pub net_log: Mutex<Vec<Value>>,
    pub games: GameManager,
    pub friends: FriendsService,
    pub wardrobe: WardrobeService,
    pub presence: PresenceService,
    /// Running installations: instance id -> cancel flag.
    pub installs: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Archives waiting for the user's decision: token -> (instance id, file).
    pub pending_archives: Mutex<HashMap<String, (String, PathBuf)>>,
    /// Other launchers' instances found by detect/pickFolder: token -> source data for import.
    pub pending_imports: Mutex<HashMap<String, PendingImport>>,
    /// Running launcher-import batches: "import-<batch>" -> cancel flag.
    pub import_batches: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// `.chaldlauncher` files waiting for confirmation: token -> (file, manifest).
    pub pending_chaldpacks: Mutex<HashMap<String, PendingChaldPack>>,
    /// Running chaldpack imports: token -> cancel flag.
    pub chaldpack_imports: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Running Modrinth modpack installs: project id -> cancel flag.
    pub modrinth_packs: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Running CurseForge modpack installs: pack key -> cancel flag.
    pub curseforge_packs: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Launcher self-update runtime state (check/download/install).
    pub updater: Arc<Mutex<UpdaterRuntime>>,
    /// In-game overlay window/state.
    pub overlay: OverlayManager,
}

impl AppState {
    pub fn new() -> Self {
        let data_dir = paths::data_dir();
        Self {
            settings: SettingsStore::new(&data_dir),
            auth: AuthService::new(&data_dir),
            net_log: Mutex::new(Vec::new()),
            games: GameManager::new(),
            friends: FriendsService::new(&data_dir),
            wardrobe: WardrobeService::new(&data_dir),
            presence: PresenceService::new(),
            data_dir,
            installs: Mutex::new(HashMap::new()),
            pending_archives: Mutex::new(HashMap::new()),
            pending_imports: Mutex::new(HashMap::new()),
            import_batches: Mutex::new(HashMap::new()),
            pending_chaldpacks: Mutex::new(HashMap::new()),
            chaldpack_imports: Mutex::new(HashMap::new()),
            modrinth_packs: Mutex::new(HashMap::new()),
            curseforge_packs: Mutex::new(HashMap::new()),
            updater: Arc::new(Mutex::new(UpdaterRuntime::default())),
            overlay: OverlayManager::new(),
        }
    }
}
