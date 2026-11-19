//! Discord Rich Presence через локальный IPC (сокет/pipe десктопного клиента). Порт `src/main/discord.ts`.
//! Без зависимостей; наружу из этого модуля ничего не уходит, всё остаётся на компьютере.

use std::io::{Read, Write};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const OP_HANDSHAKE: i32 = 0;
const OP_FRAME: i32 = 1;
const OP_CLOSE: i32 = 2;
const OP_PING: i32 = 3;
const OP_PONG: i32 = 4;
const MAX_FRAME: usize = 1024 * 1024;
const RETRY: Duration = Duration::from_secs(15);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Discord принимает около 5 обновлений за 20 секунд.
const MIN_GAP: Duration = Duration::from_millis(1500);

#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type Stream = std::fs::File;

fn connect(path: &str) -> std::io::Result<Stream> {
    #[cfg(unix)]
    {
        Stream::connect(path)
    }
    #[cfg(windows)]
    {
        std::fs::OpenOptions::new().read(true).write(true).open(path)
    }
}

fn shutdown(stream: &Stream) {
    #[cfg(unix)]
    {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    #[cfg(windows)]
    {
        let _ = stream;
    }
}

pub fn encode_frame(op: i32, data: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(data).unwrap_or_default();
    let mut out = Vec::with_capacity(8 + json.len());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(&(json.len() as i32).to_le_bytes());
    out.extend_from_slice(&json);
    out
}

/// Достаёт из буфера целые кадры; остаток (неполный кадр) остаётся в `buf`.
pub fn decode_frames(buf: &mut Vec<u8>) -> Result<Vec<(i32, Value)>, String> {
    let mut out = Vec::new();
    loop {
        if buf.len() < 8 {
            break;
        }
        let op = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let len = i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        if len < 0 || len as usize > MAX_FRAME {
            return Err("Некорректный кадр Discord".to_string());
        }
        let len = len as usize;
        if buf.len() < 8 + len {
            break;
        }
        let data = serde_json::from_slice(&buf[8..8 + len]).unwrap_or(Value::Null);
        buf.drain(..8 + len);
        out.push((op, data));
    }
    Ok(out)
}

/// Где Discord (обычный, Snap, Flatpak) создаёт `discord-ipc-0..9`.
pub fn pipe_paths(platform: &str, env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..10 {
        if platform == "windows" {
            out.push(format!(r"\\?\pipe\discord-ipc-{i}"));
            continue;
        }
        let mut dirs: Vec<String> = Vec::new();
        for key in ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"] {
            if let Some(dir) = env(key).filter(|d| !d.is_empty()) {
                if !dirs.contains(&dir) {
                    dirs.push(dir);
                }
            }
        }
        if !dirs.iter().any(|d| d == "/tmp") {
            dirs.push("/tmp".to_string());
        }
        for dir in dirs {
            let dir = dir.trim_end_matches('/');
            out.push(format!("{dir}/discord-ipc-{i}"));
            if platform == "linux" {
                out.push(format!("{dir}/snap.discord/discord-ipc-{i}"));
                out.push(format!("{dir}/app/com.discordapp.Discord/discord-ipc-{i}"));
            }
        }
    }
    out
}

fn platform() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

enum Msg {
    Set(Option<Value>),
    Stop,
    Frame(u64, i32, Value),
    Closed(u64),
}

pub struct DiscordRpc {
    tx: Sender<Msg>,
}

impl DiscordRpc {
    /// Запускает фоновое соединение; пока Discord не запущен, раз в 15 с повторяет попытки.
    pub fn start(client_id: String, pid: u32) -> Self {
        let (tx, rx) = channel();
        let worker_tx = tx.clone();
        thread::Builder::new()
            .name("discord-rpc".into())
            .spawn(move || worker(client_id, pid, rx, worker_tx))
            .ok();
        Self { tx }
    }

