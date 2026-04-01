use std::collections::{BTreeMap, HashMap};
use std::process::Command;

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
    pub id: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone)]
pub enum Selection {
    Window { title: String, id: String },
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
    normal: Vec<u8>,
    dimmed: Vec<u8>,
    configured: bool,
    needs_redraw: bool,
    frame_pending: bool,
}

// ── Window list via Niri IPC ────────────────────────────────────

fn get_windows(gap: f64, ring: f64) -> Result<Vec<WindowInfo>> {
    let win_out = Command::new("niri").args(["msg", "--json", "windows"]).output()
        .context("niri msg windows failed")?;
    if !win_out.status.success() { return Ok(Vec::new()); }
    let windows: Vec<serde_json::Value> = serde_json::from_slice(&win_out.stdout)?;

    let ws_out = Command::new("niri").args(["msg", "--json", "workspaces"]).output()
        .context("niri msg workspaces failed")?;
    let workspaces: Vec<serde_json::Value> = serde_json::from_slice(&ws_out.stdout)?;

    let out_out = Command::new("niri").args(["msg", "--json", "outputs"]).output()
        .context("niri msg outputs failed")?;
    let outputs: HashMap<String, serde_json::Value> = serde_json::from_slice(&out_out.stdout)?;

    // Map active workspace → output logical rect
    let mut ws_output: HashMap<u64, (f64, f64, f64, f64)> = HashMap::new();
    for ws in &workspaces {
        if !ws["is_active"].as_bool().unwrap_or(false) { continue; }
        let ws_id = ws["id"].as_u64().unwrap_or(0);
        let out_name = ws["output"].as_str().unwrap_or("");
        if let Some(out) = outputs.get(out_name) {
            let l = &out["logical"];
            ws_output.insert(ws_id, (
                l["x"].as_f64().unwrap_or(0.0),
                l["y"].as_f64().unwrap_or(0.0),
                l["width"].as_f64().unwrap_or(0.0),
                l["height"].as_f64().unwrap_or(0.0),
            ));
        }
    }

    // Only keep windows on active workspaces
    let mut by_ws: HashMap<u64, Vec<&serde_json::Value>> = HashMap::new();
    for w in &windows {
        let ws_id = w["workspace_id"].as_u64().unwrap_or(0);
        if ws_output.contains_key(&ws_id) {
            by_ws.entry(ws_id).or_default().push(w);
        }
    }

    let mut result = Vec::new();

    for (ws_id, wins) in &by_ws {
        let (ox, oy, ow, oh) = ws_output[ws_id];

        // Floating windows: use tile_pos_in_workspace_view (always populated for floats)
        for w in wins.iter().filter(|w| w["is_floating"].as_bool().unwrap_or(false)) {
            let layout = &w["layout"];
            let pos = &layout["tile_pos_in_workspace_view"];
            if pos.is_null() { continue; }
            let off_x = layout["window_offset_in_tile"][0].as_f64().unwrap_or(0.0);
            let off_y = layout["window_offset_in_tile"][1].as_f64().unwrap_or(0.0);
            let size = &layout["window_size"];
            result.push(WindowInfo {
                title: w["title"].as_str().unwrap_or("").to_string(),
                id: w["id"].as_u64().unwrap_or(0).to_string(),
                x: ox + pos[0].as_f64().unwrap_or(0.0) + off_x,
                y: oy + pos[1].as_f64().unwrap_or(0.0) + off_y,
                width: size[0].as_f64().unwrap_or(0.0),
                height: size[1].as_f64().unwrap_or(0.0),
            });
        }

        // Tiled windows: reconstruct positions from scrolling layout
        let mut columns: BTreeMap<u64, Vec<&serde_json::Value>> = BTreeMap::new();
        for w in wins.iter().filter(|w| !w["is_floating"].as_bool().unwrap_or(false)) {
            let col = w["layout"]["pos_in_scrolling_layout"][0].as_u64().unwrap_or(0);
            columns.entry(col).or_default().push(w);
        }
        if columns.is_empty() { continue; }
        for col in columns.values_mut() {
            col.sort_by_key(|w| w["layout"]["pos_in_scrolling_layout"][1].as_u64().unwrap_or(0));
        }

        let col_data: Vec<(u64, f64, Vec<(f64, &serde_json::Value)>)> = columns.iter().map(|(&col_idx, col)| {
            let width = col.iter()
                .map(|w| w["layout"]["tile_size"][0].as_f64().unwrap_or(0.0))
                .fold(0.0f64, f64::max);
            let rows: Vec<_> = col.iter()
                .map(|w| (w["layout"]["tile_size"][1].as_f64().unwrap_or(0.0), *w))
                .collect();
            (col_idx, width, rows)
        }).collect();

        // The visual gap between tile content = gap - 2*ring (focus ring eats
        // into the gap on each side). Use this for inter-tile spacing.
        let visual_gap = (gap - 2.0 * ring).max(0.0);

        // Compute column positions using the visual gap
        let mut col_scroll_x: Vec<f64> = Vec::new();
        let mut sx = 0.0;
        for (i, (_, col_w, _)) in col_data.iter().enumerate() {
            col_scroll_x.push(sx);
            sx += col_w;
            if i + 1 < col_data.len() { sx += visual_gap; }
        }
        let total_scroll_w = sx;

        let scroll_offset = if total_scroll_w <= ow {
            -(ow - total_scroll_w) / 2.0
        } else {
            let focused_col_idx = wins.iter()
                .find(|w| w["is_focused"].as_bool().unwrap_or(false))
                .and_then(|w| w["layout"]["pos_in_scrolling_layout"][0].as_u64());
            let focused_local = focused_col_idx
                .and_then(|idx| col_data.iter().position(|(ci, _, _)| *ci == idx))
                .unwrap_or(0);
            let fx = col_scroll_x[focused_local];
            let fw = col_data[focused_local].1;
            (fx + fw / 2.0 - ow / 2.0).max(0.0).min(total_scroll_w - ow)
        };

        // Infer vertical offset (bar/panel height) from tallest column.
        let max_used_h = col_data.iter().map(|(_, _, rows)| {
            let h: f64 = rows.iter().map(|(th, _)| *th).sum();
            h + rows.len().saturating_sub(1) as f64 * gap
        }).fold(0.0f64, f64::max);
        let bar_h = (oh - max_used_h).max(0.0);

        for (i, (_, _col_w, rows)) in col_data.iter().enumerate() {
            let screen_x = ox + col_scroll_x[i] - scroll_offset;
            let mut screen_y = oy + bar_h;
            for (tile_h, w) in rows {
                let layout = &w["layout"];
                let win_w = layout["window_size"][0].as_f64().unwrap_or(0.0);
                let win_h = layout["window_size"][1].as_f64().unwrap_or(0.0);
                let off_x = layout["window_offset_in_tile"][0].as_f64().unwrap_or(0.0);
                let off_y = layout["window_offset_in_tile"][1].as_f64().unwrap_or(0.0);

                let wx = screen_x + off_x;
                let wy = screen_y + off_y;
                if wx + win_w > ox && wx < ox + ow && wy + win_h > oy && wy < oy + oh {
                    result.push(WindowInfo {
                        title: w["title"].as_str().unwrap_or("").to_string(),
                        id: w["id"].as_u64().unwrap_or(0).to_string(),
                        x: wx, y: wy,
                        width: win_w, height: win_h,
                    });
                }

                screen_y += tile_h + visual_gap;
            }
        }
    }

    Ok(result)
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
            let kb = KeyboardInteractivity::Exclusive;
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
                phys_w, phys_h, scale,
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
                self.selection = Some(Selection::Window { title: w.title.clone(), id: w.id.clone() });
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
    let gap = config.appearance.tiling_gap as f64;
    let ring = config.appearance.focus_ring_width as f64;
    let win_handle = std::thread::spawn(move || get_windows(gap, ring));
    let (ss_data, ss_width, _ss_height, ss_stride) =
        crate::capture::capture_workspace().context("Failed to capture screenshot")?;
    let windows = win_handle.join()
        .map_err(|_| anyhow::anyhow!("Window list thread panicked"))?
        .unwrap_or_else(|e| { eprintln!("Warning: window detection failed: {e}"); Vec::new() });

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
