// Rust 1.97 | Windows x64 | KimichatCLI embedded application icon resource.
//
// Renders a small sigil (void tile, silver ring, crimson void-orb, twin
// horns) at 4x supersampling and writes a two-image (32 + 256) 32bpp ICO.
use std::{env, fs::File, io::Write, path::PathBuf};

const SIZES: [u32; 2] = [32, 256];
const SUPER: u32 = 4;

fn main() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let ico_path = out_dir.join("kimichat.ico");
    write_icon(&ico_path).expect("write icon");

    if env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut resource = winres::WindowsResource::new();
        resource.set_icon(ico_path.to_str().expect("icon path"));
        resource.compile().expect("compile Windows resources");
    }
}

fn write_icon(path: &PathBuf) -> std::io::Result<()> {
    let images: Vec<Vec<u8>> = SIZES.iter().map(|&size| render_dib(size)).collect();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0, 0, 1, 0]); // reserved, type = icon
    bytes.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * SIZES.len() as u32;
    for (index, &size) in SIZES.iter().enumerate() {
        bytes.push(if size == 256 { 0 } else { size as u8 });
        bytes.push(if size == 256 { 0 } else { size as u8 });
        bytes.extend_from_slice(&[0, 0]); // palette, reserved
        bytes.extend_from_slice(&1u16.to_le_bytes()); // planes
        bytes.extend_from_slice(&32u16.to_le_bytes()); // bpp
        bytes.extend_from_slice(&(images[index].len() as u32).to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
        offset += images[index].len() as u32;
    }
    for image in images {
        bytes.extend_from_slice(&image);
    }
    File::create(path)?.write_all(&bytes)
}

/// One 32bpp BMP-in-ICO (bottom-up BGRA pixels + empty AND mask).
fn render_dib(size: u32) -> Vec<u8> {
    let header = 40u32;
    let pixels = size * size * 4;
    let mask_row = size.div_ceil(32) * 4;
    let mask = mask_row * size;
    let mut bytes = Vec::with_capacity((header + pixels + mask) as usize);
    bytes.extend_from_slice(&40u32.to_le_bytes());
    bytes.extend_from_slice(&(size as i32).to_le_bytes());
    bytes.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    bytes.extend_from_slice(&(pixels + mask).to_le_bytes());
    bytes.extend_from_slice(&0i32.to_le_bytes());
    bytes.extend_from_slice(&0i32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..size).rev() {
        for x in 0..size {
            let (r, g, b, a) = sample_aa(x, y, size);
            bytes.extend_from_slice(&[b, g, r, a]);
        }
    }
    bytes.resize(bytes.len() + mask as usize, 0);
    bytes
}

/// 4x supersampled pixel of the sigil drawn in a 32x32 design space.
fn sample_aa(x: u32, y: u32, size: u32) -> (u8, u8, u8, u8) {
    let mut r = 0f32;
    let mut g = 0f32;
    let mut b = 0f32;
    let mut a = 0f32;
    for sy in 0..SUPER {
        for sx in 0..SUPER {
            let px = (x as f32 + (sx as f32 + 0.5) / SUPER as f32) * 32.0 / size as f32;
            let py = (y as f32 + (sy as f32 + 0.5) / SUPER as f32) * 32.0 / size as f32;
            let (cr, cg, cb, ca) = sigil(px, py);
            r += cr as f32 * ca;
            g += cg as f32 * ca;
            b += cb as f32 * ca;
            a += ca;
        }
    }
    let n = (SUPER * SUPER) as f32;
    if a <= 0.0 {
        return (0, 0, 0, 0);
    }
    (
        (r / a).round() as u8,
        (g / a).round() as u8,
        (b / a).round() as u8,
        (a / n).round() as u8,
    )
}

fn lerp(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    (
        (a.0 as f32 + (b.0 as f32 - a.0 as f32) * t).round() as u8,
        (a.1 as f32 + (b.1 as f32 - a.1 as f32) * t).round() as u8,
        (a.2 as f32 + (b.2 as f32 - a.2 as f32) * t).round() as u8,
    )
}

fn sigil(x: f32, y: f32) -> (u8, u8, u8, f32) {
    // Rounded void tile.
    let (tx, ty) = (x - 16.0, y - 16.0);
    let half = 14.0_f32;
    let corner = 6.5_f32;
    let ax = tx.abs() - (half - corner);
    let ay = ty.abs() - (half - corner);
    let dist_tile =
        ax.max(ay).min(0.0) + (ax.max(0.0).powi(2) + ay.max(0.0).powi(2)).sqrt() - corner;
    if dist_tile >= 0.0 {
        return (0, 0, 0, 0.0);
    }
    let tile_t = 1.0 - ((tx * tx + ty * ty).sqrt() / 20.0).min(1.0);
    let mut color = lerp((11, 7, 16), (30, 18, 40), tile_t);
    // Twin horns behind the orb.
    for (hx, hy) in [(11.0, 8.5), (21.0, 8.5)] {
        let d = ((x - hx).powi(2) + (y - hy).powi(2)).sqrt();
        if d < 3.4 {
            color = lerp(color, (16, 9, 26), 0.9);
        }
    }
    // Silver ring.
    let (ox, oy) = (x - 16.0, y - 17.0);
    let d = (ox * ox + oy * oy).sqrt();
    if (9.2..11.6).contains(&d) {
        color = lerp(color, (201, 194, 214), 0.95);
    }
    // Crimson void orb.
    if d < 9.0 {
        color = lerp((198, 48, 82), (46, 12, 72), (d / 9.0).powf(1.4));
    }
    (color.0, color.1, color.2, 255.0)
}
