use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};

use crate::config::Config;

const DRAG_THRESHOLD: f64 = 5.0;

// ── Public types ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub title: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone)]
pub enum Selection {
    Window { x: f64, y: f64, w: f64, h: f64, title: String },
    Region { x: f64, y: f64, w: f64, h: f64 },
}

/// Result of a screenshot overlay session.
pub struct CaptureResult {
    /// Raw BGRA pixel data of the full workspace at physical resolution.
    pub ss_data: Vec<u8>,
    pub ss_width: u32,
    pub ss_stride: u32,
    /// What the user selected.
    pub selection: Selection,
    /// Scale factor (physical / logical).
    pub scale: f64,
}

// ── Internal types ───────────────────────────────────────────────

struct OutputSurface {
    layer: LayerSurface,
    wl_surface: wl_surface::WlSurface,
    viewport: Option<wp_viewport::WpViewport>,
    pool: SlotPool,
    logical_x: i32,
    logical_y: i32,
    logical_w: u32,
    logical_h: u32,
    phys_w: u32,
    phys_h: u32,
    scale: f64,
    ss_offset_x: u32,
    ss_offset_y: u32,
    normal: Vec<u8>,
    dimmed: Vec<u8>,
    configured: bool,
    needs_redraw: bool,
    frame_pending: bool,
}

// ── Capture ──────────────────────────────────────────────────────

fn capture_workspace() -> Result<(Vec<u8>, u32, u32, u32)> {
    let tmp = std::env::temp_dir().join(format!("ss_capture_{}.png", std::process::id()));
    let status = Command::new("spectacle")
        .args(["-f", "-b", "-n", "-o", tmp.to_str().unwrap()])
        .status()
        .context("Failed to run spectacle")?;
    if !status.success() { anyhow::bail!("spectacle exited with {status}"); }
    if !tmp.exists() { anyhow::bail!("spectacle did not produce output"); }

    let img = image::open(&tmp).context("Failed to decode screenshot PNG")?;
    let _ = std::fs::remove_file(&tmp);
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let stride = width * 4;

    let mut data = rgba.into_raw();
    for pixel in data.chunks_exact_mut(4) { pixel.swap(0, 2); }
    Ok((data, width, height, stride))
}

