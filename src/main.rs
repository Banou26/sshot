use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use chrono::Local;
use image::RgbaImage;
use rand::Rng;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{CursorIcon, Fullscreen, Window, WindowId};

// ── Types ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct WinRect {
    name: String,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

#[derive(Debug, Clone)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

enum Selection {
    Window(Rect, String),
    Region(Rect),
    Full,
    Cancel,
}

// ── Screenshot capture via XDG portal ──────────────────────

async fn capture_screen() -> Result<RgbaImage, Box<dyn std::error::Error>> {
    use ashpd::desktop::screenshot::Screenshot;
    let resp = Screenshot::request()
        .interactive(false)
        .modal(false)
        .send()
        .await?
        .response()?;
    let path = resp.uri().to_file_path().map_err(|_| "bad URI")?;
    let img = image::open(&path)?.to_rgba8();
    let _ = std::fs::remove_file(&path);
    Ok(img)
}

// ── Window list from kdotool (KDE Wayland) ─────────────────

fn get_windows() -> Vec<WinRect> {
    let Ok(out) = Command::new("kdotool")
        .args(["search", "--name", ""])
        .output()
    else {
        return vec![];
    };
    if !out.status.success() {
        return vec![];
    }

    let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();

    let mut wins = Vec::new();
    for id in &ids {
        let Ok(geo) = Command::new("kdotool")
            .args(["getwindowgeometry", id])
            .output()
        else {
            continue;
        };
        if !geo.status.success() {
            continue;
        }
        let cls = Command::new("kdotool")
            .args(["getwindowclassname", id])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".into());

        let geo_s = String::from_utf8_lossy(&geo.stdout);
        let (mut x, mut y, mut w, mut h) = (0i32, 0i32, 0u32, 0u32);

        for line in geo_s.lines() {
            let line = line.trim();
            if let Some(pos) = line.strip_prefix("Position:") {
                let pos = pos.trim().split_whitespace().next().unwrap_or("");
                let parts: Vec<&str> = pos.split(',').collect();
                if parts.len() == 2 {
                    x = parts[0].parse().unwrap_or(0);
                    y = parts[1].parse().unwrap_or(0);
                }
            } else if let Some(geo) = line.strip_prefix("Geometry:") {
                let parts: Vec<&str> = geo.trim().split('x').collect();
                if parts.len() == 2 {
                    w = parts[0].parse().unwrap_or(0);
                    h = parts[1].parse().unwrap_or(0);
                }
            }
        }

        if w > 0 && h > 0 {
            wins.push(WinRect { name: cls, x, y, w, h });
        }
    }
    wins
}

// ── Overlay application ────────────────────────────────────

struct App {
    img: RgbaImage,
    wins: Vec<WinRect>,
    win: Option<Arc<Window>>,
    ctx: Option<softbuffer::Context<Arc<Window>>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    // Precomputed pixel buffers (surface resolution)
    dim_buf: Vec<u32>,
    bright_buf: Vec<u32>,
    buf_w: u32,
    buf_h: u32,
    // Mouse state
    mouse: PhysicalPosition<f64>,
    drag_from: Option<PhysicalPosition<f64>>,
    dragging: bool,
    scale: f64,
    result: Selection,
}

impl App {
    fn new(img: RgbaImage, wins: Vec<WinRect>) -> Self {
        Self {
            img,
            wins,
            win: None,
            ctx: None,
            surface: None,
            dim_buf: Vec::new(),
            bright_buf: Vec::new(),
            buf_w: 0,
            buf_h: 0,
            mouse: PhysicalPosition::new(0.0, 0.0),
            drag_from: None,
            dragging: false,
            scale: 1.0,
            result: Selection::Cancel,
        }
    }

