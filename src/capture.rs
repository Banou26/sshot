use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsFd;

use anyhow::{Context, Result};

/// Capture a specific window by its KWin internal ID. Returns raw image data + metadata.
/// Requires X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2 in the .desktop file.
pub fn capture_window(internal_id: &str) -> Result<(Vec<u8>, u32, u32, u32)> {
    let (read_pipe, write_pipe) = os_pipe::pipe()?;

    let conn = zbus::blocking::Connection::session()
        .context("Failed to connect to session D-Bus")?;

    let mut options: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
    options.insert("include-decoration", true.into());
    options.insert("native-resolution", true.into());

    let write_fd = zbus::zvariant::Fd::from(write_pipe.as_fd());

    let reply = conn.call_method(
        Some("org.kde.KWin"),
        "/org/kde/KWin/ScreenShot2",
        Some("org.kde.KWin.ScreenShot2"),
        "CaptureWindow",
        &(internal_id, options, write_fd),
    ).context("CaptureWindow D-Bus call failed")?;

    drop(write_pipe);

    let mut data = Vec::new();
    std::io::BufReader::new(read_pipe).read_to_end(&mut data)?;

    let results: HashMap<String, zbus::zvariant::OwnedValue> = reply.body().deserialize()?;

    let width = results.get("width")
        .and_then(|v| <u32>::try_from(v).ok())
        .context("No width in results")?;
    let height = results.get("height")
        .and_then(|v| <u32>::try_from(v).ok())
        .context("No height in results")?;
    let stride = results.get("stride")
        .and_then(|v| <u32>::try_from(v).ok())
        .unwrap_or(width * 4);

    Ok((data, width, height, stride))
}

/// Capture the full workspace. Returns raw BGRA data + metadata.
pub fn capture_workspace() -> Result<(Vec<u8>, u32, u32, u32)> {
    let (read_pipe, write_pipe) = os_pipe::pipe()?;

    let conn = zbus::blocking::Connection::session()
        .context("Failed to connect to session D-Bus")?;

    let mut options: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
    options.insert("include-cursor", false.into());
    options.insert("native-resolution", true.into());

    let write_fd = zbus::zvariant::Fd::from(write_pipe.as_fd());

    let reply = conn.call_method(
        Some("org.kde.KWin"),
        "/org/kde/KWin/ScreenShot2",
        Some("org.kde.KWin.ScreenShot2"),
        "CaptureWorkspace",
        &(options, write_fd),
    ).context("CaptureWorkspace D-Bus call failed")?;

    drop(write_pipe);

    let mut data = Vec::new();
    std::io::BufReader::new(read_pipe).read_to_end(&mut data)?;

    let results: HashMap<String, zbus::zvariant::OwnedValue> = reply.body().deserialize()?;

    let width = results.get("width")
        .and_then(|v| <u32>::try_from(v).ok())
        .context("No width in results")?;
    let height = results.get("height")
        .and_then(|v| <u32>::try_from(v).ok())
        .context("No height in results")?;
    let stride = results.get("stride")
        .and_then(|v| <u32>::try_from(v).ok())
        .unwrap_or(width * 4);

    // Data is already BGRA (KWin ARGB32 = BGRA on LE) — same as Wayland Argb8888
    Ok((data, width, height, stride))
}
