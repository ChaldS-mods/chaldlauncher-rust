use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Closed,
    Hud,
    Open,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Closed => "closed",
            Mode::Hud => "hud",
            Mode::Open => "open",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallPhase {
    Idle,
    Outgoing,
    Incoming,
    Connecting,
    Active,
}

impl CallPhase {
    fn from_str(s: &str) -> Self {
        match s {
            "outgoing" => Self::Outgoing,
            "incoming" => Self::Incoming,
            "connecting" => Self::Connecting,
            "active" => Self::Active,
            _ => Self::Idle,
        }
    }
}

#[derive(Debug, Clone)]
struct PendingCall {
    _user_id: i64,
    _name: String,
    _mc_name: String,
    _group_id: Option<i64>,
    _member_ids: Vec<i64>,
}

#[derive(Debug)]
struct Inner {
    open: bool,
    phase: CallPhase,
    mode: Mode,
    pending_call: Option<PendingCall>,
    _opened_at: Option<Instant>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            open: false,
            phase: CallPhase::Idle,
            mode: Mode::Closed,
            pending_call: None,
            _opened_at: None,
        }
    }
}

pub struct OverlayManager {
    inner: Mutex<Inner>,
}

impl OverlayManager {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    fn overlay_url() -> WebviewUrl {
        if cfg!(debug_assertions) {
            WebviewUrl::External(
                "http://localhost:5173/overlay.html"
                    .parse()
                    .expect("valid overlay dev url"),
            )
        } else {
            WebviewUrl::App("overlay.html".into())
        }
    }

    fn ensure_window(&self, app: &AppHandle) -> Option<WebviewWindow> {
        if let Some(win) = app.get_webview_window("overlay") {
            return Some(win);
        }
        let (x, y, w, h) = Self::target_bounds(app);
        let builder = WebviewWindowBuilder::new(app, "overlay", Self::overlay_url())
            .title("Chald Overlay")
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .closable(true)
            .focused(false)
            .visible(false)
            .shadow(false)
            .position(x as f64, y as f64)
            .inner_size(w as f64, h as f64);

        let win = builder.build().ok()?;
        // Best-effort initial state: hidden and click-through.
        let _ = win.set_ignore_cursor_events(true);
        let _ = win.set_always_on_top(true);
        Some(win)
    }

    fn target_bounds(app: &AppHandle) -> (i32, i32, u32, u32) {
        // Prefer monitor under cursor (where the game likely is), fall back to primary.
        let monitor = app
            .cursor_position()
            .ok()
            .and_then(|pos| app.monitor_from_point(pos.x, pos.y).ok().flatten())
            .or_else(|| app.primary_monitor().ok().flatten());
        if let Some(m) = monitor {
            let pos = m.position();
            let size = m.size();
            (pos.x, pos.y, size.width, size.height)
        } else {
            (0, 0, 1280, 720)
        }
    }

    fn place_window(app: &AppHandle, win: &WebviewWindow) {
        let (x, y, w, h) = Self::target_bounds(app);
        #[cfg(target_os = "linux")]
        {
            let _ = win.set_fullscreen(false);
        }
        let _ = win.set_position(PhysicalPosition::new(x, y));
        let _ = win.set_size(PhysicalSize::new(w, h));
    }

    fn emit_event(app: &AppHandle, channel: &str, payload: Value) {
        let _ = app.emit(
            "chald:event",
            json!({
                "channel": channel,
                "payload": payload,
            }),
        );
    }

    fn send_mode(&self, app: &AppHandle) {
        let mode = self.inner.lock().unwrap().mode;
        Self::emit_event(app, "overlay:mode", Value::String(mode.as_str().to_string()));
    }

    fn apply(&self, app: &AppHandle) {
        let Some(win) = self.ensure_window(app) else {
            return;
        };
        let next_mode = {
            let mut inner = self.inner.lock().unwrap();
            let next = if inner.open {
                Mode::Open
            } else if inner.phase != CallPhase::Idle {
                Mode::Hud
            } else {
                Mode::Closed
            };
            let prev = inner.mode;
            let place = prev == Mode::Closed || next == Mode::Open;
            if next == Mode::Open {
                inner._opened_at = Some(Instant::now());
            }
            inner.mode = next;
            (next, place)
        };

        if next_mode.0 == Mode::Closed {
            let _ = win.set_ignore_cursor_events(true);
            #[cfg(target_os = "linux")]
            {
                let _ = win.set_fullscreen(false);
            }
            let _ = win.hide();
            self.send_mode(app);
            return;
        }

        if next_mode.1 {
            Self::place_window(app, &win);
        }

        match next_mode.0 {
            Mode::Open => {
                let _ = win.set_ignore_cursor_events(false);
                let _ = win.set_always_on_top(true);
                let _ = win.show();
                #[cfg(target_os = "linux")]
                {
                    let _ = win.set_fullscreen(true);
                    let _ = win.set_always_on_top(true);
                }
                let _ = win.set_focus();
            }
            Mode::Hud => {
                let _ = win.set_ignore_cursor_events(true);
                let _ = win.set_always_on_top(true);
                let _ = win.show();
                #[cfg(target_os = "linux")]
                {
                    let _ = win.set_fullscreen(true);
                    let _ = win.set_always_on_top(true);
                }
            }
            Mode::Closed => {}
        }
        let _ = win.set_always_on_top(true);
        self.send_mode(app);
    }

    pub fn set_open(&self, app: &AppHandle, open: bool) -> bool {
        {
            self.inner.lock().unwrap().open = open;
        }
        self.apply(app);
        self.inner.lock().unwrap().open
    }

    pub fn toggle(&self, app: &AppHandle) -> bool {
        let open = { !self.inner.lock().unwrap().open };
        self.set_open(app, open)
    }

    pub fn show_window(&self, app: &AppHandle) -> bool {
        self.set_open(app, true)
    }

    pub fn is_visible(&self) -> bool {
        self.inner.lock().unwrap().mode != Mode::Closed
    }

    pub fn set_call_phase(&self, app: &AppHandle, phase: &str) -> bool {
        {
            self.inner.lock().unwrap().phase = CallPhase::from_str(phase);
        }
        self.apply(app);
        self.is_visible()
    }

    pub fn start_call(
        &self,
        app: &AppHandle,
        user_id: i64,
        name: &str,
        mc_name: &str,
        group_id: Option<i64>,
        member_ids: Vec<i64>,
    ) -> bool {
        if self.ensure_window(app).is_none() {
            return false;
        }
        {
            let mut inner = self.inner.lock().unwrap();
            inner.pending_call = Some(PendingCall {
                _user_id: user_id,
                _name: name.to_string(),
                _mc_name: mc_name.to_string(),
                _group_id: group_id,
                _member_ids: member_ids.clone(),
            });
        }
        self.apply(app);
        let payload = json!({
            "userId": user_id,
            "name": name,
            "mcName": mc_name,
            "groupId": group_id,
            "memberIds": member_ids,
        });
        Self::emit_event(app, "overlay:startCall", payload.clone());
        let app_delayed = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            Self::emit_event(&app_delayed, "overlay:startCall", payload);
        });
        let _ = self.inner.lock().unwrap().pending_call.take();
        true
    }

    #[allow(dead_code)]
    pub fn ensure_background(&self, app: &AppHandle) {
        let _ = self.ensure_window(app);
    }
}

impl Default for OverlayManager {
    fn default() -> Self {
        Self::new()
    }
}