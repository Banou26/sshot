mod capture;
mod config;
mod overlay;
mod tray;

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;

use anyhow::{Context, Result};
use signal_hook::consts::{SIGUSR1, SIGUSR2, SIGTERM, SIGINT};
use signal_hook::iterator::Signals;

use crate::config::Config;
use crate::overlay::{CaptureResult, Selection};

/// Recursively find the most recently modified .png file under a directory.
fn find_latest_file(dir: &std::path::Path) -> Option<PathBuf> {
    fn walk(dir: &std::path::Path, best: &mut Option<(std::time::SystemTime, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, best);
            } else if path.extension().map(|e| e == "png").unwrap_or(false) {
                if let Ok(meta) = path.metadata() {
                    if let Ok(modified) = meta.modified() {
                        if best.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                            *best = Some((modified, path));
                        }
                    }
                }
            }
        }
    }

    let mut best = None;
    walk(dir, &mut best);
    best.map(|(_, p)| p)
}

// ── Save & Clipboard ─────────────────────────────────────────────

fn save_and_copy(result: &CaptureResult, config: &Config) -> Result<PathBuf> {
    match &result.selection {
        Selection::Window { title, id } => {
            // Use niri's built-in window capture
            let dir = config.save_dir()?;
            let filename = config.format_filename(Some(title));
            let filepath = dir.join(&filename);

            let status = Command::new("niri")
                .args([
                    "msg", "action", "screenshot-window",
                    "--id", id,
                    "--path", &filepath.to_string_lossy(),
                ])
                .status()
                .context("Failed to run niri msg")?;

            if !status.success() {
                anyhow::bail!("niri screenshot-window failed");
            }

            // Copy to clipboard (niri may not do this when --path is used)
            let png_data = std::fs::read(&filepath).ok();
            if let Some(data) = png_data {
                match Command::new("wl-copy")
                    .args(["-t", "image/png"])
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                {
                    Ok(mut child) => {
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(&data);
                        }
                        let _ = child.wait();
                    }
                    Err(e) => eprintln!("Clipboard failed: {e}"),
                }
            }

            eprintln!("Saved: {}", filepath.display());
            Ok(filepath)
        }
        Selection::Region { x, y, w, h } => {
            let s = result.scale;
            let px = (x * s).round() as u32;
            let py = (y * s).round() as u32;
            let mut pw = (w * s).round() as u32;
            let mut ph = (h * s).round() as u32;
            let ss_h = result.ss_data.len() as u32 / result.ss_stride;

            let px = px.min(result.ss_width.saturating_sub(1));
            let py = py.min(ss_h.saturating_sub(1));
            pw = pw.min(result.ss_width - px);
            ph = ph.min(ss_h - py);

            if pw == 0 || ph == 0 { anyhow::bail!("Selection is empty"); }

            // Crop region from workspace screenshot and convert BGRA → RGBA
            let mut rgba = vec![0u8; (pw * ph * 4) as usize];
            for row in 0..ph {
                for col in 0..pw {
                    let si = ((py + row) * result.ss_stride + (px + col) * 4) as usize;
                    let di = ((row * pw + col) * 4) as usize;
                    if si + 3 < result.ss_data.len() {
                        rgba[di] = result.ss_data[si + 2];
                        rgba[di + 1] = result.ss_data[si + 1];
                        rgba[di + 2] = result.ss_data[si];
                        rgba[di + 3] = result.ss_data[si + 3];
                    }
                }
            }

            // Save file
            let dir = config.save_dir()?;
            let filename = config.format_filename(None);
            let filepath = dir.join(&filename);
            image::save_buffer(&filepath, &rgba, pw, ph, image::ColorType::Rgba8)?;

            // Clipboard
            {
                use image::ImageEncoder;
                let mut png_buf = Vec::new();
                image::codecs::png::PngEncoder::new(&mut png_buf)
                    .write_image(&rgba, pw, ph, image::ColorType::Rgba8.into())?;

                match Command::new("wl-copy")
                    .args(["-t", "image/png"])
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                {
                    Ok(mut child) => {
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(&png_buf);
                        }
                        let _ = child.wait();
                    }
                    Err(e) => eprintln!("Clipboard failed (wl-copy not found?): {e}"),
                }
            }

            eprintln!("Saved: {}", filepath.display());
            Ok(filepath)
        }
    }
}

// ── PID file ─────────────────────────────────────────────────────

fn pid_path() -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
    PathBuf::from(runtime_dir).join("sshot.pid")
}

fn write_pid() {
    let _ = std::fs::write(pid_path(), std::process::id().to_string());
}

fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_path()).ok()?.trim().parse().ok()
}

fn remove_pid() {
    let _ = std::fs::remove_file(pid_path());
}

/// Check if a process with the given PID is alive (sends signal 0).
fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

// ── Actions ──────────────────────────────────────────────────────

fn take_screenshot(config: &Config) {
    match overlay::run(config) {
        Ok(Some(result)) => {
            if let Err(e) = save_and_copy(&result, config) {
                eprintln!("Save error: {e}");
            }
        }
        Ok(None) => {} // Cancelled
        Err(e) => eprintln!("Screenshot error: {e:#}"),
    }
}

