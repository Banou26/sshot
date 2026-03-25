use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub save: SaveConfig,
    pub appearance: AppearanceConfig,
    /// Global shortcut key (e.g. "Print", "Meta+Shift+S"). Empty to disable.
    #[serde(default = "default_shortcut")]
    pub shortcut: String,
}

fn default_shortcut() -> String {
    "Print".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SaveConfig {
    /// Base directory for screenshots
    pub directory: String,
    /// Subfolder format (strftime). e.g. "%Y-%m"
    pub subfolder: String,
    /// Filename format for window captures.
    /// Variables: {title}, {random}. Also supports strftime.
    pub window_format: String,
    /// Filename format for region captures.
    /// Variables: {random}. Also supports strftime.
    pub region_format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    pub dim_factor: f32,
    pub border_width: u32,
    pub window_border_color: [u8; 3],
    pub region_border_color: [u8; 3],
}

impl Default for Config {
    fn default() -> Self {
        Self {
            save: SaveConfig::default(),
            appearance: AppearanceConfig::default(),
            shortcut: default_shortcut(),
        }
    }
}

impl Default for SaveConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        Self {
            directory: format!("{home}/Pictures/Screenshots"),
            subfolder: "%Y-%m".into(),
            window_format: "{title} %Y-%m-%d-%H-%M-%S-{random}".into(),
            region_format: "%Y-%m-%d-%H-%M-%S-{random}".into(),
        }
    }
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            dim_factor: 0.75,
            border_width: 3,
            window_border_color: [80, 140, 255],
            region_border_color: [255, 255, 255],
        }
    }
}

impl Config {
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("sshot")
            .join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        let config = match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                eprintln!("Warning: invalid config at {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        config.save_to_disk(); // Persist any new fields
        config
    }

    pub fn save_to_disk(&self) {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(content) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, content);
        }
    }

    pub fn format_filename(&self, window_title: Option<&str>) -> String {
        let now = chrono::Local::now();
        let rand_id: String = (0..6)
            .map(|_| {
                let i = rand::random::<u8>() % 36;
                if i < 10 { (b'0' + i) as char } else { (b'a' + i - 10) as char }
            })
            .collect();

        let template = match window_title {
            Some(_) => &self.save.window_format,
            None => &self.save.region_format,
        };

        let name = now.format(template).to_string();
        let name = name.replace("{random}", &rand_id);
        let name = if let Some(title) = window_title {
            let safe: String = title.chars()
                .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' { c } else { '_' })
                .collect();
            let safe = safe.trim().trim_matches('_');
            let safe = if safe.len() > 80 { &safe[..80] } else { safe };
            name.replace("{title}", if safe.is_empty() { "window" } else { safe })
        } else {
            name
        };

        format!("{name}.png")
    }

    pub fn save_dir(&self) -> Result<PathBuf> {
        let now = chrono::Local::now();
        let subfolder = now.format(&self.save.subfolder).to_string();
        let dir = PathBuf::from(&self.save.directory).join(subfolder);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}