fn get_windows() -> Result<Vec<WindowInfo>> {
    let token = format!("WD{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos());
    let script = format!(
        r#"var out = [];
var wins = workspace.stackingOrder;
for (var i = 0; i < wins.length; i++) {{
    var w = wins[i];
    if (!w.minimized && w.normalWindow) {{
        var g = w.frameGeometry;
        out.push(JSON.stringify({{t: w.caption, x: g.x, y: g.y, w: g.width, h: g.height}}));
    }}
}}
console.log("{token}:[" + out.join(",") + "]");"#
    );
    let script_path = std::env::temp_dir().join(format!("kwin_ss_{token}.js"));
    std::fs::write(&script_path, &script)?;
    let script_name = format!("ss_{token}");

    let load_out = Command::new("qdbus")
        .args(["org.kde.KWin", "/Scripting", "loadScript", script_path.to_str().unwrap(), &script_name])
        .output().context("Failed to run qdbus")?;
    let sid = String::from_utf8_lossy(&load_out.stdout).trim().to_string();
    let _ = Command::new("qdbus").args(["org.kde.KWin", &format!("/Scripting/Script{sid}"), "run"]).output();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let journal = Command::new("journalctl")
        .args(["--user", "-t", "kwin_wayland", "-n", "50", "--output=cat", "--no-pager", "--since", "-5s"])
        .output().context("journalctl failed")?;
    let _ = Command::new("qdbus").args(["org.kde.KWin", &format!("/Scripting/Script{sid}"), "stop"]).output();
    let _ = Command::new("qdbus").args(["org.kde.KWin", "/Scripting", "unloadScript", &script_name]).output();
    let _ = std::fs::remove_file(&script_path);

    let stdout = String::from_utf8_lossy(&journal.stdout);
    for line in stdout.lines() {
        if let Some(json_str) = line.split(&format!("{token}:")).nth(1) {
            let arr: Vec<serde_json::Value> = serde_json::from_str(json_str.trim())?;
            return Ok(arr.iter().map(|item| WindowInfo {
                title: item["t"].as_str().unwrap_or("").to_string(),
                x: item["x"].as_f64().unwrap_or(0.0), y: item["y"].as_f64().unwrap_or(0.0),
                width: item["w"].as_f64().unwrap_or(0.0), height: item["h"].as_f64().unwrap_or(0.0),
            }).collect());
        }
    }
    Ok(Vec::new())
}

// ── Rendering ────────────────────────────────────────────────────

fn precompute_overlay(
    ss_data: &[u8], ss_stride: u32, ss_width: u32,
    ss_offset_x: u32, ss_offset_y: u32,
    phys_w: u32, phys_h: u32, dim_factor: f32,
) -> (Vec<u8>, Vec<u8>) {
    let size = (phys_w * phys_h * 4) as usize;
    let mut normal = vec![0u8; size];
    let mut dimmed = vec![0u8; size];
    let ss_h = ss_data.len() as u32 / ss_stride;

    for py in 0..phys_h {
        let sy = ss_offset_y + py;
        if sy >= ss_h { break; }
        for px in 0..phys_w {
            let sx = ss_offset_x + px;
            if sx >= ss_width { break; }
            let si = (sy * ss_stride + sx * 4) as usize;
            let di = ((py * phys_w + px) * 4) as usize;
            if si + 3 < ss_data.len() {
                normal[di..di + 4].copy_from_slice(&ss_data[si..si + 4]);
                dimmed[di] = (ss_data[si] as f32 * dim_factor) as u8;
                dimmed[di + 1] = (ss_data[si + 1] as f32 * dim_factor) as u8;
                dimmed[di + 2] = (ss_data[si + 2] as f32 * dim_factor) as u8;
                dimmed[di + 3] = ss_data[si + 3];
            }
        }
    }
    (normal, dimmed)
}

fn render(
    canvas: &mut [u8], normal: &[u8], dimmed: &[u8],
    width: u32, height: u32,
    highlight: Option<(i32, i32, u32, u32)>,
    border_color: [u8; 4], border_width: u32,
) {
    let len = canvas.len().min(dimmed.len());
    canvas[..len].copy_from_slice(&dimmed[..len]);

    if let Some((hx_raw, hy_raw, hw, hh)) = highlight {
        // Compute hx2/hy2 from raw coords BEFORE clamping hx/hy (cross-monitor correctness)
        let hx2 = (hx_raw as i64 + hw as i64).max(0).min(width as i64) as u32;
        let hy2 = (hy_raw as i64 + hh as i64).max(0).min(height as i64) as u32;
        let hx = (hx_raw.max(0) as u32).min(width);
        let hy = (hy_raw.max(0) as u32).min(height);
        if hx >= hx2 || hy >= hy2 { return; }

        for y in hy..hy2 {
            let start = ((y * width + hx) * 4) as usize;
            let end = ((y * width + hx2) * 4) as usize;
            if start < end && end <= normal.len() && end <= canvas.len() {
                canvas[start..end].copy_from_slice(&normal[start..end]);
            }
        }

        let bw = border_width;
        let bx1 = hx.saturating_sub(bw);
        let by1 = hy.saturating_sub(bw);
        let bx2 = (hx2 + bw).min(width);
        let by2 = (hy2 + bw).min(height);
        for y in by1..by2 {
            for x in bx1..bx2 {
                if !(x >= hx && x < hx2 && y >= hy && y < hy2) {
                    let idx = ((y * width + x) * 4) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx..idx + 4].copy_from_slice(&border_color);
                    }
                }
            }
        }
    }
}

// ── Wayland App ──────────────────────────────────────────────────

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    viewporter: Option<wp_viewporter::WpViewporter>,
    config: Config,

    ss_data: Vec<u8>,
    ss_width: u32,
    ss_stride: u32,
    windows: Vec<WindowInfo>,
    surfaces: Vec<OutputSurface>,

    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    mouse_global: (f64, f64),
    pressed: bool,
    dragging: bool,
    press_global: (f64, f64),
    drag_start: (f64, f64),
    drag_end: (f64, f64),
    hovered_window: Option<usize>,

    global_scale: f64,
    exit: bool,
    selection: Option<Selection>,
}

