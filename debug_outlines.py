#!/usr/bin/env python3
"""Debug: detect window positions from the focus ring color in a screenshot,
compare to computed positions, and draw both on the image."""
import json
import subprocess
from collections import defaultdict
from PIL import Image, ImageDraw

GAP = 16
# Niri focus ring colors (from user's config)
ACTIVE_RING = (0x7f, 0xc8, 0xff)   # #7fc8ff
INACTIVE_RING = (0x50, 0x50, 0x50)  # #505050

def run_json(args):
    r = subprocess.run(args, capture_output=True)
    return json.loads(r.stdout) if r.returncode == 0 else None

def is_ring_color(r, g, b, tolerance=25):
    """Check if a pixel matches the active or inactive focus ring color."""
    for ref in [ACTIVE_RING, INACTIVE_RING]:
        if abs(r-ref[0]) < tolerance and abs(g-ref[1]) < tolerance and abs(b-ref[2]) < tolerance:
            return True
    return False

def find_ring_rects(img, ox_phys, oy_phys, w_phys, h_phys):
    """Find rectangles bounded by focus ring pixels on one output."""
    # Scan for rows/cols that contain ring pixels
    ring_rows = set()
    ring_cols = set()

    # Sample every other pixel for speed
    for y in range(oy_phys, oy_phys + h_phys, 2):
        for x in range(ox_phys, ox_phys + w_phys, 2):
            r, g, b = img.getpixel((x, y))
            if is_ring_color(r, g, b):
                ring_rows.add(y)
                ring_cols.add(x)

    if not ring_rows or not ring_cols:
        return []

    # Find contiguous horizontal bands of ring rows
    sorted_rows = sorted(ring_rows)
    h_bands = []
    band_start = sorted_rows[0]
    prev = sorted_rows[0]
    for y in sorted_rows[1:]:
        if y - prev > 4:  # gap > 4px = new band
            h_bands.append((band_start, prev))
            band_start = y
        prev = y
    h_bands.append((band_start, prev))

    # Find contiguous vertical bands of ring cols
    sorted_cols = sorted(ring_cols)
    v_bands = []
    band_start = sorted_cols[0]
    prev = sorted_cols[0]
    for x in sorted_cols[1:]:
        if x - prev > 4:
            v_bands.append((band_start, prev))
            band_start = x
        prev = x
    v_bands.append((band_start, prev))

    return h_bands, v_bands

def compute_rects(gap):
    windows = run_json(["niri", "msg", "--json", "windows"]) or []
    workspaces = run_json(["niri", "msg", "--json", "workspaces"]) or []
    outputs = run_json(["niri", "msg", "--json", "outputs"]) or {}

    ws_output = {}
    for ws in workspaces:
        if not ws.get("is_active"):
            continue
        ws_id = ws["id"]
        out_name = ws.get("output", "")
        if out_name in outputs:
            l = outputs[out_name]["logical"]
            ws_output[ws_id] = (l["x"], l["y"], l["width"], l["height"], out_name)

    by_ws = defaultdict(list)
    for w in windows:
        ws_id = w.get("workspace_id")
        if ws_id in ws_output:
            by_ws[ws_id].append(w)

    return by_ws, ws_output, outputs