    fn precompute(&mut self, width: u32, height: u32) {
        if self.buf_w == width && self.buf_h == height {
            return;
        }
        let iw = self.img.width();
        let ih = self.img.height();
        let size = (width as usize) * (height as usize);
        self.dim_buf = Vec::with_capacity(size);
        self.bright_buf = Vec::with_capacity(size);

        for py in 0..height {
            for px in 0..width {
                let sx = (px as u64 * iw as u64 / width as u64) as u32;
                let sy = (py as u64 * ih as u64 / height as u64) as u32;
                let p = self.img.get_pixel(sx.min(iw - 1), sy.min(ih - 1));
                self.bright_buf
                    .push((p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32);
                self.dim_buf.push(
                    (p[0] as u32 / 3) << 16 | (p[1] as u32 / 3) << 8 | (p[2] as u32 / 3),
                );
            }
        }
        self.buf_w = width;
        self.buf_h = height;
    }

    /// Find the KWin window under the cursor (physical coords → logical compare).
    fn window_at(&self, px: f64, py: f64) -> Option<&WinRect> {
        let lx = (px / self.scale) as i32;
        let ly = (py / self.scale) as i32;
        self.wins
            .iter()
            .rev()
            .find(|w| lx >= w.x && lx < w.x + w.w as i32 && ly >= w.y && ly < w.y + w.h as i32)
    }

    /// Current highlight rectangle in physical pixel coordinates.
    fn highlight(&self) -> Option<Rect> {
        if self.dragging {
            if let Some(s) = self.drag_from {
                let x1 = s.x.min(self.mouse.x).max(0.0) as u32;
                let y1 = s.y.min(self.mouse.y).max(0.0) as u32;
                let x2 = s.x.max(self.mouse.x) as u32;
                let y2 = s.y.max(self.mouse.y) as u32;
                return Some(Rect {
                    x: x1,
                    y: y1,
                    w: x2.saturating_sub(x1),
                    h: y2.saturating_sub(y1),
                });
            }
        }
        self.window_at(self.mouse.x, self.mouse.y)
            .map(|w| Rect {
                x: (w.x as f64 * self.scale) as u32,
                y: (w.y as f64 * self.scale) as u32,
                w: (w.w as f64 * self.scale) as u32,
                h: (w.h as f64 * self.scale) as u32,
            })
    }

    fn render(&mut self) {
        let Some(win) = self.win.as_ref() else {
            return;
        };
        let sz = win.inner_size();
        if sz.width == 0 || sz.height == 0 {
            return;
        }

        // Precompute buffers and get highlight BEFORE borrowing surface
        self.precompute(sz.width, sz.height);
        let hl = self.highlight();

        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let _ = surface.resize(
            NonZeroU32::new(sz.width).unwrap(),
            NonZeroU32::new(sz.height).unwrap(),
        );
        let Ok(mut buf) = surface.buffer_mut() else {
            return;
        };

        // Start with the dimmed image
        buf.copy_from_slice(&self.dim_buf);

        // Overdraw the highlight region with bright pixels + draw border
        if let Some(hl) = hl {
            let w = sz.width;
            let x2 = (hl.x + hl.w).min(sz.width);
            let y2 = (hl.y + hl.h).min(sz.height);

            // Bright region
            for py in hl.y..y2 {
                let row = (py * w) as usize;
                let start = row + hl.x as usize;
                let end = row + x2 as usize;
                buf[start..end].copy_from_slice(&self.bright_buf[start..end]);
            }

            // Border (2px, inside the highlight edge)
            let border_color: u32 = 0x00_44_88_FF;
            let t = 2u32;

            // Top border
            for py in hl.y..hl.y.saturating_add(t).min(y2) {
                for px in hl.x..x2 {
                    buf[(py * w + px) as usize] = border_color;
                }
            }
            // Bottom border
            for py in y2.saturating_sub(t)..y2 {
                for px in hl.x..x2 {
                    buf[(py * w + px) as usize] = border_color;
                }
            }
            // Left border
            for py in hl.y..y2 {
                for px in hl.x..hl.x.saturating_add(t).min(x2) {
                    buf[(py * w + px) as usize] = border_color;
                }
            }
            // Right border
            for py in hl.y..y2 {
                for px in x2.saturating_sub(t)..x2 {
                    buf[(py * w + px) as usize] = border_color;
                }
            }
        }

        let _ = buf.present();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.win.is_some() {
            return;
        }

        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_fullscreen(Some(Fullscreen::Borderless(None)))
                    .with_decorations(false)
                    .with_title("sshot"),
            )
            .unwrap(),
        );
        w.set_cursor(CursorIcon::Crosshair);
        self.scale = w.scale_factor();

        let ctx = softbuffer::Context::new(w.clone()).unwrap();
        let surface = softbuffer::Surface::new(&ctx, w.clone()).unwrap();
        self.ctx = Some(ctx);
        self.surface = Some(surface);
        self.win = Some(w);

