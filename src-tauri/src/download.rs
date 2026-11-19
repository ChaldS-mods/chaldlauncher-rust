use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};
use sha2::Sha256;
use tokio::io::AsyncWriteExt;

use crate::errors::{AppError, AppResult};
use crate::net::net_request;

#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub url: String,
    pub dest: PathBuf,
    pub sha1: Option<String>,
    pub sha256: Option<String>,
    pub size: Option<u64>,
    pub purpose: String,
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn hash_file(path: &Path, algo: &str) -> AppResult<String> {
    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0u8; 64 * 1024];
    if algo == "sha1" {
        let mut hasher = Sha1::new();
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hex(&hasher.finalize()))
    } else {
        let mut hasher = Sha256::new();
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hex(&hasher.finalize()))
    }
}

/// True when the file exists and matches every check we know about.
pub fn is_file_valid(path: &Path, size: Option<u64>, sha1: Option<&str>, sha256: Option<&str>) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else { return false };
    if !metadata.is_file() {
        return false;
    }
    if let Some(size) = size {
        if metadata.len() != size {
            return false;
        }
    }
    if let Some(expected) = sha1 {
        if hash_file(path, "sha1").ok().as_deref() != Some(expected) {
            return false;
        }
    }
    if let Some(expected) = sha256 {
        if hash_file(path, "sha256").ok().as_deref() != Some(expected) {
            return false;
        }
    }
    true
}

fn temp_path(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".part");
    dest.with_file_name(name)
}


// ---------------------------------------------------------------------------------------------
// Per-file progress ("Подробнее" panel).
//
// An operation (install, modpack, ...) wraps its work in `with_tracker`. While that future runs,
// every `download_file` call reports itself to the tracker through a task-local, so the existing
// call sites do not change. The tracker emits throttled `op:files` events.
// ---------------------------------------------------------------------------------------------

const EMIT_EVERY: Duration = Duration::from_millis(150);
const MAX_ACTIVE_SENT: usize = 24;

struct ActiveFile {
    id: u64,
    name: String,
    purpose: String,
    done: u64,
    total: Option<u64>,
}

struct TrackerInner {
    next_id: u64,
    active: Vec<ActiveFile>,
    files_done: u64,
    files_total: u64,
    bytes_done: u64,
    bytes_total: u64,
    /// files already counted in `files_total` by `expect` and not yet started
    expected_left: u64,
    speed: f64,
    last_emit: Instant,
    last_speed_at: Instant,
    last_speed_bytes: u64,
}

pub struct FileTracker {
    app: tauri::AppHandle,
    op_id: String,
    kind: String,
    instance_id: Option<String>,
    inner: Mutex<TrackerInner>,
}

tokio::task_local! {
    static TRACKER: Arc<FileTracker>;
}

impl FileTracker {
    fn new(app: &tauri::AppHandle, op_id: &str, kind: &str, instance_id: Option<&str>) -> Self {
        let now = Instant::now();
        Self {
            app: app.clone(),
            op_id: op_id.to_string(),
            kind: kind.to_string(),
            instance_id: instance_id.map(str::to_string),
            inner: Mutex::new(TrackerInner {
                next_id: 1,
                active: Vec::new(),
                files_done: 0,
                files_total: 0,
                bytes_done: 0,
                bytes_total: 0,
                expected_left: 0,
                speed: 0.0,
                last_emit: now - EMIT_EVERY,
                last_speed_at: now,
                last_speed_bytes: 0,
            }),
        }
    }

    /// Announces a batch of files before they start, so the totals are known up front.
    pub fn expect(&self, files: u64, bytes: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.files_total += files;
        inner.expected_left += files;
        inner.bytes_total += bytes;
        self.emit_locked(&mut inner, true);
    }

