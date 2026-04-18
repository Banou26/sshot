use std::sync::mpsc::Sender;

use ksni::blocking::TrayMethods;

#[derive(Debug, Clone)]
pub enum Action {
    TakeScreenshot,
    OpenLastScreenshot,
    OpenFolder,
    OpenConfig,
    ReloadConfig,
    Quit,
}

pub struct TrayIcon {
    sender: Sender<Action>,
}

impl ksni::Tray for TrayIcon {
    fn id(&self) -> String {
        "sshot".into()
    }

    fn icon_name(&self) -> String {
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        // 24x24 camera emoji icon (ARGB premultiplied, big-endian per SNI spec)
        let size = 24i32;
        let mut data = vec![0u8; (size * size * 4) as usize];
        let set = |data: &mut Vec<u8>, x: i32, y: i32, r: u8, g: u8, b: u8| {
            if x >= 0 && x < size && y >= 0 && y < size {
                let i = ((y * size + x) * 4) as usize;
                data[i] = 255; data[i+1] = r; data[i+2] = g; data[i+3] = b;
            }
        };
        // Camera body (white rounded rect)
        for y in 8..20 { for x in 3..21 { set(&mut data, x, y, 220, 220, 230); } }
        // Viewfinder bump
        for y in 6..9 { for x in 8..14 { set(&mut data, x, y, 200, 200, 210); } }
        // Lens (dark circle)
        let cx = 12.0f64; let cy = 14.0;
        for y in 8..20 { for x in 3..21 {
            let dx = x as f64 - cx; let dy = y as f64 - cy;
            let d = (dx*dx + dy*dy).sqrt();
            if d < 4.5 { set(&mut data, x, y, 60, 60, 70); }
            if d < 3.2 { set(&mut data, x, y, 100, 160, 220); }
            if d < 1.5 { set(&mut data, x, y, 180, 210, 240); }
        }}
        vec![ksni::Icon { width: size, height: size, data }]
    }

    fn title(&self) -> String {
        "Screenshot".into()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.sender.send(Action::OpenLastScreenshot);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: "Take Screenshot".into(),
                icon_name: "camera-photo".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.sender.send(Action::TakeScreenshot);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open Screenshots Folder".into(),
                icon_name: "folder-pictures".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.sender.send(Action::OpenFolder);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open Config".into(),
                icon_name: "preferences-other".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.sender.send(Action::OpenConfig);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Reload Config".into(),
                icon_name: "view-refresh".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.sender.send(Action::ReloadConfig);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.sender.send(Action::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Spawn the tray icon in a background thread. Returns immediately.
///
/// Registration retries in the background because StatusNotifierWatcher may
/// not be on the bus yet when the daemon starts (e.g. the shell's tray host
/// hasn't come up). Compositor startup order is not something we can rely on.
pub fn spawn(sender: Sender<Action>) {
    std::thread::spawn(move || {
        for attempt in 1u32..=60 {
            let icon = TrayIcon { sender: sender.clone() };
            match icon.spawn() {
                Ok(handle) => {
                    if attempt > 1 {
                        eprintln!("tray: registered on attempt {attempt}");
                    }
                    // handle is leaked intentionally — tray lives until process exits
                    std::mem::forget(handle);
                    return;
                }
                Err(e) if attempt == 1 || attempt.is_power_of_two() => {
                    eprintln!("tray: register failed (attempt {attempt}): {e}");
                }
                Err(_) => {}
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        eprintln!("tray: giving up after 60 attempts — no StatusNotifierWatcher");
    });
}