    pub fn set_activity(&self, activity: Option<Value>) {
        let _ = self.tx.send(Msg::Set(activity));
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

fn log(message: &str) {
    eprintln!("[discord] {message}");
}

fn spawn_reader(mut stream: Stream, generation: u64, tx: Sender<Msg>) {
    thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    match decode_frames(&mut buf) {
                        Ok(frames) => {
                            for (op, data) in frames {
                                if tx.send(Msg::Frame(generation, op, data)).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        let _ = tx.send(Msg::Closed(generation));
    });
}

fn worker(client_id: String, pid: u32, rx: std::sync::mpsc::Receiver<Msg>, tx: Sender<Msg>) {
    let mut wanted: Option<Value> = None;
    let mut conn: Option<Stream> = None;
    let mut generation = 0u64;
    let mut ready = false;
    let mut last_sent: Option<String> = None;
    let mut last_sent_at: Option<Instant> = None;
    let mut hello_deadline: Option<Instant> = None;
    let mut next_retry = Instant::now();
    let mut nonce = 0u64;
    let mut missing_logged = false;

    macro_rules! drop_conn {
        () => {
            if let Some(stream) = conn.take() {
                shutdown(&stream);
            }
            ready = false;
            last_sent = None;
            hello_deadline = None;
            next_retry = Instant::now() + RETRY;
        };
    }

    loop {
        if conn.is_none() && Instant::now() >= next_retry {
            let paths = pipe_paths(platform(), &|key| std::env::var(key).ok());
            let mut opened = None;
            for path in &paths {
                if let Ok(stream) = connect(path) {
                    opened = Some((stream, path.clone()));
                    break;
                }
            }
            match opened {
                Some((mut stream, path)) => {
                    missing_logged = false;
                    log(&format!("подключено к {path}"));
                    generation += 1;
                    let handshake = encode_frame(OP_HANDSHAKE, &json!({ "v": 1, "client_id": client_id }));
                    let reader = stream.try_clone();
                    if stream.write_all(&handshake).is_ok() && reader.is_ok() {
                        spawn_reader(reader.unwrap(), generation, tx.clone());
                        conn = Some(stream);
                        ready = false;
                        hello_deadline = Some(Instant::now() + HELLO_TIMEOUT);
                    } else {
                        shutdown(&stream);
                        next_retry = Instant::now() + RETRY;
                    }
                }
                None => {
                    if !missing_logged {
                        missing_logged = true;
                        log("Discord не найден: запущен ли Discord для компьютера (не браузерный)? Повторяю попытки.");
                    }
                    next_retry = Instant::now() + RETRY;
                }
            }
        }

        match rx.recv_timeout(Duration::from_millis(400)) {
            Ok(Msg::Set(activity)) => wanted = activity,
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(mut stream) = conn.take() {
                    // очищаем статус до выхода, чтобы он не висел, пока Discord заметит закрытый pipe
                    nonce += 1;
                    let clear = json!({ "cmd": "SET_ACTIVITY", "args": { "pid": pid, "activity": null }, "nonce": nonce.to_string() });
                    let _ = stream.write_all(&encode_frame(OP_FRAME, &clear));
                    thread::sleep(Duration::from_millis(150));
                    shutdown(&stream);
                }
                return;
            }
            Ok(Msg::Frame(g, op, data)) if g == generation => {
                let event = data.get("evt").and_then(Value::as_str).unwrap_or("");
                if op == OP_PING {
                    if let Some(stream) = conn.as_mut() {
                        let _ = stream.write_all(&encode_frame(OP_PONG, &data));
                    }
                } else if op == OP_CLOSE {
                    log("соединение с Discord закрыто");
                    drop_conn!();
                } else if op == OP_FRAME && event == "ERROR" {
                    log(&format!("Discord ответил ошибкой: {data}"));
                } else if op == OP_FRAME && data.get("cmd").and_then(Value::as_str) == Some("SET_ACTIVITY") {
                    log("статус принят Discord");
                } else if op == OP_FRAME && event == "READY" {
                    log("Discord готов");
                    ready = true;
                    hello_deadline = None;
                    last_sent = None;
                }
            }
            Ok(Msg::Closed(g)) if g == generation && conn.is_some() => {
                log("соединение с Discord закрыто");
                drop_conn!();
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
        }

        if let Some(deadline) = hello_deadline {
            if Instant::now() >= deadline && !ready && conn.is_some() {
                log("Discord не ответил на приветствие");
                drop_conn!();
            }
        }

        if ready {
            let key = serde_json::to_string(&wanted).unwrap_or_default();
            let gap_ok = last_sent_at.map(|at| at.elapsed() >= MIN_GAP).unwrap_or(true);
            if last_sent.as_deref() != Some(key.as_str()) && gap_ok {
                nonce += 1;
                let payload = json!({ "cmd": "SET_ACTIVITY", "args": { "pid": pid, "activity": wanted }, "nonce": nonce.to_string() });
                let ok = conn.as_mut().map(|s| s.write_all(&encode_frame(OP_FRAME, &payload)).is_ok()).unwrap_or(false);
                if ok {
                    last_sent = Some(key);
                    last_sent_at = Some(Instant::now());
                } else {
                    drop_conn!();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip_and_split() {
        let mut bytes = encode_frame(OP_FRAME, &json!({ "evt": "READY" }));
        bytes.extend(encode_frame(OP_PING, &json!({ "n": 1 })));
        let tail = bytes.split_off(bytes.len() - 5);
        let mut buf = bytes;
        let first = decode_frames(&mut buf).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].1["evt"], "READY");
        buf.extend(tail);
        let second = decode_frames(&mut buf).unwrap();
        assert_eq!(second[0].0, OP_PING);
        assert!(buf.is_empty());
    }

    #[test]
    fn hostile_length_is_refused() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1i32.to_le_bytes());
        buf.extend_from_slice(&i32::MAX.to_le_bytes());
        assert!(decode_frames(&mut buf).is_err());
    }

    #[test]
    fn pipe_paths_cover_snap_and_flatpak() {
        let env = |key: &str| if key == "XDG_RUNTIME_DIR" { Some("/run/user/1000".to_string()) } else { None };
        let paths = pipe_paths("linux", &env);
        assert!(paths.contains(&"/run/user/1000/discord-ipc-0".to_string()));
        assert!(paths.contains(&"/run/user/1000/snap.discord/discord-ipc-0".to_string()));
        assert!(paths.contains(&"/run/user/1000/app/com.discordapp.Discord/discord-ipc-0".to_string()));
        assert!(paths.contains(&"/tmp/discord-ipc-9".to_string()));
        assert_eq!(pipe_paths("windows", &env)[0], r"\\?\pipe\discord-ipc-0");
    }

    #[cfg(unix)]
    #[test]
    fn talks_to_a_fake_discord() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", dir.path());
        let listener = UnixListener::bind(dir.path().join("discord-ipc-0")).unwrap();
        let rpc = DiscordRpc::start("123".to_string(), 42);
        rpc.set_activity(Some(json!({ "details": "hi" })));
        let (mut conn, _) = listener.accept().unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(8))).unwrap();

        fn read_frame(conn: &mut std::os::unix::net::UnixStream) -> (i32, Value) {
            let mut head = [0u8; 8];
            conn.read_exact(&mut head).unwrap();
            let op = i32::from_le_bytes([head[0], head[1], head[2], head[3]]);
            let len = i32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
            let mut body = vec![0u8; len];
            conn.read_exact(&mut body).unwrap();
            (op, serde_json::from_slice(&body).unwrap())
        }

        let (op, hello) = read_frame(&mut conn);
        assert_eq!(op, OP_HANDSHAKE);
        assert_eq!(hello["client_id"], "123");
        conn.write_all(&encode_frame(OP_FRAME, &json!({ "cmd": "DISPATCH", "evt": "READY" }))).unwrap();
        let (op, set) = read_frame(&mut conn);
        assert_eq!(op, OP_FRAME);
        assert_eq!(set["cmd"], "SET_ACTIVITY");
        assert_eq!(set["args"]["pid"], 42);
        assert_eq!(set["args"]["activity"]["details"], "hi");
        rpc.stop();
        let (_, clear) = read_frame(&mut conn);
        assert!(clear["args"]["activity"].is_null());
    }
}
