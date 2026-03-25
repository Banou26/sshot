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

// ── Save & Clipboard ─────────────────────────────────────────────

fn save_and_copy(result: &CaptureResult, config: &Config) -> Result<PathBuf> {
    let (x, y, w, h, title) = match &result.selection {
        Selection::Window { x, y, w, h, title } => (*x, *y, *w, *h, Some(title.as_str())),
        Selection::Region { x, y, w, h } => (*x, *y, *w, *h, None),
    };

    let s = result.scale;
    let px = (x * s).round() as u32;
    let py = (y * s).round() as u32;
    let pw = (w * s).round() as u32;
    let ph = (h * s).round() as u32;
    let ss_h = result.ss_data.len() as u32 / result.ss_stride;

    let px = px.min(result.ss_width.saturating_sub(1));
    let py = py.min(ss_h.saturating_sub(1));
    let pw = pw.min(result.ss_width - px);
    let ph = ph.min(ss_h - py);

    if pw == 0 || ph == 0 { anyhow::bail!("Selection is empty"); }

    // BGRA -> RGBA crop
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
    let filename = config.format_filename(title);
    let filepath = dir.join(&filename);
    image::save_buffer(&filepath, &rgba, pw, ph, image::ColorType::Rgba8)?;

    // Clipboard (synchronous — wait for wl-copy to read all data)
    {
        use image::ImageEncoder;
        let mut png_buf = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png_buf)
            .write_image(&rgba, pw, ph, image::ColorType::Rgba8.into())?;

        if let Ok(mut child) = Command::new("wl-copy")
            .args(["-t", "image/png"])
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(&png_buf);
            }
            let _ = child.wait();
        }
    }

    eprintln!("Saved: {}", filepath.display());
    Ok(filepath)
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

// ── KDE Global Shortcut ──────────────────────────────────────────

fn register_shortcut(shortcut: &str) {
    if shortcut.is_empty() {
        return;
    }

    // Install a .desktop file for the trigger action
    let apps_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("applications");
    let _ = std::fs::create_dir_all(&apps_dir);

    let sshot_bin = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "sshot".into());

    let desktop = format!(
        "[Desktop Entry]\n\
         Name=Screenshot\n\
         Comment=Take a screenshot\n\
         Exec={sshot_bin} --trigger\n\
         Type=Application\n\
         NoDisplay=true\n"
    );
    let desktop_path = apps_dir.join("sshot-trigger.desktop");
    let _ = std::fs::write(&desktop_path, &desktop);

    // Register the shortcut with KDE kglobalaccel
    let key_entry = format!("{shortcut},none,Take Screenshot");
    let _ = Command::new("kwriteconfig6")
        .args([
            "--file", "kglobalshortcutsrc",
            "--group", "sshot-trigger.desktop",
            "--key", "_launch",
            &key_entry,
        ])
        .status();

    // Reload kglobalaccel
    let _ = Command::new("dbus-send")
        .args([
            "--session", "--type=signal",
            "--dest=org.kde.kglobalaccel",
            "/kglobalaccel",
            "org.kde.KGlobalAccel.yourShortGotChanged",
        ])
        .status();

    // Alternative reload via qdbus
    let _ = Command::new("qdbus")
        .args(["org.kde.kglobalaccel", "/kglobalaccel", "blockGlobalShortcuts", "false"])
        .status();

    eprintln!("Shortcut registered: {shortcut}");
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
        Err(e) => eprintln!("Screenshot error: {e}"),
    }
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
    eprintln!("Usage: sshot [--daemon | --trigger | --settings | --oneshot]");
    eprintln!("  --daemon    Start as background daemon with system tray (default)");
    eprintln!("  --trigger   Signal running daemon to take a screenshot");
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
    let mut config = Config::load();
    eprintln!("Screenshot daemon starting. Config: {}", Config::config_path().display());

    write_pid();

    let (tx, rx) = mpsc::channel::<tray::Action>();

    // System tray (background thread via ksni)
    if let Err(e) = tray::spawn(tx.clone()) {
        eprintln!("Warning: tray icon failed: {e}");
    }

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

    register_shortcut(&config.shortcut);

    eprintln!("Ready. Use 'sshot --trigger' or system tray to capture.");

    // Main event loop
    for action in &rx {
        match action {
            tray::Action::TakeScreenshot => take_screenshot(&config),
            tray::Action::OpenConfig => open_config(),
            tray::Action::ReloadConfig => {
                config = Config::load();
                register_shortcut(&config.shortcut);
                eprintln!("Config reloaded");
            }
            tray::Action::Quit => break,
        }
    }

    remove_pid();
    eprintln!("Daemon stopped");
    Ok(())
}
