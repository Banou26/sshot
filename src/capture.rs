use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// Capture all outputs via grim. Returns raw BGRA data + width, height, stride.
pub fn capture_workspace() -> Result<(Vec<u8>, u32, u32, u32)> {
    let output = Command::new("grim")
        .args(["-t", "ppm", "-"])
        .stdout(Stdio::piped())
        .output()
        .context("Failed to run grim — is it installed?")?;

    if !output.status.success() {
        anyhow::bail!("grim failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    parse_ppm(&output.stdout)
}

/// Parse a PPM P6 image into BGRA pixel data.
fn parse_ppm(data: &[u8]) -> Result<(Vec<u8>, u32, u32, u32)> {
    let mut pos = 0;

    if !data.starts_with(b"P6") {
        anyhow::bail!("Not a PPM P6 file");
    }
    pos = find_byte(b'\n', data, pos)? + 1;

    // Skip comment lines
    while pos < data.len() && data[pos] == b'#' {
        pos = find_byte(b'\n', data, pos)? + 1;
    }

    // Parse "width height\n"
    let line_end = find_byte(b'\n', data, pos)?;
    let dims = std::str::from_utf8(&data[pos..line_end]).context("Invalid PPM dimensions")?;
    let mut parts = dims.split_whitespace();
    let width: u32 = parts.next().context("No width in PPM")?.parse()?;
    let height: u32 = parts.next().context("No height in PPM")?.parse()?;
    pos = line_end + 1;

    // Skip "255\n" (max value)
    pos = find_byte(b'\n', data, pos)? + 1;

    // Convert RGB → BGRA (Wayland Argb8888 = BGRA on little-endian)
    let rgb = &data[pos..];
    let stride = width * 4;
    let pixel_count = (width * height) as usize;
    let mut bgra = vec![0u8; pixel_count * 4];

    for i in 0..pixel_count {
        let si = i * 3;
        let di = i * 4;
        if si + 2 < rgb.len() {
            bgra[di] = rgb[si + 2];     // B
            bgra[di + 1] = rgb[si + 1]; // G
            bgra[di + 2] = rgb[si];     // R
            bgra[di + 3] = 255;         // A
        }
    }

    Ok((bgra, width, height, stride))
}

fn find_byte(byte: u8, data: &[u8], start: usize) -> Result<usize> {
    data[start..].iter().position(|&b| b == byte)
        .map(|p| start + p)
        .context("Unexpected end of PPM data")
}