def main():
    subprocess.run(["grim", "-t", "png", "/tmp/debug_base.png"], check=True)
    img = Image.open("/tmp/debug_base.png")
    draw = ImageDraw.Draw(img)
    print(f"Screenshot: {img.size}")

    by_ws, ws_output, outputs = compute_rects(GAP)

    for ws_id, wins in by_ws.items():
        ox, oy, ow, oh, out_name = ws_output[ws_id]
        out = outputs[out_name]
        scale = out["logical"]["scale"]
        ox_phys = int(ox * scale)
        oy_phys = int(oy * scale)
        w_phys = int(ow * scale)
        h_phys = int(oh * scale)

        print(f"\n=== {out_name} (ws={ws_id}) ===")

        # Detect focus ring positions
        result = find_ring_rects(img, ox_phys, oy_phys, w_phys, h_phys)
        if not result:
            print("  No ring pixels found")
            continue

        h_bands, v_bands = result
        print(f"  Horizontal ring bands (logical from output top):")
        for s, e in h_bands:
            print(f"    {(s-oy_phys)/scale:.1f} - {(e-oy_phys)/scale:.1f} (width {(e-s)/scale:.1f})")
        print(f"  Vertical ring bands (logical from output left):")
        for s, e in v_bands:
            print(f"    {(s-ox_phys)/scale:.1f} - {(e-ox_phys)/scale:.1f} (width {(e-s)/scale:.1f})")

        # Draw detected ring bands as green lines
        for s, e in h_bands:
            draw.line([(ox_phys, s), (ox_phys + w_phys, s)], fill=(0, 255, 0), width=1)
            draw.line([(ox_phys, e), (ox_phys + w_phys, e)], fill=(0, 255, 0), width=1)
        for s, e in v_bands:
            draw.line([(s, oy_phys), (s, oy_phys + h_phys)], fill=(0, 255, 0), width=1)
            draw.line([(e, oy_phys), (e, oy_phys + h_phys)], fill=(0, 255, 0), width=1)

        # Now compute positions using current algorithm
        columns = defaultdict(list)
        for w in wins:
            if w.get("is_floating"):
                continue
            pos = w["layout"].get("pos_in_scrolling_layout")
            if pos:
                columns[pos[0]].append(w)

        for col in columns.values():
            col.sort(key=lambda w: w["layout"]["pos_in_scrolling_layout"][1])

        col_data = []
        for col_idx in sorted(columns.keys()):
            col = columns[col_idx]
            width = max(w["layout"]["tile_size"][0] for w in col)
            rows = [(w["layout"]["tile_size"][1], w) for w in col]
            col_data.append((col_idx, width, rows))

        if not col_data:
            continue

        col_scroll_x = []
        sx = 0.0
        for i, (_, col_w, _) in enumerate(col_data):
            col_scroll_x.append(sx)
            sx += col_w
            if i + 1 < len(col_data):
                sx += GAP
        total_scroll_w = sx

        scroll_offset = -(ow - total_scroll_w) / 2.0 if total_scroll_w <= ow else 0

        max_used_h = max(
            sum(th for th, _ in rows) + max(0, len(rows) - 1) * GAP
            for _, _, rows in col_data
        )
        bar_h = max(0, oh - max_used_h)

        print(f"\n  Computed: bar_h={bar_h:.1f} scroll_offset={scroll_offset:.1f}")

        for i, (_, col_w, rows) in enumerate(col_data):
            screen_x = ox + col_scroll_x[i] - scroll_offset
            screen_y = oy + bar_h
            for tile_h, w in rows:
                win_w = w["layout"]["window_size"][0]
                win_h = w["layout"]["window_size"][1]

                px = int(screen_x * scale)
                py = int(screen_y * scale)
                pw = int(win_w * scale)
                ph = int(win_h * scale)

                # Draw computed rect in blue
                for b in range(3):
                    draw.rectangle([px-b, py-b, px+pw+b, py+ph+b], outline=(80, 140, 255))

                print(f"  {w['title'][:25]:25s} computed=({screen_x-ox:.1f}, {screen_y-oy:.1f}) size={win_w}x{win_h}")

                screen_y += tile_h + GAP

    # Save per-output crops
    for name, out in outputs.items():
        l = out["logical"]
        s = l.get("scale", 1.5)
        x1, y1 = int(l["x"] * s), int(l["y"] * s)
        x2, y2 = int((l["x"] + l["width"]) * s), int((l["y"] + l["height"]) * s)
        img.crop((x1, y1, x2, y2)).save(f"/tmp/debug_{name}.png")
        print(f"\nSaved: /tmp/debug_{name}.png")

if __name__ == "__main__":
    main()
