//! Embeds the logo as the exe icon, plus version info for Explorer's
//! Properties > Details tab. The image is rendered from the same code as the
//! tray icon, so there is no separate asset to keep in sync.
//!
//! The resources are written directly as a COFF object that the linker merges
//! into the exe, so no resource compiler (rc.exe, windres) or crate is needed.
//! Formats follow the PE/COFF spec ("The .rsrc Section") and the Win32 docs
//! for GRPICONDIR (NEWHEADER/RESDIR) and VS_VERSIONINFO.

use std::env::var;
use std::path::PathBuf;

#[path = "src/logo.rs"]
mod logo;

/// Icon sizes to embed. Small sizes are rendered natively rather than left to
/// Windows to downscale from 256, which keeps the thin marks crisp.
const ICON_SIZES: [usize; 5] = [16, 24, 32, 48, 256];

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
const LANG_EN_US: u16 = 0x0409;
const CODEPAGE_UTF16: u16 = 1200;

struct Resource {
    kind: u16,
    id: u16,
    data: Vec<u8>,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/logo.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    if var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let (machine, reloc_type) = match env("CARGO_CFG_TARGET_ARCH").as_str() {
        "x86_64" => (0x8664, 0x0003),  // IMAGE_REL_AMD64_ADDR32NB
        "x86" => (0x014C, 0x0007),     // IMAGE_REL_I386_DIR32NB
        "aarch64" => (0xAA64, 0x0002), // IMAGE_REL_ARM64_ADDR32NB
        arch => panic!("no resource support for target arch {arch}"),
    };

    let mut resources = icon_resources();
    resources.push(Resource {
        kind: RT_VERSION,
        id: 1,
        data: version_info(),
    });
    resources.sort_by_key(|r| (r.kind, r.id));

    let path = PathBuf::from(env("OUT_DIR")).join("resources.obj");
    std::fs::write(&path, resource_object(&resources, machine, reloc_type))
        .expect("failed to write resource object");
    println!("cargo:rustc-link-arg-bins={}", path.display());
}

fn env(key: &str) -> String {
    var(key).unwrap_or_else(|_| panic!("{key} is not set"))
}

/// One RT_ICON per size, each a PNG, and an RT_GROUP_ICON listing them.
/// Explorer shows the group with the lowest ID.
fn icon_resources() -> Vec<Resource> {
    // NEWHEADER: reserved, type (1 = icon), image count.
    let mut group = Vec::new();
    for v in [0, 1, ICON_SIZES.len() as u16] {
        put16(&mut group, v);
    }

    let mut resources = Vec::new();
    for (i, &size) in ICON_SIZES.iter().enumerate() {
        let id = i as u16 + 1;
        let mut pixels = vec![0u32; size * size];
        logo::draw(&mut pixels, size);
        let png = encode_png(&pixels, size);

        // RESDIR: width, height (0 means 256), color count, reserved,
        // planes, bit count, byte size, RT_ICON id.
        let dim = if size >= 256 { 0 } else { size as u8 };
        group.extend_from_slice(&[dim, dim, 0, 0]);
        put16(&mut group, 1);
        put16(&mut group, 32);
        put32(&mut group, png.len() as u32);
        put16(&mut group, id);

        resources.push(Resource {
            kind: RT_ICON,
            id,
            data: png,
        });
    }
    resources.push(Resource {
        kind: RT_GROUP_ICON,
        id: 1,
        data: group,
    });
    resources
}

