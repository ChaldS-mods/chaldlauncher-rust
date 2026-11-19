use std::fs::OpenOptions;
use std::io::Write;

use crate::cli::Cli;
use crate::env::{self, SystemEnv};
use crate::payload::read_header_bytes;
use crate::setup::{self, InstallOptions};

fn log(message: &str) {
    let path = std::env::temp_dir().join("ChaldLauncher-Setup.log");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {}", chrono::Local::now().to_rfc3339(), message);
    }
}

/// `--silent`: no window, log to `%TEMP%\ChaldLauncher-Setup.log`, exit code 0/1.
pub fn run(payload: &[u8], cli: &Cli) -> i32 {
    let result = (|| -> Result<(), String> {
        let header = read_header_bytes(payload)?;
        let existing = env::find_existing(&header.meta);
        let dir = cli
            .dir
            .clone()
            .or_else(|| existing.map(|e| e.dir))
            .unwrap_or_else(|| env::default_install_dir(&header.meta));
        if !dir.is_absolute() {
            return Err("--dir должен быть абсолютным путём".to_string());
        }
        let options = InstallOptions {
            dir,
            desktop_shortcut: cli.desktop,
            start_menu_shortcut: cli.start_menu,
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let done = setup::install(payload, &options, &SystemEnv, &cancel, |_| {})?;
        let warnings = if done.warnings.is_empty() {
            String::new()
        } else {
            format!(" ({})", done.warnings.join("; "))
        };
        log(&format!("OK {} -> {}{}", done.version, done.dir, warnings));
        if cli.launch {
            let _ = std::process::Command::new(&done.exe_path).current_dir(&done.dir).spawn();
        }
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            log(&format!("ERROR {error}"));
            1
        }
    }
}