/// Capture the focused window using niri's built-in screenshot-window action.
fn take_window_screenshot(config: &Config) -> Result<()> {
    // Get focused window info for the filename
    let title = Command::new("niri")
        .args(["msg", "--json", "focused-window"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|v| v["title"].as_str().map(String::from));

    let dir = config.save_dir()?;
    let filename = config.format_filename(title.as_deref());
    let filepath = dir.join(&filename);

    let status = Command::new("niri")
        .args([
            "msg", "action", "screenshot-window",
            "--path", &filepath.to_string_lossy(),
        ])
        .status()
        .context("Failed to run niri msg")?;

    if !status.success() {
        anyhow::bail!("niri screenshot-window failed (no focused window?)");
    }

    eprintln!("Saved: {}", filepath.display());
    Ok(())
}

fn open_config() {
    let path = Config::config_path();
    // Ensure config file exists
    if !path.exists() {
        Config::default().save_to_disk();
    }
    // Open in default editor
    if let Err(e) = Command::new("xdg-open").arg(&path).spawn() {
        eprintln!("Failed to open config: {e}");
    }
}

// ── Entry point ──────────────────────────────────────────────────

fn print_usage() {
    eprintln!("Usage: sshot [--daemon | --trigger | --window | --settings | --oneshot]");
    eprintln!("  --daemon    Start as background daemon with system tray (default)");
    eprintln!("  --trigger   Signal running daemon to take a screenshot");
    eprintln!("  --window    Screenshot the focused window (via niri)");
    eprintln!("  --settings  Signal running daemon to open settings");
    eprintln!("  --oneshot   Take one screenshot and exit");
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("--daemon");

    match mode {
        "--trigger" => {
            if let Some(pid) = read_pid() {
                unsafe { libc::kill(pid as i32, SIGUSR1); }
            } else {
                eprintln!("No running daemon found. Start with: sshot --daemon");
            }
            return Ok(());
        }
        "--window" => {
            let config = Config::load();
            if let Err(e) = take_window_screenshot(&config) {
                eprintln!("Window screenshot error: {e:#}");
            }
            return Ok(());
        }
        "--settings" => {
            if let Some(pid) = read_pid() {
                unsafe { libc::kill(pid as i32, SIGUSR2); }
            } else {
                open_config();
            }
            return Ok(());
        }
        "--oneshot" => {
            let config = Config::load();
            take_screenshot(&config);
            return Ok(());
        }
        "--help" | "-h" => {
            print_usage();
            return Ok(());
        }
        "--daemon" | _ => {}
    }

    // ── Daemon mode ──────────────────────────────────────────────
    // Single-instance check: if another daemon is already running, exit.
    if let Some(existing) = read_pid() {
        if is_process_alive(existing) {
            eprintln!("sshot daemon already running (pid {existing}), exiting.");
            return Ok(());
        }
        // Stale PID file — remove and continue
        remove_pid();
    }

    let mut config = Config::load();
    eprintln!("Screenshot daemon starting. Config: {}", Config::config_path().display());

    write_pid();

    let (tx, rx) = mpsc::channel::<tray::Action>();

    // System tray (background thread via ksni; retries until the watcher is up)
    tray::spawn(tx.clone());

    // Signal handler (background thread)
    let sig_tx = tx.clone();
    std::thread::spawn(move || {
        let mut signals = Signals::new([SIGUSR1, SIGUSR2, SIGTERM, SIGINT])
            .expect("Failed to register signals");
        for sig in signals.forever() {
            match sig {
                SIGUSR1 => { let _ = sig_tx.send(tray::Action::TakeScreenshot); }
                SIGUSR2 => { let _ = sig_tx.send(tray::Action::OpenConfig); }
                SIGTERM | SIGINT => { let _ = sig_tx.send(tray::Action::Quit); break; }
                _ => {}
            }
        }
    });

    eprintln!("Ready. Use 'sshot --trigger' or system tray to capture.");

    // Main event loop
    for action in &rx {
        match action {
            tray::Action::TakeScreenshot => take_screenshot(&config),
            tray::Action::OpenLastScreenshot => {
                let base = Config::expand_path(&config.save.directory);
                if let Some(latest) = find_latest_file(&base) {
                    // dolphin --select opens the folder with the file highlighted
                    if Command::new("dolphin").arg("--select").arg(&latest).spawn().is_err() {
                        let _ = Command::new("xdg-open").arg(latest.parent().unwrap_or(&base)).spawn();
                    }
                } else {
                    let _ = Command::new("xdg-open").arg(&base).spawn();
                }
            }
            tray::Action::OpenFolder => {
                let dir = config.save_dir().unwrap_or_else(|_| PathBuf::from(&config.save.directory));
                let _ = Command::new("xdg-open").arg(&dir).spawn();
            }
            tray::Action::OpenConfig => open_config(),
            tray::Action::ReloadConfig => {
                config = Config::load();
                eprintln!("Config reloaded");
            }
            tray::Action::Quit => break,
        }
    }

    remove_pid();
    eprintln!("Daemon stopped");
    Ok(())
}
