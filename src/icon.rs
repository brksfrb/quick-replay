// The app icon, drawn in code so there is no binary asset to keep in sync.
// Shared by build.rs (exe icon) and the tray (via `include!`), so it only uses `std`.

/// Renders the icon as straight (non-premultiplied) RGBA, `size * size * 4` bytes.
/// `badge` adds a small recording dot in the corner.
pub fn render_icon(size: u32, badge: bool) -> Vec<u8> {
    const SUBSAMPLES: u32 = 4;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    let n = f32::from(SUBSAMPLES as u16);
    for py in 0..size {
        for px in 0..size {
            let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..SUBSAMPLES {
                for sx in 0..SUBSAMPLES {
                    let x = (px as f32 + (sx as f32 + 0.5) / n) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / n) / size as f32;
                    if let Some([cr, cg, cb]) = icon_color(x, y, badge) {
                        r += cr;
                        g += cg;
                        b += cb;
                        a += 1.0;
                    }
                }
            }
            if a > 0.0 {
                rgba.extend_from_slice(&[(r / a) as u8, (g / a) as u8, (b / a) as u8, (a / (n * n) * 255.0) as u8]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    rgba
}

fn icon_color(x: f32, y: f32, badge: bool) -> Option<[f32; 3]> {
    if badge {
        let d = ((x - 0.80).powi(2) + (y - 0.80).powi(2)).sqrt();
        if d < 0.13 {
            return Some([255.0, 59.0, 48.0]);
        }
        if d < 0.19 {
            return Some([255.0, 255.0, 255.0]);
        }
    }
    if !in_rounded_square(x, y, 0.03, 0.23) {
        return None;
    }
    if in_symbol(x, y) {
        return Some([255.0, 255.0, 255.0]);
    }
    // Diagonal coral-to-raspberry gradient.
    let t = ((x + y) / 2.0).clamp(0.0, 1.0);
    Some([255.0 - 23.0 * t, 106.0 - 70.0 * t, 92.0 - 6.0 * t])
}

fn in_rounded_square(x: f32, y: f32, margin: f32, radius: f32) -> bool {
    let (lo, hi) = (margin + radius, 1.0 - margin - radius);
    let dx = (lo - x).max(x - hi).max(0.0);
    let dy = (lo - y).max(y - hi).max(0.0);
    x >= margin && x <= 1.0 - margin && y >= margin && y <= 1.0 - margin && dx * dx + dy * dy <= radius * radius
}

/// A counter-clockwise "replay" arrow around a play triangle.
fn in_symbol(x: f32, y: f32) -> bool {
    const CX: f32 = 0.5;
    const CY: f32 = 0.5;
    const RING: f32 = 0.27;
    const THICK: f32 = 0.075;
    const GAP_FROM: f32 = 100.0;
    const GAP_TO: f32 = 150.0;

    let (dx, dy) = (x - CX, y - CY);
    let dist = (dx * dx + dy * dy).sqrt();
    let angle = (-dy).atan2(dx).to_degrees().rem_euclid(360.0);
    let on_ring = (dist - RING).abs() <= THICK / 2.0 && !(GAP_FROM..GAP_TO).contains(&angle);

    // Arrowhead at the GAP_FROM end, pointing counter-clockwise into the gap.
    let a = GAP_FROM.to_radians();
    let (nx, ny) = (a.cos(), -a.sin());
    let (tx, ty) = (-a.sin(), -a.cos());
    let (bx, by) = (CX + RING * nx, CY + RING * ny);
    let half = THICK * 1.35;
    let tip = (bx + tx * THICK * 2.4, by + ty * THICK * 2.4);
    let head = in_triangle((x, y), (bx + nx * half, by + ny * half), (bx - nx * half, by - ny * half), tip);

    let play = in_triangle((x, y), (0.445, 0.395), (0.445, 0.605), (0.615, 0.5));
    on_ring || head || play
}

fn in_triangle(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let cross = |o: (f32, f32), u: (f32, f32), v: (f32, f32)| (u.0 - o.0) * (v.1 - o.1) - (u.1 - o.1) * (v.0 - o.0);
    let (d1, d2, d3) = (cross(p, a, b), cross(p, b, c), cross(p, c, a));
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}

/// Packs several sizes into a `.ico` file using uncompressed 32-bit DIB entries.
#[allow(dead_code)]
pub fn build_ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = sizes.iter().map(|&s| (s, ico_dib(s, &render_icon(s, false)))).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, dib) in &images {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(dib.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += dib.len() as u32;
    }
    for (_, dib) in images {
        out.extend_from_slice(&dib);
    }
    out
}

fn ico_dib(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mask_row = size.div_ceil(32) * 4;
    let mut dib = Vec::new();
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(size as i32).to_le_bytes());
    dib.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&[0u8; 24]);
    for row in (0..size).rev() {
        for col in 0..size {
            let i = ((row * size + col) * 4) as usize;
            dib.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    dib.extend(std::iter::repeat_n(0u8, (mask_row * size) as usize));
    dib
}
