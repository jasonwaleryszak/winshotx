//! Embeds the logo as the exe icon. The image is rendered from the same code
//! as the tray icon, so there is no separate asset to keep in sync.

#[path = "src/logo.rs"]
mod logo;

const SIZE: usize = 256;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/logo.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut pixels = vec![0u32; SIZE * SIZE];
    logo::draw(&mut pixels, SIZE);

    // Explorer shows the icon with the lowest ID.
    embedinator::ResourceBuilder::from_env()
        .add_icon(
            1,
            embedinator::Icon::from_png_bytes(encode_png(&pixels, SIZE)),
        )
        .finish();
}

/// A minimal RGBA PNG using uncompressed deflate blocks, which avoids
/// pulling in an image or compression crate.
fn encode_png(pixels: &[u32], size: usize) -> Vec<u8> {
    let mut raw = Vec::with_capacity(size * (size * 4 + 1));
    for row in pixels.chunks(size) {
        raw.push(0); // filter: none
        for &p in row {
            let [b, g, r, a] = p.to_le_bytes();
            raw.extend_from_slice(&[r, g, b, a]);
        }
    }

    let mut zlib = vec![0x78, 0x01];
    let mut blocks = raw.chunks(u16::MAX as usize).peekable();
    while let Some(block) = blocks.next() {
        let len = block.len() as u16;
        zlib.push(blocks.peek().is_none() as u8);
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(size as u32).to_be_bytes());
    ihdr.extend_from_slice(&(size as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, no interlace

    let mut png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];
    for (kind, data) in [(b"IHDR", &ihdr), (b"IDAT", &zlib), (b"IEND", &Vec::new())] {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = png.len();
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let crc = crc32(&png[start..]);
        png.extend_from_slice(&crc.to_be_bytes());
    }
    png
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    b << 16 | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                crc >> 1 ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
