use std::sync::mpsc::Sender;

use ksni::blocking::TrayMethods;

#[derive(Debug, Clone)]
pub enum Action {
    TakeScreenshot,
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
        "camera-photo".into()
    }

    fn title(&self) -> String {
        "Screenshot".into()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.sender.send(Action::TakeScreenshot);
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
pub fn spawn(sender: Sender<Action>) -> Result<(), ksni::Error> {
    let icon = TrayIcon { sender };
    let _handle = icon.spawn()?;
    // handle is leaked intentionally — tray lives until process exits
    std::mem::forget(_handle);
    Ok(())
}