    fn begin(&self, spec: &DownloadSpec) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        if inner.expected_left > 0 {
            inner.expected_left -= 1;
        } else {
            inner.files_total += 1;
            inner.bytes_total += spec.size.unwrap_or(0);
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let name = spec
            .dest
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| spec.url.clone());
        inner.active.push(ActiveFile {
            id,
            name,
            purpose: spec.purpose.clone(),
            done: 0,
            total: spec.size,
        });
        self.emit_locked(&mut inner, false);
        id
    }

    /// A file that was already on disk and valid: counts as done without showing up as active.
    fn skipped(&self, size: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.expected_left > 0 {
            inner.expected_left -= 1;
        } else {
            inner.files_total += 1;
            inner.bytes_total += size;
        }
        inner.files_done += 1;
        inner.bytes_done += size;
        self.emit_locked(&mut inner, false);
    }

    fn add_bytes(&self, id: u64, bytes: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.bytes_done += bytes;
        if let Some(file) = inner.active.iter_mut().find(|f| f.id == id) {
            file.done += bytes;
            if file.total.map_or(false, |t| file.done > t) {
                file.total = Some(file.done);
            }
        }
        self.emit_locked(&mut inner, false);
    }

    fn finish(&self, id: u64, ok: bool) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(position) = inner.active.iter().position(|f| f.id == id) {
            let file = inner.active.remove(position);
            if ok {
                inner.files_done += 1;
                // a file of unknown size: bring the byte total in line with what really came
                if file.total.is_none() {
                    inner.bytes_total += file.done;
                }
            } else {
                // a failed download will be retried or abort the operation: take its bytes back
                inner.bytes_done = inner.bytes_done.saturating_sub(file.done);
            }
        }
        self.emit_locked(&mut inner, false);
    }

    fn flush(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.active.clear();
        self.emit_locked(&mut inner, true);
    }

    fn emit_locked(&self, inner: &mut TrackerInner, force: bool) {
        let now = Instant::now();
        if !force && now.duration_since(inner.last_emit) < EMIT_EVERY {
            return;
        }
        let dt = now.duration_since(inner.last_speed_at).as_secs_f64();
        if dt >= 0.25 {
            let instant = inner.bytes_done.saturating_sub(inner.last_speed_bytes) as f64 / dt;
            inner.speed = if inner.speed == 0.0 { instant } else { inner.speed * 0.7 + instant * 0.3 };
            inner.last_speed_at = now;
            inner.last_speed_bytes = inner.bytes_done;
        }
        inner.last_emit = now;
        let active: Vec<serde_json::Value> = inner
            .active
            .iter()
            .take(MAX_ACTIVE_SENT)
            .map(|f| {
                serde_json::json!({
                    "id": f.id,
                    "name": f.name,
                    "purpose": f.purpose,
                    "done": f.done,
                    "total": f.total,
                })
            })
            .collect();
        crate::game::emit(
            &self.app,
            "op:files",
            serde_json::json!({
                "opId": self.op_id,
                "kind": self.kind,
                "instanceId": self.instance_id,
                "filesDone": inner.files_done,
                "filesTotal": inner.files_total,
                "bytesDone": inner.bytes_done,
                "bytesTotal": inner.bytes_total,
                "speed": inner.speed,
                "active": active,
            }),
        );
    }
}

/// Runs `fut` with per-file progress reporting for the operation `op_id`.
pub async fn with_tracker<F, T>(
    app: &tauri::AppHandle,
    op_id: &str,
    kind: &str,
    instance_id: Option<&str>,
    fut: F,
) -> T
where
    F: std::future::Future<Output = T>,
{
    let tracker = Arc::new(FileTracker::new(app, op_id, kind, instance_id));
    let result = TRACKER.scope(tracker.clone(), fut).await;
    tracker.flush();
    result
}

/// Tells the current tracker (if any) about a batch of files that is about to be downloaded.
pub fn expect_files(files: u64, bytes: u64) {
    let _ = TRACKER.try_with(|tracker| tracker.expect(files, bytes));
}

fn current_tracker() -> Option<Arc<FileTracker>> {
    TRACKER.try_with(|tracker| tracker.clone()).ok()
}