/// A VS_VERSIONINFO block filled from the Cargo package metadata.
fn version_info() -> Vec<u8> {
    let part = |key: &str| -> u32 {
        env(key)
            .parse::<u16>()
            .unwrap_or_else(|_| panic!("{key} does not fit in 16 bits"))
            .into()
    };
    let (major, minor, patch) = (
        part("CARGO_PKG_VERSION_MAJOR"),
        part("CARGO_PKG_VERSION_MINOR"),
        part("CARGO_PKG_VERSION_PATCH"),
    );
    let (ms, ls) = ((major << 16) | minor, patch << 16);
    let prerelease = !env("CARGO_PKG_VERSION_PRE").is_empty();

    let mut fixed = Vec::new();
    for v in [
        0xFEEF_04BD,           // dwSignature
        0x0001_0000,           // dwStrucVersion
        ms,                    // dwFileVersionMS
        ls,                    // dwFileVersionLS
        ms,                    // dwProductVersionMS
        ls,                    // dwProductVersionLS
        0x3F,                  // dwFileFlagsMask: VS_FFI_FILEFLAGSMASK
        prerelease as u32 * 2, // dwFileFlags: VS_FF_PRERELEASE
        0x0004_0004,           // dwFileOS: VOS_NT_WINDOWS32
        1,                     // dwFileType: VFT_APP
        0,                     // dwFileSubtype
        0,                     // dwFileDateMS
        0,                     // dwFileDateLS
    ] {
        put32(&mut fixed, v);
    }

    let name = env("CARGO_PKG_NAME");
    let version = env("CARGO_PKG_VERSION");
    let description = var("CARGO_PKG_DESCRIPTION")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| name.clone());
    let exe = format!("{name}.exe");
    let strings = [
        ("FileDescription", description.as_str()),
        ("FileVersion", version.as_str()),
        ("InternalName", name.as_str()),
        ("OriginalFilename", exe.as_str()),
        ("ProductName", name.as_str()),
        ("ProductVersion", version.as_str()),
    ];

    // Var "Translation": language in the low word, code page in the high word.
    let mut translation = Vec::new();
    put16(&mut translation, LANG_EN_US);
    put16(&mut translation, CODEPAGE_UTF16);
    let table_key = format!("{LANG_EN_US:04x}{CODEPAGE_UTF16:04x}");

    let mut out = Vec::new();
    version_node(&mut out, "VS_VERSION_INFO", Value::Binary(&fixed), |out| {
        version_node(out, "StringFileInfo", Value::None, |out| {
            version_node(out, &table_key, Value::None, |out| {
                for (key, value) in strings {
                    version_node(out, key, Value::Text(value), |_| {});
                }
            });
        });
        version_node(out, "VarFileInfo", Value::None, |out| {
            version_node(out, "Translation", Value::Binary(&translation), |_| {});
        });
    });
    out
}

