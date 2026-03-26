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

fn which(name: &str) -> Option<String> {
    std::env::var("PATH").ok()?
        .split(':')
        .map(|dir| PathBuf::from(dir).join(name))
        .find(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
}

// ── Save & Clipboard ─────────────────────────────────────────────

fn save_and_copy(result: &CaptureResult, config: &Config) -> Result<PathBuf> {
    let title = match &result.selection {
        Selection::Window { title, .. } => Some(title.as_str()),
        Selection::Region { .. } => None,
    };

    // For window captures: use KWin CaptureWindow (isolated, no stacking changes)
    // For region captures: crop from the workspace screenshot
    let (rgba, pw, ph) = if let Selection::Window { title, .. } = &result.selection {
        let target_idx = result.windows.iter().position(|w| &w.title == title);
        let target_id = target_idx
            .map(|i| result.windows[i].internal_id.clone())
            .unwrap_or_default();

        let (data, w, h, stride) = capture::capture_window(&target_id)
            .context("CaptureWindow failed — is sshot.desktop installed with X-KDE-DBUS-Restricted-Interfaces?")?;

        // Convert BGRA raw → RGBA
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for row in 0..h {
            for col in 0..w {
                let si = (row * stride + col * 4) as usize;
                let di = ((row * w + col) * 4) as usize;
                if si + 3 < data.len() {
                    rgba[di] = data[si + 2];
                    rgba[di + 1] = data[si + 1];
                    rgba[di + 2] = data[si];
                    rgba[di + 3] = data[si + 3];
                }
            }
        }
        (rgba, w, h)
    } else {
        let Selection::Region { x, y, w, h } = &result.selection else { unreachable!() };
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
        (rgba, pw, ph)
    };

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

/// Convert a shortcut string like "Ctrl+Shift+4" to a Qt key integer.
/// When Shift is combined with a digit, KDE expects the shifted symbol (e.g. $ not Shift+4).
fn parse_shortcut_to_qt_key(shortcut: &str) -> Option<i32> {
    let mut modifiers: i32 = 0;
    let mut key: i32 = 0;
    let mut has_shift = false;

    for part in shortcut.split('+') {
        let part = part.trim();
        match part.to_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= 0x04000000,
            "shift" => { modifiers |= 0x02000000; has_shift = true; }
            "alt" => modifiers |= 0x08000000,
            "meta" | "super" => modifiers |= 0x10000000,
            _ => {
                key = match part.to_lowercase().as_str() {
                    "print" | "printscreen" => 0x01000009,
                    "escape" | "esc" => 0x01000000,
                    "space" => 0x20,
                    "tab" => 0x01000001,
                    "return" | "enter" => 0x01000004,
                    "backspace" => 0x01000003,
                    "delete" => 0x01000007,
                    "f1" => 0x01000030, "f2" => 0x01000031, "f3" => 0x01000032,
                    "f4" => 0x01000033, "f5" => 0x01000034, "f6" => 0x01000035,
                    "f7" => 0x01000036, "f8" => 0x01000037, "f9" => 0x01000038,
                    "f10" => 0x01000039, "f11" => 0x0100003a, "f12" => 0x0100003b,
                    s if s.len() == 1 => s.chars().next().unwrap().to_ascii_uppercase() as i32,
                    _ => return None,
                };
            }
        }
    }

    if key == 0 { return None; }

    // When Shift + digit, KDE expects the shifted symbol instead
    if has_shift {
        let shifted = match (key as u8) as char {
            '1' => Some('!' as i32),
            '2' => Some('@' as i32),
            '3' => Some('#' as i32),
            '4' => Some('$' as i32),
            '5' => Some('%' as i32),
            '6' => Some('^' as i32),
            '7' => Some('&' as i32),
            '8' => Some('*' as i32),
            '9' => Some('(' as i32),
            '0' => Some(')' as i32),
            _ => None,
        };
        if let Some(sym) = shifted {
            // Replace key with shifted symbol, remove Shift modifier
            key = sym;
            modifiers &= !0x02000000;
        }
    }

    Some(key | modifiers)
}

fn register_shortcut(shortcut: &str) {
    if shortcut.is_empty() {
        return;
    }

    // Install a .desktop file for the trigger action
    let apps_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("applications");
    let _ = std::fs::create_dir_all(&apps_dir);

    let sshot_bin = which("sshot").unwrap_or_else(|| "sshot".into());
    let desktop = format!(
        "[Desktop Entry]\n\
         Name=Screenshot\n\
         Comment=Take a screenshot\n\
         Exec={sshot_bin} --trigger\n\
         Type=Application\n\
         NoDisplay=true\n"
    );
    let _ = std::fs::write(apps_dir.join("sshot-trigger.desktop"), &desktop);

    // Parse shortcut to Qt key integer
    let qt_key = match parse_shortcut_to_qt_key(shortcut) {
        Some(k) => k,
        None => {
            eprintln!("Warning: could not parse shortcut '{shortcut}'");
            return;
        }
    };

    // Register via kglobalaccel D-Bus API
    let _ = Command::new("busctl")
        .args([
            "--user", "call",
            "org.kde.kglobalaccel", "/kglobalaccel",
            "org.kde.KGlobalAccel", "setForeignShortcut",
            "asai",
            "4", "sshot-trigger.desktop", "_launch", "Screenshot", "Take Screenshot",
            "1", &qt_key.to_string(),
        ])
        .output();

    // Also write to config file for persistence across reboots
    let key_entry = format!("{shortcut},none,Take Screenshot");
    let _ = Command::new("kwriteconfig6")
        .args([
            "--file", "kglobalshortcutsrc",
            "--group", "sshot-trigger.desktop",
            "--key", "_launch",
            &key_entry,
        ])
        .status();

    eprintln!("Shortcut registered: {shortcut} (Qt key: 0x{qt_key:08x})");
}

// ── Actions ──────────────────────────────────────────────────────

/// Install a .desktop file that grants this process KWin ScreenShot2 authorization.
fn install_screenshot_permission() {
    let apps_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("applications");
    let _ = std::fs::create_dir_all(&apps_dir);

    let sshot_bin = which("sshot").unwrap_or_else(|| "sshot".into());
    let desktop = format!(
        "[Desktop Entry]\n\
         Name=sshot\n\
         Exec={sshot_bin} --daemon\n\
         Type=Application\n\
         NoDisplay=true\n\
         X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2\n"
    );
    let _ = std::fs::write(apps_dir.join("sshot.desktop"), &desktop);
}

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
    install_screenshot_permission();

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
            tray::Action::OpenFolder => {
                let dir = config.save_dir().unwrap_or_else(|_| PathBuf::from(&config.save.directory));
                let _ = Command::new("xdg-open").arg(&dir).spawn();
            }
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