impl App {
    fn create_overlays(&mut self, qh: &QueueHandle<Self>) {
        let outputs: Vec<_> = self.output_state.outputs()
            .map(|o| (o.clone(), self.output_state.info(&o)))
            .collect();

        let dim = self.config.appearance.dim_factor;

        for (output, info) in &outputs {
            let info = match info { Some(i) => i, None => continue };
            let (phys_w, phys_h) = info.modes.iter().find(|m| m.current)
                .map(|m| (m.dimensions.0 as u32, m.dimensions.1 as u32))
                .unwrap_or((3840, 2160));
            let (log_w, log_h) = info.logical_size.map(|(w, h)| (w as u32, h as u32)).unwrap_or((phys_w, phys_h));
            let (log_x, log_y) = info.logical_position.unwrap_or((0, 0));
            let scale = if log_w > 0 { phys_w as f64 / log_w as f64 } else { 1.0 };
            if self.global_scale == 0.0 { self.global_scale = scale; }

            let ss_offset_x = (log_x as f64 * scale).round() as u32;
            let ss_offset_y = (log_y as f64 * scale).round() as u32;

            let (normal, dimmed) = precompute_overlay(
                &self.ss_data, self.ss_stride, self.ss_width,
                ss_offset_x, ss_offset_y, phys_w, phys_h, dim,
            );

            let surface = self.compositor.create_surface(qh);
            let layer = self.layer_shell.create_layer_surface(
                qh, surface.clone(), Layer::Overlay, Some("screenshot"), Some(output),
            );
            layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            layer.set_size(0, 0);
            let kb = if self.surfaces.is_empty() { KeyboardInteractivity::Exclusive } else { KeyboardInteractivity::None };
            layer.set_keyboard_interactivity(kb);
            layer.set_exclusive_zone(-1);
            layer.commit();

            let viewport = self.viewporter.as_ref().map(|vp| {
                let v = vp.get_viewport(&surface, qh, ());
                v.set_destination(log_w as i32, log_h as i32);
                v
            });

            let pool_size = (phys_w as usize * phys_h as usize * 4 * 3).max(1);
            let pool = SlotPool::new(pool_size, &self.shm).expect("Failed to create SHM pool");

            self.surfaces.push(OutputSurface {
                layer, wl_surface: surface, viewport, pool,
                logical_x: log_x, logical_y: log_y, logical_w: log_w, logical_h: log_h,
                phys_w, phys_h, scale, ss_offset_x, ss_offset_y,
                normal, dimmed, configured: false, needs_redraw: false, frame_pending: false,
            });
        }
    }

    fn window_at(&self, gx: f64, gy: f64) -> Option<usize> {
        for (i, w) in self.windows.iter().enumerate().rev() {
            if gx >= w.x && gx < w.x + w.width && gy >= w.y && gy < w.y + w.height {
                return Some(i);
            }
        }
        None
    }

    fn highlight_for(&self, idx: usize) -> Option<((i32, i32, u32, u32), [u8; 4])> {
        let surf = &self.surfaces[idx];
        let s = surf.scale;
        let ap = &self.config.appearance;
        let win_border = [ap.window_border_color[2], ap.window_border_color[1], ap.window_border_color[0], 255];
        let reg_border = [ap.region_border_color[2], ap.region_border_color[1], ap.region_border_color[0], 255];

        if self.dragging {
            let (sx, sy) = self.drag_start;
            let (ex, ey) = self.drag_end;
            let x = sx.min(ex); let y = sy.min(ey);
            let w = (sx - ex).abs(); let h = (sy - ey).abs();
            if w < 1.0 || h < 1.0 { return None; }
            let lx = ((x - surf.logical_x as f64) * s) as i32;
            let ly = ((y - surf.logical_y as f64) * s) as i32;
            Some(((lx, ly, (w * s) as u32, (h * s) as u32), reg_border))
        } else if let Some(widx) = self.hovered_window {
            let w = &self.windows[widx];
            let lx = ((w.x - surf.logical_x as f64) * s) as i32;
            let ly = ((w.y - surf.logical_y as f64) * s) as i32;
            Some(((lx, ly, (w.width * s) as u32, (w.height * s) as u32), win_border))
        } else {
            None
        }
    }

