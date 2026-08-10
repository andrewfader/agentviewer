//! Microsoft Agent 1.5 structured-storage (`.acs`) reader.

use crate::reader::Cursor;
use crate::types::*;
use crate::Error;
use std::io::{Cursor as IoCursor, Read};

const ACF_SIGNATURE: u32 = 0xABCD_ABC1;
const INNER_SIGNATURE: u32 = 0x0001_001F;

struct AnimationRef {
    name: String,
    file: String,
    checksum: u32,
}

fn unpack_acf(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let mut c = Cursor::new(bytes);
    if c.u32()? != ACF_SIGNATURE {
        return Err(Error::Parse("invalid Agent 1.5 ACF signature".into()));
    }
    let unpacked = c.u32()? as usize;
    let packed = c.u32()? as usize;
    crate::decompress::decompress(&c.bytes(packed)?, unpacked)
}

fn unpack_aaf(bytes: &[u8], checksum: u32) -> Result<Vec<u8>, Error> {
    let mut c = Cursor::new(bytes);
    if c.u32()? != INNER_SIGNATURE {
        return Err(Error::Parse("invalid Agent 1.5 AAF signature".into()));
    }
    let stored_checksum = c.u32()?;
    let _checksum_matches = checksum == 0 || stored_checksum == checksum;
    let compressed = c.u8()? != 0;
    let unpacked = c.u32()? as usize;
    let stored = c.u32()? as usize;
    let payload = c.bytes(stored)?;
    if compressed {
        crate::decompress::decompress(&payload, unpacked)
    } else if payload.len() == unpacked {
        Ok(payload)
    } else {
        Err(Error::Parse("Agent 1.5 AAF size mismatch".into()))
    }
}

fn parse_acf(bytes: &[u8]) -> Result<(Vec<AnimationRef>, CharacterInfo), Error> {
    let data = unpack_acf(bytes)?;
    let mut c = Cursor::new(&data);
    if c.u32()? != INNER_SIGNATURE {
        return Err(Error::Parse("invalid decompressed Agent 1.5 ACF".into()));
    }
    let count = c.u16()? as usize;
    let mut refs = Vec::with_capacity(count);
    for _ in 0..count {
        let name = c.string_legacy()?;
        let file = c.string_legacy()?;
        let _return_animation = c.string_legacy()?;
        refs.push(AnimationRef {
            name,
            file,
            checksum: c.u32()?,
        });
    }
    let mut guid = [0; 16];
    guid.copy_from_slice(&c.bytes(16)?);
    let name = c.string_legacy()?;
    let description = c.string_legacy()?;
    let extra = c.string_legacy()?;
    let width = c.u16()?;
    let height = c.u16()?;
    let transparent_index = c.u8()?;
    let flags = c.u32()?;

    if data.len() < 1028 {
        return Err(Error::Parse("Agent 1.5 ACF has no complete palette".into()));
    }
    // Four legacy flag bytes follow the palette.
    let palette_pos = data.len() - 1028;
    let palette = data[palette_pos..palette_pos + 1024]
        .chunks_exact(4)
        .map(|q| Rgb {
            r: q[2],
            g: q[1],
            b: q[0],
        })
        .collect();
    let localized = vec![LocalizedInfo {
        language_id: 0x0409,
        name,
        description,
        extra,
    }];
    Ok((
        refs,
        CharacterInfo {
            major_version: 1,
            minor_version: 5,
            guid,
            width,
            height,
            transparent_index,
            flags,
            voice: None,
            balloon: None,
            palette,
            states: Vec::new(),
            localized,
        },
    ))
}

fn parse_aaf(
    bytes: &[u8],
    r: &AnimationRef,
    info: &CharacterInfo,
    images: &mut Vec<IndexedImage>,
    sounds: &mut Vec<Vec<u8>>,
) -> Result<Animation, Error> {
    let data = unpack_aaf(bytes, r.checksum)?;
    let mut c = Cursor::new(&data);
    let has_audio = c.u8()? != 0;
    c.u8()?;
    let audio_index = if has_audio {
        let size = c.u32()? as usize;
        let wave = c.bytes(size)?;
        if !wave.starts_with(b"RIFF") || wave.get(8..12) != Some(b"WAVE") {
            return Err(Error::Parse(format!(
                "{} has an invalid WAVE block",
                r.file
            )));
        }
        sounds.push(wave);
        Some((sounds.len() - 1) as u16)
    } else {
        None
    };
    let frame_count = c.u16()? as usize;
    let frame_size = c.u32()? as usize;
    let _transparent = c.u8()?;
    let stride = stride_for(info.width);
    let expected = stride * info.height as usize;
    if frame_size != expected {
        return Err(Error::Parse(format!(
            "{} frame size {} != {}",
            r.file, frame_size, expected
        )));
    }
    let mut frames = Vec::with_capacity(frame_count);
    for n in 0..frame_count {
        let top_down = c.bytes(frame_size)?;
        let mut pixels = vec![0; frame_size];
        for y in 0..info.height as usize {
            let src = y * stride;
            let dst = (info.height as usize - 1 - y) * stride;
            pixels[dst..dst + stride].copy_from_slice(&top_down[src..src + stride]);
        }
        images.push(IndexedImage {
            width: info.width,
            height: info.height,
            pixels,
        });
        frames.push(Frame {
            images: vec![FrameImage {
                image_index: (images.len() - 1) as u32,
                x: 0,
                y: 0,
            }],
            audio_index: if n == 0 { audio_index } else { None },
            duration: 10,
            exit_frame: -1,
            branches: Vec::new(),
            overlays: Vec::new(),
        });
    }
    Ok(Animation {
        name: r.name.clone(),
        transition: Transition::None,
        return_animation: String::new(),
        frames,
    })
}

pub(crate) fn parse(data: Vec<u8>) -> Result<Character, Error> {
    let mut compound = cfb::CompoundFile::open(IoCursor::new(data))
        .map_err(|e| Error::Parse(format!("invalid Agent 1.5 compound file: {}", e)))?;
    let mut acf = Vec::new();
    compound
        .open_stream("/char.acf")
        .map_err(|_| Error::Parse("Agent 1.5 file has no char.acf stream".into()))?
        .read_to_end(&mut acf)?;
    let (refs, info) = parse_acf(&acf)?;
    let mut images = Vec::new();
    let mut sounds = Vec::new();
    let mut animations = Vec::with_capacity(refs.len());
    for r in refs {
        let mut aaf = Vec::new();
        let parsed = compound
            .open_stream(format!("/{}", r.file))
            .and_then(|mut s| s.read_to_end(&mut aaf));
        if parsed.is_ok() {
            if let Ok(a) = parse_aaf(&aaf, &r, &info, &mut images, &mut sounds) {
                animations.push(a);
                continue;
            }
        }
        animations.push(Animation {
            name: r.name,
            transition: Transition::None,
            return_animation: String::new(),
            frames: Vec::new(),
        });
    }
    Ok(Character::from_legacy(info, animations, images, sounds))
}