        self.render();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),

            WindowEvent::RedrawRequested => self.render(),

            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed =>
            {
                if event.logical_key == Key::Named(NamedKey::Escape) {
                    self.result = Selection::Cancel;
                    el.exit();
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.mouse = position;
                if let Some(s) = self.drag_from {
                    if (position.x - s.x).abs() > 5.0 || (position.y - s.y).abs() > 5.0 {
                        self.dragging = true;
                    }
                }
                if let Some(w) = self.win.as_ref() {
                    w.request_redraw();
                }
            }

            WindowEvent::MouseInput { state, button, .. } => match (button, state) {
                (MouseButton::Left, ElementState::Pressed) => {
                    self.drag_from = Some(self.mouse);
                    self.dragging = false;
                }
                (MouseButton::Left, ElementState::Released) => {
                    if self.dragging {
                        // Region selection via drag
                        if let Some(s) = self.drag_from {
                            let x1 = s.x.min(self.mouse.x).max(0.0) as u32;
                            let y1 = s.y.min(self.mouse.y).max(0.0) as u32;
                            let x2 = s.x.max(self.mouse.x) as u32;
                            let y2 = s.y.max(self.mouse.y) as u32;
                            self.result = Selection::Region(Rect {
                                x: x1,
                                y: y1,
                                w: x2.saturating_sub(x1),
                                h: y2.saturating_sub(y1),
                            });
                        }
                    } else if let Some(w) =
                        self.window_at(self.mouse.x, self.mouse.y).cloned()
                    {
                        // Click on a window → capture that window
                        self.result = Selection::Window(
                            Rect {
                                x: (w.x as f64 * self.scale) as u32,
                                y: (w.y as f64 * self.scale) as u32,
                                w: (w.w as f64 * self.scale) as u32,
                                h: (w.h as f64 * self.scale) as u32,
                            },
                            w.name,
                        );
                    } else {
                        // Click on empty space → fullscreen
                        self.result = Selection::Full;
                    }
                    self.drag_from = None;
                    self.dragging = false;
                    el.exit();
                }
                (MouseButton::Right, ElementState::Pressed) => {
                    self.result = Selection::Cancel;
                    el.exit();
                }
                _ => {}
            },

            _ => {}
        }
    }
}

// ── Save & clipboard ───────────────────────────────────────

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn save_path(name: &str, dir: &str) -> PathBuf {
    let ts = Local::now().format("%Y-%m-%d_%H-%M-%S");
    let r: u16 = rand::thread_rng().gen_range(1000..9999);
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).ok();
    dir.join(format!("{}_{ts}_{r}.png", sanitize(name)))
}

fn clipboard(path: &std::path::Path) {
    match std::fs::File::open(path) {
        Ok(f) => {
            let status = Command::new("wl-copy")
                .args(["--type", "image/png"])
                .stdin(f)
                .status();
            match status {
                Ok(s) if s.success() => {}
                Ok(_) => eprintln!("wl-copy exited with error"),
                Err(e) => eprintln!("wl-copy not found: {e}"),
            }
        }
        Err(e) => eprintln!("clipboard: {e}"),
    }
}

fn crop_image(img: &RgbaImage, rect: &Rect) -> RgbaImage {
    let x = rect.x.min(img.width().saturating_sub(1));
    let y = rect.y.min(img.height().saturating_sub(1));
    let w = rect.w.min(img.width().saturating_sub(x)).max(1);
    let h = rect.h.min(img.height().saturating_sub(y)).max(1);
    image::imageops::crop_imm(img, x, y, w, h).to_image()
}

// ── Main ───────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::env::var("SSHOT_DIR").unwrap_or_else(|_| {
        format!(
            "{}/Pictures/Screenshots",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
        )
    });

    // 1. Capture the screen immediately (freeze) + get window list
    let rt = tokio::runtime::Runtime::new()?;
    let img = rt.block_on(capture_screen())?;
    drop(rt);
    let wins = get_windows();

    // 2. Show interactive overlay on the frozen screenshot
    let ev = EventLoop::new()?;
    let mut app = App::new(img, wins);
    ev.run_app(&mut app)?;

    // 3. Close overlay resources before saving
    app.surface = None;
    app.ctx = None;
    app.win = None;

    // 4. Process selection
    match &app.result {
        Selection::Cancel => {
            eprintln!("Cancelled");
        }
        Selection::Full => {
            let p = save_path("screen", &out_dir);
            app.img.save(&p)?;
            println!("{}", p.display());
            clipboard(&p);
        }
        Selection::Window(rect, name) => {
            let cropped = crop_image(&app.img, rect);
            let p = save_path(name, &out_dir);
            cropped.save(&p)?;
            println!("{}", p.display());
            clipboard(&p);
        }
        Selection::Region(rect) => {
            let cropped = crop_image(&app.img, rect);
            let p = save_path("region", &out_dir);
            cropped.save(&p)?;
            println!("{}", p.display());
            clipboard(&p);
        }
    }

    Ok(())
}