    fn draw_surface(&mut self, idx: usize, qh: &QueueHandle<Self>) {
        if !self.surfaces[idx].configured { return; }
        let phys_w = self.surfaces[idx].phys_w;
        let phys_h = self.surfaces[idx].phys_h;
        let stride = phys_w * 4;
        let border_width = self.config.appearance.border_width;

        let highlight = self.highlight_for(idx);
        let (hl_rect, border_color) = match highlight {
            Some((r, c)) => (Some(r), c),
            None => (None, [0; 4]),
        };

        let surface = &mut self.surfaces[idx];
        let (buffer, canvas) = match surface.pool.create_buffer(
            phys_w as i32, phys_h as i32, stride as i32, wl_shm::Format::Argb8888,
        ) {
            Ok(bc) => bc,
            Err(_) => {
                surface.pool.resize((phys_w as usize * phys_h as usize * 4 * 3).max(1)).ok();
                match surface.pool.create_buffer(phys_w as i32, phys_h as i32, stride as i32, wl_shm::Format::Argb8888) {
                    Ok(bc) => bc,
                    Err(_) => return,
                }
            }
        };

        render(canvas, &surface.normal, &surface.dimmed, phys_w, phys_h, hl_rect, border_color, border_width);
        surface.wl_surface.attach(Some(buffer.wl_buffer()), 0, 0);
        surface.wl_surface.damage_buffer(0, 0, phys_w as i32, phys_h as i32);
        surface.wl_surface.frame(qh, surface.wl_surface.clone());
        surface.wl_surface.commit();
        surface.needs_redraw = false;
        surface.frame_pending = true;
    }

    /// Mark all surfaces dirty and draw any that aren't waiting for a frame callback.
    fn request_redraw(&mut self, qh: &QueueHandle<Self>) {
        for i in 0..self.surfaces.len() {
            self.surfaces[i].needs_redraw = true;
        }
        for i in 0..self.surfaces.len() {
            if self.surfaces[i].needs_redraw && !self.surfaces[i].frame_pending {
                self.draw_surface(i, qh);
            }
        }
    }

    fn handle_selection(&mut self) {
        if self.dragging {
            let (sx, sy) = self.drag_start;
            let (ex, ey) = self.drag_end;
            let x = sx.min(ex); let y = sy.min(ey);
            let w = (sx - ex).abs(); let h = (sy - ey).abs();
            if w > 1.0 && h > 1.0 {
                self.selection = Some(Selection::Region { x, y, w, h });
            }
        } else {
            let (gx, gy) = self.mouse_global;
            if let Some(idx) = self.window_at(gx, gy) {
                let w = &self.windows[idx];
                self.selection = Some(Selection::Window { x: w.x, y: w.y, w: w.width, h: w.height, title: w.title.clone() });
            }
        }
        self.exit = true;
    }
}

