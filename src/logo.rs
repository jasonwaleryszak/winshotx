//! The WinShotX logo: white selection-corner marks on a blue square.
//!
//! Shared by the tray icon and `build.rs`, which embeds it as the exe icon.

/// Draws the logo into `pixels`, a `size` x `size` buffer of 0xAARRGGBB.
pub fn draw(pixels: &mut [u32], size: usize) {
    const SS: usize = 4; // supersampling per axis, for anti-aliasing
    const BLUE: [f32; 3] = [0x2D as f32, 0x7D as f32, 0xF6 as f32];

    let in_marks = |u: f32, v: f32| {
        let (lo, hi, thick, arm) = (0.24, 0.76, 0.09, 0.2);
        if !(lo..=hi).contains(&u) || !(lo..=hi).contains(&v) {
            return false;
        }
        let on_edge = |p: f32| p <= lo + thick || p >= hi - thick;
        let near_corner = |p: f32| p <= lo + arm || p >= hi - arm;
        (on_edge(u) && near_corner(v)) || (on_edge(v) && near_corner(u))
    };

    let s = size as f32;
    for y in 0..size {
        for x in 0..size {
            let mut mark = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (x as f32 + (sx as f32 + 0.5) / SS as f32) / s;
                    let v = (y as f32 + (sy as f32 + 0.5) / SS as f32) / s;
                    mark += in_marks(u, v) as u32;
                }
            }
            let white = mark as f32 / (SS * SS) as f32;
            let [r, g, b] = BLUE.map(|c| (c + (255.0 - c) * white).round() as u32);
            pixels[y * size + x] = 0xFF << 24 | r << 16 | g << 8 | b;
        }
    }
}