enum Value<'a> {
    None,
    Text(&'a str),
    Binary(&'a [u8]),
}

/// Writes one version block: wLength, wValueLength, wType, the key, padding,
/// the value, padding, then the children. Every block starts on a 32-bit
/// boundary, and wLength excludes any padding after the block.
fn version_node(out: &mut Vec<u8>, key: &str, value: Value, children: impl FnOnce(&mut Vec<u8>)) {
    align(out, 4);
    let start = out.len();
    // wValueLength is in characters for text and in bytes for binary data.
    let (value_len, text, bytes) = match value {
        Value::None => (0, true, Vec::new()),
        Value::Text(s) => {
            let b = utf16z(s);
            (b.len() / 2, true, b)
        }
        Value::Binary(b) => (b.len(), false, b.to_vec()),
    };
    put16(out, 0); // wLength, patched below
    put16(out, value_len as u16);
    put16(out, text as u16);
    out.extend(utf16z(key));
    align(out, 4);
    out.extend(bytes);
    children(out);

    let len = u16::try_from(out.len() - start).expect("version block too large");
    out[start..start + 2].copy_from_slice(&len.to_le_bytes());
}

/// Builds a COFF object with a single `.rsrc` section: the resource directory
/// tree (type, then ID, then language), the data entries, then the raw data.
/// Each data entry's RVA holds its offset in the section, with a relocation so
/// the linker adds the section's RVA once it is placed in the image.
fn resource_object(resources: &[Resource], machine: u16, reloc_type: u16) -> Vec<u8> {
    const SUBDIR: u32 = 0x8000_0000;
    let table_size = |entries: usize| 16 + 8 * entries;

    let mut kinds: Vec<u16> = resources.iter().map(|r| r.kind).collect();
    kinds.dedup();
    let of_kind = |kind: u16| {
        resources
            .iter()
            .enumerate()
            .filter(move |(_, r)| r.kind == kind)
    };

    // Lay out the section in the order it is written below.
    let mut offset = table_size(kinds.len());
    let mut next = |size: usize, align: usize| {
        offset = offset.next_multiple_of(align);
        offset += size;
        (offset - size) as u32
    };
    let kind_tables: Vec<u32> = kinds
        .iter()
        .map(|&k| next(table_size(of_kind(k).count()), 1))
        .collect();
    let lang_tables: Vec<u32> = resources.iter().map(|_| next(table_size(1), 1)).collect();
    let entries: Vec<u32> = resources.iter().map(|_| next(16, 1)).collect();
    let data: Vec<u32> = resources.iter().map(|r| next(r.data.len(), 8)).collect();

    let mut sec = Vec::new();
    let root: Vec<_> = kinds
        .iter()
        .zip(&kind_tables)
        .map(|(&k, &at)| (k.into(), at | SUBDIR))
        .collect();
    dir_table(&mut sec, &root);
    for &kind in &kinds {
        let ids: Vec<_> = of_kind(kind)
            .map(|(i, r)| (r.id.into(), lang_tables[i] | SUBDIR))
            .collect();
        dir_table(&mut sec, &ids);
    }
    for &entry in &entries {
        dir_table(&mut sec, &[(LANG_EN_US.into(), entry)]);
    }
    let mut relocs = Vec::new();
    for (r, &at) in resources.iter().zip(&data) {
        relocs.push(sec.len() as u32);
        put32(&mut sec, at); // Data RVA, relocated
        put32(&mut sec, r.data.len() as u32);
        put32(&mut sec, 0); // code page
        put32(&mut sec, 0); // reserved
    }
    for (r, &at) in resources.iter().zip(&data) {
        align(&mut sec, 8);
        assert_eq!(sec.len(), at as usize);
        sec.extend_from_slice(&r.data);
    }

    const NAME: &[u8; 8] = b".rsrc\0\0\0";
    let raw_ptr = 20 + 40; // file header + one section header
    let reloc_ptr = (raw_ptr + sec.len()).next_multiple_of(4);
    let symbol_ptr = reloc_ptr + 10 * relocs.len();
    let nrelocs = u16::try_from(relocs.len()).expect("too many resources");
    // On x86 the object must be marked SEH-safe or /SAFESEH links fail. It has
    // no code, so it trivially is.
    let safe_seh = machine == 0x014C;

    let mut obj = Vec::new();
    // File header.
    put16(&mut obj, machine);
    put16(&mut obj, 1); // NumberOfSections
    put32(&mut obj, 0); // TimeDateStamp, zero for reproducible builds
    put32(&mut obj, symbol_ptr as u32);
    // NumberOfSymbols: section symbol + its aux record, then @feat.00.
    put32(&mut obj, 2 + safe_seh as u32);
    put16(&mut obj, 0); // SizeOfOptionalHeader
    put16(&mut obj, 0); // Characteristics

    // Section header.
    obj.extend_from_slice(NAME);
    put32(&mut obj, 0); // VirtualSize
    put32(&mut obj, 0); // VirtualAddress
    put32(&mut obj, sec.len() as u32);
    put32(&mut obj, raw_ptr as u32);
    put32(&mut obj, reloc_ptr as u32);
    put32(&mut obj, 0); // PointerToLinenumbers
    put16(&mut obj, nrelocs);
    put16(&mut obj, 0); // NumberOfLinenumbers
    put32(&mut obj, 0x4000_0040); // IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ

    obj.extend_from_slice(&sec);
    align(&mut obj, 4);

    // Relocations, all against symbol 0 (the section itself).
    for at in relocs {
        put32(&mut obj, at);
        put32(&mut obj, 0);
        put16(&mut obj, reloc_type);
    }

    // Section symbol: Value, SectionNumber, Type, StorageClass (STATIC),
    // NumberOfAuxSymbols.
    obj.extend_from_slice(NAME);
    put32(&mut obj, 0);
    put16(&mut obj, 1);
    put16(&mut obj, 0);
    obj.extend_from_slice(&[3, 1]);
    // Aux section definition: Length, NumberOfRelocations,
    // NumberOfLinenumbers, CheckSum, Number, Selection, 3 unused bytes.
    put32(&mut obj, sec.len() as u32);
    put16(&mut obj, nrelocs);
    put16(&mut obj, 0);
    put32(&mut obj, 0);
    put16(&mut obj, 0);
    obj.extend_from_slice(&[0; 4]);

    if safe_seh {
        // Absolute (section -1) static symbol with bit 0 set: registered SEH.
        obj.extend_from_slice(b"@feat.00");
        put32(&mut obj, 1);
        put16(&mut obj, 0xFFFF);
        put16(&mut obj, 0);
        obj.extend_from_slice(&[3, 0]);
    }

    // String table: just its own size field.
    put32(&mut obj, 4);
    obj
}

/// A resource directory table with ID entries only: (ID, target offset).
fn dir_table(out: &mut Vec<u8>, entries: &[(u32, u32)]) {
    put32(out, 0); // Characteristics
    put32(out, 0); // TimeDateStamp
    put16(out, 0); // MajorVersion
    put16(out, 0); // MinorVersion
    put16(out, 0); // NumberOfNameEntries
    put16(out, entries.len() as u16);
    for &(id, target) in entries {
        put32(out, id);
        put32(out, target);
    }
}

fn put16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn align(out: &mut Vec<u8>, to: usize) {
    out.resize(out.len().next_multiple_of(to), 0);
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
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