// ── SCTK Handlers ────────────────────────────────────────────────

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, surface: &wl_surface::WlSurface, _: u32) {
        if let Some(idx) = self.surfaces.iter().position(|s| &s.wl_surface == surface) {
            self.surfaces[idx].frame_pending = false;
            if self.surfaces[idx].needs_redraw {
                self.draw_surface(idx, qh);
            }
        }
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState { &mut self.seat_state }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: smithay_client_toolkit::seat::Capability) {
        use smithay_client_toolkit::seat::Capability;
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: smithay_client_toolkit::seat::Capability) {
        use smithay_client_toolkit::seat::Capability;
        if capability == Capability::Keyboard { self.keyboard.take(); }
        if capability == Capability::Pointer { self.pointer.take(); }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            let surf_idx = match self.surfaces.iter().position(|s| s.wl_surface == event.surface) {
                Some(i) => i, None => continue,
            };
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.mouse_global = (
                        event.position.0 + self.surfaces[surf_idx].logical_x as f64,
                        event.position.1 + self.surfaces[surf_idx].logical_y as f64,
                    );
                    if self.pressed {
                        if !self.dragging {
                            let dx = self.mouse_global.0 - self.press_global.0;
                            let dy = self.mouse_global.1 - self.press_global.1;
                            if dx.abs() > DRAG_THRESHOLD || dy.abs() > DRAG_THRESHOLD { self.dragging = true; }
                        }
                        if self.dragging { self.drag_end = self.mouse_global; self.request_redraw(qh); }
                    } else {
                        let old = self.hovered_window;
                        self.hovered_window = self.window_at(self.mouse_global.0, self.mouse_global.1);
                        if old != self.hovered_window { self.request_redraw(qh); }
                    }
                }
                PointerEventKind::Press { button: 272, .. } => {
                    self.pressed = true;
                    self.press_global = self.mouse_global;
                    self.drag_start = self.mouse_global;
                    self.drag_end = self.mouse_global;
                }
                PointerEventKind::Release { button: 272, .. } => { self.handle_selection(); }
                PointerEventKind::Press { button: 273, .. } => { self.exit = true; }
                _ => {}
            }
        }
    }
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        if event.keysym.raw() == 0xff1b || event.raw_code == 1 { self.exit = true; }
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: Modifiers, _: u32) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) { self.exit = true; }
    fn configure(&mut self, _: &Connection, qh: &QueueHandle<Self>, layer: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        if let Some(idx) = self.surfaces.iter().position(|s| &s.layer == layer) {
            let (w, h) = configure.new_size;
            if w > 0 && h > 0 {
                self.surfaces[idx].logical_w = w;
                self.surfaces[idx].logical_h = h;
                if let Some(ref vp) = self.surfaces[idx].viewport {
                    vp.set_destination(w as i32, h as i32);
                }
            }
            self.surfaces[idx].configured = true;
            self.draw_surface(idx, qh);
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers![OutputState, SeatState];
}

impl Dispatch<wp_viewporter::WpViewporter, ()> for App {
    fn event(_: &mut Self, _: &wp_viewporter::WpViewporter, _: wp_viewporter::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wp_viewport::WpViewport, ()> for App {
    fn event(_: &mut Self, _: &wp_viewport::WpViewport, _: wp_viewport::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_compositor!(App);
delegate_output!(App);
delegate_seat!(App);
delegate_pointer!(App);
delegate_keyboard!(App);
delegate_layer!(App);
delegate_shm!(App);
delegate_registry!(App);

// ── Public entry point ───────────────────────────────────────────

/// Run the screenshot overlay. Returns a CaptureResult if the user made a selection.
pub fn run(config: &Config) -> Result<Option<CaptureResult>> {
    let win_handle = std::thread::spawn(get_windows);
    let (ss_data, ss_width, _ss_height, ss_stride) =
        capture_workspace().context("Failed to capture screenshot")?;
    let windows = win_handle.join()
        .map_err(|_| anyhow::anyhow!("Window list thread panicked"))?
        .context("Failed to get window list")?;

    let conn = Connection::connect_to_env().context("No Wayland connection")?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    let seat_state = SeatState::new(&globals, &qh);
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    let viewporter = globals.bind::<wp_viewporter::WpViewporter, _, _>(&qh, 1..=1, ()).ok();

    let mut app = App {
        registry_state, seat_state, output_state, compositor, layer_shell, shm, viewporter,
        config: config.clone(),
        ss_data, ss_width, ss_stride,
        windows, surfaces: Vec::new(),
        keyboard: None, pointer: None,
        mouse_global: (0.0, 0.0), pressed: false, dragging: false,
        press_global: (0.0, 0.0), drag_start: (0.0, 0.0), drag_end: (0.0, 0.0),
        hovered_window: None, global_scale: 0.0,
        exit: false, selection: None,
    };

    event_queue.roundtrip(&mut app)?;
    app.create_overlays(&qh);
    event_queue.roundtrip(&mut app)?;

    loop {
        event_queue.blocking_dispatch(&mut app)?;
        if app.exit { break; }
    }

    match app.selection {
        Some(sel) => Ok(Some(CaptureResult {
            ss_data: app.ss_data, ss_width: app.ss_width, ss_stride: app.ss_stride,
            selection: sel, scale: app.global_scale,
        })),
        None => Ok(None),
    }
}
