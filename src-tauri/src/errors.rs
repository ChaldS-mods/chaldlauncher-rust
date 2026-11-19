use serde::Serialize;

/// An error that is shown in the UI. `code` is stable for the renderer, `message` is human readable.
#[derive(Debug, Clone, Serialize)]
pub struct AppError {
    pub code: String,
    pub message: String,
}

impl AppError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }

    pub fn not_implemented(what: &str) -> Self {
        Self::new("NOT_IMPLEMENTED", format!("{what}: эта часть ещё не перенесена на Rust"))
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::new("IO", e.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        Self::new("JSON", e.to_string())
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        Self::new("NETWORK", e.to_string())
    }
}

impl From<tauri::Error> for AppError {
    fn from(e: tauri::Error) -> Self {
        Self::new("WINDOW", e.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;

/// The envelope the renderer expects: `IpcResult<T>` from `src/shared/ipc.ts`.
pub fn ok<T: Serialize>(data: T) -> serde_json::Value {
    serde_json::json!({ "ok": true, "data": data })
}

pub fn fail(e: &AppError) -> serde_json::Value {
    serde_json::json!({
        "ok": false,
        "error": { "code": e.code, "message": e.message }
    })
}
