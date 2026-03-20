use ashpd::desktop::screenshot::Screenshot;
use chrono::Local;
use clap::{Parser, ValueEnum};
use rand::Rng;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, ValueEnum)]
enum Mode {
    /// Select a region interactively
    Region,
    /// Capture the active window
    Window,
    /// Capture the full screen
    Full,
}

#[derive(Parser, Debug)]
#[command(name = "sshot", about = "Wayland screenshot tool for KDE Plasma")]
struct Args {
    /// Screenshot mode
    #[arg(short, long, default_value = "region")]
    mode: Mode,

    /// Output directory
    #[arg(short, long, default_value_t = default_output_dir())]
    output: String,

    /// Skip copying to clipboard
    #[arg(long, default_value_t = false)]
    no_clipboard: bool,
}

fn default_output_dir() -> String {
    dirs_or_home("Pictures/Screenshots")
}

fn dirs_or_home(sub: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = PathBuf::from(home);
        p.push(sub);
        p.to_string_lossy().into_owned()
    } else {
        format!("/tmp/{sub}")
    }
}

/// Try to get the active window name via KDE's D-Bus interface or kdotool.
fn get_active_window_name() -> String {
    // Try kdotool first (common on KDE Wayland)
    if let Ok(output) = Command::new("kdotool").args(["getactivewindow", "getwindowclassname"]).output() {
        if output.status.success() {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !name.is_empty() {
                return sanitize_filename(&name);
            }
        }
    }

    // Try qdbus (KWin scripting)
    if let Ok(output) = Command::new("qdbus")
        .args(["org.kde.KWin", "/KWin", "org.kde.KWin.activeWindow"])
        .output()
    {
        if output.status.success() {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !name.is_empty() {
                return sanitize_filename(&name);
            }
        }
    }

    "unknown".to_string()
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>()
        .chars()
        .take(64)
        .collect()
}

fn generate_filename(window_name: &str) -> String {
    let now = Local::now();
    let date = now.format("%Y-%m-%d_%H-%M-%S");
    let random: u16 = rand::thread_rng().gen_range(1000..9999);
    format!("{window_name}_{date}_{random}.png")
}

fn copy_to_clipboard(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    // Use wl-copy which works on all Wayland compositors
    let status = Command::new("wl-copy")
        .args(["--type", "image/png"])
        .stdin(std::process::Stdio::from(std::fs::File::open(path)?))
        .status()?;

    if !status.success() {
        return Err("wl-copy failed".into());
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let window_name = get_active_window_name();

    // Use XDG Desktop Portal for screenshot (works on KDE Plasma Wayland)
    let uri = match args.mode {
        Mode::Region => {
            // Interactive mode lets the user select a region via the compositor
            Screenshot::request()
                .interactive(true)
                .modal(false)
                .send()
                .await?
                .response()?
                .uri()
                .to_owned()
        }
        Mode::Window => {
            // Interactive mode with the portal — KDE Plasma shows window selection
            Screenshot::request()
                .interactive(true)
                .modal(true)
                .send()
                .await?
                .response()?
                .uri()
                .to_owned()
        }
        Mode::Full => {
            // Non-interactive: capture the entire screen
            Screenshot::request()
                .interactive(false)
                .modal(false)
                .send()
                .await?
                .response()?
                .uri()
                .to_owned()
        }
    };

    // The portal returns a file:// URI — convert to a path
    let source_path = uri
        .to_file_path()
        .map_err(|_| format!("Failed to convert URI to path: {uri}"))?;

    // Build destination path
    let output_dir = PathBuf::from(&args.output);
    std::fs::create_dir_all(&output_dir)?;

    let filename = generate_filename(&window_name);
    let dest_path = output_dir.join(&filename);

    // Copy screenshot from portal temp location to our destination
    std::fs::copy(&source_path, &dest_path)?;

    // Clean up the temp file from the portal
    let _ = std::fs::remove_file(&source_path);

    println!("Saved: {}", dest_path.display());

    // Copy to clipboard
    if !args.no_clipboard {
        match copy_to_clipboard(&dest_path) {
            Ok(()) => println!("Copied to clipboard"),
            Err(e) => eprintln!("Clipboard copy failed: {e}"),
        }
    }

    Ok(())
}
