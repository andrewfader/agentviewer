//! Diagnostic tool: parse a character file, decode every image, and optionally
//! write a composited frame out as a PNG.

use std::process::ExitCode;

use acs::{ImageCache, RgbaImage};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: acsdump <file> [--anims] [--png <animation> <frame> <out.png>]");
        return ExitCode::from(2);
    }

    let path = &args[0];
    let character = match acs::load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}: {}", path, e);
            return ExitCode::FAILURE;
        }
    };

    let info = &character.info;
    println!("file          {}", path);
    println!(
        "version       {}.{}",
        info.major_version, info.minor_version
    );
    println!("name          {}", info.name().unwrap_or("(unnamed)"));
    if let Some(d) = info.description() {
        println!("description   {}", d);
    }
    println!("size          {}x{}", info.width, info.height);
    println!(
        "palette       {} colours, transparent index {}",
        info.palette.len(),
        info.transparent_index
    );
    println!("flags         0x{:08X}", info.flags);
    println!("animations    {}", character.animations.len());
    println!("images        {}", character.image_count());
    println!("sounds        {}", character.audio_count());
    println!("states        {}", info.states.len());
    if let Some(v) = &info.voice {
        println!(
            "voice         speed {} pitch {} gender {:?} language {:?}",
            v.speed, v.pitch, v.gender, v.language_id
        );
    }
    if let Some(b) = &info.balloon {
        println!(
            "balloon       {} lines x {} chars, font {:?} {}pt",
            b.lines, b.chars_per_line, b.font_name, b.font_height
        );
    }

    // Decode every image; this is where decompression bugs surface.  A PNG
    // request only decodes the selected frame, which keeps large Actor files
    // pleasantly quick to inspect (their WMF assets are rasterized lazily).
    let mut failures = 0;
    let mut total_pixels: u64 = 0;
    let png_requested = args.iter().any(|a| a == "--png");
    if !png_requested {
        // Actor artwork is vector, so every frame is drawn rather than decoded;
        // that goes through render_frame instead of the image table.
        let mut cache = ImageCache::new();
        for i in 0..character.image_count() {
            let decoded = if character.is_actor() {
                let frame = acs::Frame {
                    images: vec![acs::FrameImage {
                        image_index: i as u32,
                        x: 0,
                        y: 0,
                    }],
                    audio_index: None,
                    duration: 0,
                    exit_frame: -1,
                    branches: Vec::new(),
                    overlays: Vec::new(),
                };
                character
                    .render_frame(&frame, None, &mut cache)
                    .map(|img| img.width as u64 * img.height as u64)
            } else {
                character
                    .image(i)
                    .map(|img| img.width as u64 * img.height as u64)
            };
            match decoded {
                Ok(pixels) => total_pixels += pixels,
                Err(e) => {
                    if failures < 5 {
                        eprintln!("  image {} failed: {}", i, e);
                    }
                    failures += 1;
                }
            }
        }
    }
    if !png_requested {
        println!(
            "decoded       {}/{} images ({} pixels)",
            character.image_count() - failures,
            character.image_count(),
            total_pixels
        );
    }

    let overlay_anims = character
        .animations
        .iter()
        .filter(|a| a.frames.iter().any(|f| !f.overlays.is_empty()))
        .count();
    let overlay_frames: usize = character
        .animations
        .iter()
        .map(|a| a.frames.iter().filter(|f| !f.overlays.is_empty()).count())
        .sum();
    println!(
        "mouth overlays {} animations, {} frames",
        overlay_anims, overlay_frames
    );

    if args.iter().any(|a| a == "--anims") {
        for anim in &character.animations {
            let sounds = anim.frames.iter().filter(|f| f.audio_index.is_some()).count();
            let branches: usize = anim.frames.iter().map(|f| f.branches.len()).sum();
            println!(
                "  {:<20} {:>3} frames {:>6}ms  {} sounds, {} branches",
                anim.name,
                anim.frames.len(),
                anim.duration_ms(),
                sounds,
                branches
            );
        }
    }

    let empty_anims = character
        .animations
        .iter()
        .filter(|a| a.frames.is_empty())
        .count();
    if empty_anims > 0 {
        println!(
            "warning       {} animations parsed with no frames",
            empty_anims
        );
    }

    if let Some(pos) = args.iter().position(|a| a == "--png") {
        let name = args.get(pos + 1).map(String::as_str).unwrap_or("");
        let frame_no: usize = args.get(pos + 2).and_then(|s| s.parse().ok()).unwrap_or(0);
        let out = args.get(pos + 3).map(String::as_str).unwrap_or("frame.png");

        let anim = character
            .animation_by_name(name)
            .or_else(|| character.animations.iter().find(|a| !a.frames.is_empty()));
        let Some(anim) = anim else {
            eprintln!("no animation with frames found");
            return ExitCode::FAILURE;
        };
        let Some(frame) = anim.frames.get(frame_no) else {
            eprintln!("animation {:?} has {} frames", anim.name, anim.frames.len());
            return ExitCode::FAILURE;
        };
        // Optional mouth shape, so lip-sync substitution can be inspected.
        let mouth = args
            .iter()
            .position(|a| a == "--mouth")
            .and_then(|p| args.get(p + 1))
            .and_then(|s| s.parse::<u8>().ok())
            .and_then(acs::MouthShape::from_u8);

        let mut cache = ImageCache::new();
        match character.render_frame(frame, mouth, &mut cache) {
            Ok(img) => match write_png(out, &img) {
                Ok(()) => println!("wrote         {} ({} frame {})", out, anim.name, frame_no),
                Err(e) => {
                    eprintln!("writing {}: {}", out, e);
                    return ExitCode::FAILURE;
                }
            },
            Err(e) => {
                eprintln!("rendering frame: {}", e);
                return ExitCode::FAILURE;
            }
        }
    }

    if failures > 0 {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

// --- Minimal PNG writer -----------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, e) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *e = c;
    }
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    let mut body = Vec::with_capacity(4 + payload.len());
    body.extend_from_slice(kind);
    body.extend_from_slice(payload);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

/// Writes an RGBA PNG using stored (uncompressed) deflate blocks, so no
/// compression library is needed.
fn write_png(path: &str, img: &RgbaImage) -> std::io::Result<()> {
    let mut raw = Vec::with_capacity(img.data.len() + img.height as usize);
    for y in 0..img.height as usize {
        raw.push(0); // filter type: none
        raw.extend_from_slice(&img.data[y * img.stride()..(y + 1) * img.stride()]);
    }

    let mut z = vec![0x78, 0x01]; // zlib header, no preset dictionary
    for (i, block) in raw.chunks(65535).enumerate() {
        let last = ((i + 1) * 65535 >= raw.len()) as u8;
        z.push(last);
        z.extend_from_slice(&(block.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        z.extend_from_slice(block);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&img.width.to_be_bytes());
    ihdr.extend_from_slice(&img.height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);

    std::fs::write(path, png)
}