pub async fn download_file(spec: &DownloadSpec, mut on_bytes: impl FnMut(u64)) -> AppResult<()> {
    let tracker = current_tracker();
    if is_file_valid(&spec.dest, spec.size, spec.sha1.as_deref(), spec.sha256.as_deref()) {
        if let Ok(metadata) = std::fs::metadata(&spec.dest) {
            on_bytes(metadata.len());
            if let Some(tracker) = &tracker {
                tracker.skipped(metadata.len());
            }
        }
        return Ok(());
    }
    if let Some(parent) = spec.dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temp = temp_path(&spec.dest);
    let file_id = tracker.as_ref().map(|tracker| tracker.begin(spec));
    let result = {
        let mut forward = |bytes: u64| {
            if let (Some(tracker), Some(id)) = (&tracker, file_id) {
                tracker.add_bytes(id, bytes);
            }
            on_bytes(bytes);
        };
        write_download(spec, &temp, &mut forward).await
    };
    if let (Some(tracker), Some(id)) = (&tracker, file_id) {
        tracker.finish(id, result.is_ok());
    }
    match result {
        Ok(()) => {
            tokio::fs::rename(&temp, &spec.dest).await?;
            Ok(())
        }
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp).await;
            Err(error)
        }
    }
}

async fn write_download(
    spec: &DownloadSpec,
    temp: &Path,
    on_bytes: &mut impl FnMut(u64),
) -> AppResult<()> {
    let crate::net::NetResponse { mut response, url } = net_request(&spec.url, &spec.purpose).await?;
    if !response.status().is_success() {
        return Err(AppError::new(
            "HTTP_ERROR",
            format!("HTTP {} от {}", response.status().as_u16(), url.host_str().unwrap_or("")),
        ));
    }
    if let Some(expected) = spec.size {
        if let Some(length) = response.content_length() {
            if length != expected {
                return Err(AppError::new(
                    "DOWNLOAD_SIZE",
                    format!("Неверный размер файла: ожидался {expected}, сервер отдаёт {length}"),
                ));
            }
        }
    }

    let mut file = tokio::fs::File::create(temp).await?;
    let mut sha1_hasher = Sha1::new();
    let mut sha256_hasher = Sha256::new();
    let mut written: u64 = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| AppError::new("NET_FAILED", format!("Соединение прервано: {e}")))?
    {
        written += chunk.len() as u64;
        if let Some(size) = spec.size {
            if written > size {
                return Err(AppError::new("DOWNLOAD_SIZE", "Файл больше заявленного размера"));
            }
        }
        sha1_hasher.update(&chunk);
        sha256_hasher.update(&chunk);
        file.write_all(&chunk).await?;
        on_bytes(chunk.len() as u64);
    }
    file.flush().await?;
    drop(file);

    if let Some(size) = spec.size {
        if written != size {
            return Err(AppError::new("DOWNLOAD_SIZE", "Файл скачался не полностью"));
        }
    }
    if let Some(expected) = &spec.sha1 {
        if &hex(&sha1_hasher.finalize()) != expected {
            return Err(AppError::new("DOWNLOAD_HASH", "Файл повреждён (SHA-1 не совпал)"));
        }
    }
    if let Some(expected) = &spec.sha256 {
        if &hex(&sha256_hasher.finalize()) != expected {
            return Err(AppError::new("DOWNLOAD_HASH", "Файл повреждён (SHA-256 не совпал)"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_size_checks() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"hello").unwrap();
        assert_eq!(hash_file(&file, "sha1").unwrap(), "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
        assert!(is_file_valid(&file, Some(5), Some("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"), None));
        assert!(!is_file_valid(&file, Some(6), None, None));
        assert!(!is_file_valid(&file, None, Some("0".repeat(40).as_str()), None));
        assert!(!is_file_valid(&dir.path().join("missing"), None, None, None));
    }
}
