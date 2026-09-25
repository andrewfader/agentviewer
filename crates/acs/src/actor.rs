//! Microsoft Actor 1.0/2.0 (`.act`) reader.
//!
//! Actor is the format behind the Microsoft Bob cast and the Office 95-era
//! assistants. Unlike Agent's `.acs`, the artwork is vector: every frame is a
//! stack of placeable Windows Metafiles positioned on a 2880x2160 logical
//! canvas, which [`crate::wmf`] rasterizes.
//!
//! File layout, all offsets relative to the end of the name string:
//!
//! | pointer | contents |
//! |---|---|
//! | 0 | metafile and frame-composition blobs |
//! | 1 | offset table into pointer 0 |
//! | 2 | audio blobs |
//! | 3 | offset table into pointer 2 |
//! | 4 | animation sequences |
//! | 5 | action table |
//! | 6 | character bio strings |
//! | 7 | secondary sequence table (unused here) |
//!
//! Entries below `first_frame` in the offset table are metafiles; the rest are
//! frame compositions.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use crate::types::*;
use crate::wmf;
use crate::Error;

/// Actor's logical drawing canvas, which frame rectangles are expressed in.
const LOGICAL_WIDTH: i32 = 2880;
const LOGICAL_HEIGHT: i32 = 2160;

/// Stands in for "no artwork" in a layer and "no image" in a sequence slot.
const BLANK: u16 = 0xFFFF;

/// Characters declare a 192x144 canvas, which is small for artwork that is
/// vector to begin with. Rasterizing a few times larger costs little and keeps
/// the character crisp when the window scales it up.
const OVERSAMPLE: u16 = 3;

/// One metafile placed on a frame, in output pixels.
#[derive(Clone)]
struct Layer {
    asset: u16,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

pub(crate) struct ActorSource {
    data: Vec<u8>,
    assets: Vec<Range<usize>>,
    /// Size each metafile is rasterized at: the largest it is ever drawn, so
    /// one raster serves every frame that uses it.
    asset_size: Vec<(usize, usize)>,
    frames: Vec<Vec<Layer>>,
    width: u16,
    height: u16,
    cache: RefCell<HashMap<u16, Rc<wmf::Bitmap>>>,
}

impl ActorSource {
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    fn asset(&self, index: u16) -> Result<Rc<wmf::Bitmap>, Error> {
        if let Some(bitmap) = self.cache.borrow().get(&index) {
            return Ok(bitmap.clone());
        }
        let range = self
            .assets
            .get(index as usize)
            .ok_or_else(|| Error::Parse(format!("Actor asset {} is out of range", index)))?;
        let (w, h) = self.asset_size[index as usize];
        let bitmap = Rc::new(wmf::render(&self.data[range.clone()], w, h)?);
        self.cache.borrow_mut().insert(index, bitmap.clone());
        Ok(bitmap)
    }

    /// Composites one frame into a straight-alpha RGBA buffer.
    pub fn render(&self, index: usize) -> Result<Vec<u8>, Error> {
        let layers = self
            .frames
            .get(index)
            .ok_or_else(|| Error::Parse(format!("Actor frame {} is out of range", index)))?;
        let (w, h) = (self.width as usize, self.height as usize);
        let mut rgba = vec![0u8; w * h * 4];

        for layer in layers {
            // 0xFFFF is the same "nothing here" sentinel the sequence records
            // use for a blank frame.
            if layer.asset == BLANK {
                continue;
            }
            let asset = self.asset(layer.asset)?;
            if asset.width == 0 || asset.height == 0 {
                continue;
            }
            let (dw, dh) = (layer.x1 - layer.x0, layer.y1 - layer.y0);
            if dw <= 0.0 || dh <= 0.0 {
                continue;
            }
            // Clip to the canvas rather than clamping the rectangle, so layers
            // that hang off an edge keep their scale.
            let px0 = layer.x0.floor().max(0.0) as usize;
            let py0 = layer.y0.floor().max(0.0) as usize;
            let px1 = (layer.x1.ceil().max(0.0) as usize).min(w);
            let py1 = (layer.y1.ceil().max(0.0) as usize).min(h);

            for py in py0..py1 {
                let v = (py as f32 + 0.5 - layer.y0) / dh;
                let sy = (v * asset.height as f32 - 0.5).clamp(0.0, asset.height as f32 - 1.0);
                for px in px0..px1 {
                    let u = (px as f32 + 0.5 - layer.x0) / dw;
                    let sx = (u * asset.width as f32 - 0.5).clamp(0.0, asset.width as f32 - 1.0);
                    let src = sample(&asset, sx, sy);
                    let a = src[3] as u32;
                    if a == 0 {
                        continue;
                    }
                    let d = (py * w + px) * 4;
                    let da = rgba[d + 3] as u32;
                    let out_a = a + da * (255 - a) / 255;
                    if out_a == 0 {
                        continue;
                    }
                    for c in 0..3 {
                        let s = src[c] as u32 * a;
                        let dst = rgba[d + c] as u32 * da * (255 - a) / 255;
                        rgba[d + c] = ((s + dst) / out_a) as u8;
                    }
                    rgba[d + 3] = out_a as u8;
                }
            }
        }
        Ok(rgba)
    }
}

/// Bilinear sample of a straight-alpha bitmap. Colour is weighted by alpha so
/// transparent pixels do not bleed their (undefined) colour into the edges.
fn sample(bitmap: &wmf::Bitmap, x: f32, y: f32) -> [u8; 4] {
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(bitmap.width - 1);
    let y1 = (y0 + 1).min(bitmap.height - 1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;

    let mut acc = [0.0f32; 4];
    for (px, py, weight) in [
        (x0, y0, (1.0 - fx) * (1.0 - fy)),
        (x1, y0, fx * (1.0 - fy)),
        (x0, y1, (1.0 - fx) * fy),
        (x1, y1, fx * fy),
    ] {
        let o = (py * bitmap.width + px) * 4;
        let a = bitmap.rgba[o + 3] as f32 / 255.0;
        for (c, slot) in acc.iter_mut().take(3).enumerate() {
            *slot += bitmap.rgba[o + c] as f32 * a * weight;
        }
        acc[3] += a * weight;
    }
    if acc[3] <= 0.0 {
        return [0; 4];
    }
    [
        (acc[0] / acc[3]).clamp(0.0, 255.0) as u8,
        (acc[1] / acc[3]).clamp(0.0, 255.0) as u8,
        (acc[2] / acc[3]).clamp(0.0, 255.0) as u8,
        (acc[3] * 255.0).clamp(0.0, 255.0) as u8,
    ]
}

fn u16_at(data: &[u8], at: usize) -> Result<u16, Error> {
    data.get(at..at + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| Error::Parse("truncated Actor data".into()))
}

fn u32_at(data: &[u8], at: usize) -> Result<u32, Error> {
    data.get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| Error::Parse("truncated Actor data".into()))
}

fn parse_offsets(data: &[u8], at: usize, end: usize) -> Result<Vec<u32>, Error> {
    if end < at || !(end - at).is_multiple_of(4) {
        return Err(Error::Parse("invalid Actor offset table".into()));
    }
    (at..end).step_by(4).map(|p| u32_at(data, p)).collect()
}

/// Frame composition: a count of metafile placements, each with its rectangle
/// on the logical canvas.
fn parse_layers(data: &[u8], range: Range<usize>, sx: f32, sy: f32) -> Result<Vec<Layer>, Error> {
    if range.len() < 4 {
        return Err(Error::Parse("short Actor frame composition".into()));
    }
    let count = u16_at(data, range.start + 2)? as usize;
    if range.len() < 4 + count * 10 {
        return Err(Error::Parse("truncated Actor frame composition".into()));
    }
    let mut layers = Vec::with_capacity(count);
    let mut p = range.start + 4;
    for _ in 0..count {
        let asset = u16_at(data, p)?;
        let left = u16_at(data, p + 2)? as i16 as f32;
        let top = u16_at(data, p + 4)? as i16 as f32;
        let right = u16_at(data, p + 6)? as i16 as f32;
        let bottom = u16_at(data, p + 8)? as i16 as f32;
        layers.push(Layer {
            asset,
            x0: left * sx,
            y0: top * sy,
            x1: right * sx,
            y1: bottom * sy,
        });
        p += 10;
    }
    Ok(layers)
}

/// One entry of an animation sequence, before it becomes a [`Frame`].
struct Slot {
    id: u16,
    value: u16,
    flags: u16,
}

const FLAG_BRANCH: u16 = 0x0001;
const FLAG_SOUND: u16 = 0x0002;
const FLAG_LAST: u16 = 0x0100;

/// Splits the sequence region into sequences.
///
/// The region opens with a two-byte prefix, then each sequence is a `u16` slot
/// count and a `u16` flags word followed by that many six-byte slots. A slot is
/// `id, value, flags`, and the flags say what follows it: bit 0 chains a branch
/// record, bit 1 appends a sound record after any branch chain, and bit 8 marks
/// the last frame of the sequence.
fn parse_sequences(data: &[u8], region: Range<usize>, count: usize) -> Vec<Vec<Slot>> {
    let mut sequences = Vec::with_capacity(count);
    let mut p = region.start + 2;
    while sequences.len() < count {
        let (Ok(slots), Ok(_flags)) = (u16_at(data, p), u16_at(data, p + 2)) else {
            break;
        };
        p += 4;
        let slots = slots as usize;
        if p + slots * 6 > region.end {
            break;
        }
        let mut sequence = Vec::with_capacity(slots);
        for i in 0..slots {
            let at = p + i * 6;
            sequence.push(Slot {
                id: u16_at(data, at).unwrap_or(0),
                value: u16_at(data, at + 2).unwrap_or(0),
                flags: u16_at(data, at + 4).unwrap_or(0),
            });
        }
        sequences.push(sequence);
        p += slots * 6;
    }
    sequences
}

/// Turns a sequence's slots into frames, resolving sounds and branch targets.
fn build_frames(slots: &[Slot], first_frame: u16, frame_total: usize) -> Vec<Frame> {
    // Branch targets address raw slots, so remember where each slot landed.
    let mut slot_to_frame = vec![usize::MAX; slots.len()];
    let mut frames: Vec<Frame> = Vec::new();
    let mut pending: Vec<(usize, Vec<(usize, u16)>)> = Vec::new();

    let mut i = 0;
    // A leading sound belongs to the sequence as a whole; the first frame
    // carries it.
    let mut carried_sound = None;
    while i < slots.len() {
        let slot = &slots[i];
        let start = i;
        i += 1;

        let mut sound = carried_sound.take();
        if slot.flags & FLAG_SOUND != 0 && i < slots.len() {
            sound = Some(slots[i].id);
            i += 1;
        }

        let mut branches = Vec::new();
        if slot.flags & FLAG_BRANCH != 0 {
            while i < slots.len() {
                let b = &slots[i];
                i += 1;
                branches.push((b.id as usize, b.value));
                if b.flags & FLAG_SOUND != 0 && i < slots.len() {
                    sound = Some(slots[i].id);
                    i += 1;
                }
                if b.flags & FLAG_BRANCH == 0 {
                    break;
                }
            }
        }

        let image_index = if slot.id == BLANK {
            None
        } else {
            slot.id
                .checked_sub(first_frame)
                .filter(|i| (*i as usize) < frame_total)
        };

        slot_to_frame[start] = frames.len();
        if !branches.is_empty() {
            pending.push((frames.len(), branches));
        }
        frames.push(Frame {
            images: image_index
                .map(|i| {
                    vec![FrameImage {
                        image_index: i as u32,
                        x: 0,
                        y: 0,
                    }]
                })
                .unwrap_or_default(),
            audio_index: sound,
            // Durations are milliseconds; Frame counts hundredths of a second.
            duration: (slot.value as u32 / 10).min(u16::MAX as u32) as u16,
            exit_frame: -1,
            branches: Vec::new(),
            overlays: Vec::new(),
        });

        if slot.flags & FLAG_LAST != 0 {
            break;
        }
    }

    // Actor's probabilities are a cascade: each branch takes its share of what
    // the earlier ones left, and the last one catches the remainder. The player
    // wants plain cumulative percentages, so fold the cascade out here.
    for (frame, branches) in pending {
        let mut remaining = 100u32;
        let last = branches.len() - 1;
        let mut resolved = Vec::with_capacity(branches.len());
        for (n, (target, value)) in branches.into_iter().enumerate() {
            let share = if n == last {
                remaining
            } else {
                remaining * value as u32 / 0x8000
            };
            remaining -= share;
            let Some(&index) = slot_to_frame.get(target) else {
                continue;
            };
            if index == usize::MAX || share == 0 {
                continue;
            }
            resolved.push(Branch {
                frame_index: index as u16,
                probability: share as u16,
            });
        }
        frames[frame].branches = resolved;
    }

    frames
}

/// A 6x7x6 colour cube. Actor frames are composited straight to RGBA, so this
/// only stands in for the palette field of [`CharacterInfo`].
fn palette() -> Vec<Rgb> {
    let mut p = vec![Rgb { r: 0, g: 0, b: 0 }];
    for r in 0..6 {
        for g in 0..7 {
            for b in 0..6 {
                p.push(Rgb {
                    r: (r * 51) as u8,
                    g: (g * 255 / 6) as u8,
                    b: (b * 51) as u8,
                });
            }
        }
    }
    p.resize(256, Rgb { r: 0, g: 0, b: 0 });
    p
}

pub(crate) fn parse(data: Vec<u8>) -> Result<Character, Error> {
    if !data.starts_with(b"LP") {
        return Err(Error::Unsupported("not a Microsoft Actor file".into()));
    }
    let version = u16_at(&data, 2)?;
    if !(1..=2).contains(&version) {
        return Err(Error::Unsupported(format!(
            "unknown Microsoft Actor version {}",
            version
        )));
    }
    let name_len = u16_at(&data, 8)? as usize;
    let header = 18 + name_len;
    let name_bytes = data
        .get(18..header)
        .ok_or_else(|| Error::Parse("truncated Actor name".into()))?;
    let name =
        String::from_utf8_lossy(name_bytes.strip_suffix(&[0]).unwrap_or(name_bytes)).into_owned();

    let first_frame = u16_at(&data, header + 4)?;
    let width = u16_at(&data, header + 12)?.saturating_mul(OVERSAMPLE);
    let height = u16_at(&data, header + 14)?.saturating_mul(OVERSAMPLE);
    if width == 0 || height == 0 {
        return Err(Error::Parse("Actor character has an empty canvas".into()));
    }
    let pointers: Vec<usize> = (0..8)
        .map(|i| u32_at(&data, header + 38 + i * 4).map(|v| header + v as usize))
        .collect::<Result<_, _>>()?;
    if pointers.iter().any(|&p| p > data.len()) {
        return Err(Error::Parse("Actor section offset past end of file".into()));
    }

    let offsets = parse_offsets(&data, pointers[1], pointers[2])?;
    let ranges: Vec<_> = offsets
        .windows(2)
        .map(|pair| pointers[0] + pair[0] as usize..pointers[0] + pair[1] as usize)
        .collect();
    if first_frame as usize >= ranges.len() {
        return Err(Error::Parse("Actor first-frame index is invalid".into()));
    }
    let assets = ranges[..first_frame as usize].to_vec();

    let sx = width as f32 / LOGICAL_WIDTH as f32;
    let sy = height as f32 / LOGICAL_HEIGHT as f32;
    let frames: Vec<Vec<Layer>> = ranges[first_frame as usize..]
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, r)| {
            parse_layers(&data, r, sx, sy).map_err(|e| {
                Error::Parse(format!("Actor frame {}: {}", i + first_frame as usize, e))
            })
        })
        .collect::<Result<_, _>>()?;

    // Rasterize each metafile once, at the largest size any frame draws it.
    let mut asset_size = vec![(1usize, 1usize); assets.len()];
    for layers in &frames {
        for layer in layers {
            if let Some(slot) = asset_size.get_mut(layer.asset as usize) {
                let w = (layer.x1 - layer.x0).ceil().clamp(1.0, 4096.0) as usize;
                let h = (layer.y1 - layer.y0).ceil().clamp(1.0, 4096.0) as usize;
                slot.0 = slot.0.max(w);
                slot.1 = slot.1.max(h);
            }
        }
    }

    let sound_offsets = parse_offsets(&data, pointers[3], pointers[4])?;
    let audio: Vec<Vec<u8>> = sound_offsets
        .windows(2)
        .filter_map(|pair| {
            data.get(pointers[2] + pair[0] as usize..pointers[2] + pair[1] as usize)
                .map(|s| s.to_vec())
        })
        .collect();

    // The action table names the sequences; its `start` field is 1-based, so
    // the highest start plus its variant count is one past the last sequence.
    let action_count = u32_at(&data, pointers[5])? as usize;
    let actions: Vec<(u16, usize, usize)> = (0..action_count)
        .map(|i| {
            let p = pointers[5] + 4 + i * 6;
            Ok((
                u16_at(&data, p)?,
                u16_at(&data, p + 2)? as usize,
                u16_at(&data, p + 4)? as usize,
            ))
        })
        .collect::<Result<_, Error>>()?;
    let sequence_count = actions
        .iter()
        .map(|&(_, variants, start)| start + variants)
        .max()
        .unwrap_or(1)
        .saturating_sub(1);

    let sequences = parse_sequences(&data, pointers[4]..pointers[5], sequence_count);

    let mut animations = Vec::new();
    for (action, variants, start) in actions {
        for variant in 0..variants {
            // `start` counts from one.
            let Some(slots) = sequences.get((start + variant).wrapping_sub(1)) else {
                continue;
            };
            let anim_frames = build_frames(slots, first_frame, frames.len());
            if anim_frames.is_empty() {
                continue;
            }
            animations.push(Animation {
                name: if variants > 1 {
                    format!("Action {} ({})", action, variant + 1)
                } else {
                    format!("Action {}", action)
                },
                transition: Transition::None,
                return_animation: String::new(),
                frames: anim_frames,
            });
        }
    }
    if animations.is_empty() {
        return Err(Error::Parse(
            "Actor file contains no playable animations".into(),
        ));
    }

    let info = CharacterInfo {
        major_version: version,
        minor_version: 0,
        guid: [0; 16],
        width,
        height,
        transparent_index: 0,
        flags: 0,
        voice: None,
        balloon: None,
        palette: palette(),
        states: Vec::new(),
        localized: vec![LocalizedInfo {
            language_id: 0x0409,
            name,
            description: if version == 1 {
                "Microsoft Bob Actor"
            } else {
                "Microsoft Office 97 Actor"
            }
            .into(),
            extra: String::new(),
        }],
    };
    let source = ActorSource {
        data,
        assets,
        asset_size,
        frames,
        width,
        height,
        cache: RefCell::new(HashMap::new()),
    };
    Ok(Character::from_actor(info, animations, source, audio))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sequence region holding one sequence: a frame carrying a sound, a
    /// frame carrying a branch back to the first, and a closing frame.
    /// Byte offset of the first slot's id: the two-byte region prefix plus the
    /// sequence header's count and flags words.
    const FIRST_SLOT: usize = 6;

    #[rustfmt::skip]
    fn region() -> Vec<u8> {
        let mut out = vec![0x00, 0x01]; // the region's two-byte prefix
        for word in [
            5, 0, // slot count, sequence flags
            10, 100, FLAG_SOUND, // frame 10, 100ms, sound follows
            3, 0x0100, 0, // the sound record: audio index 3
            11, 200, FLAG_BRANCH, // frame 11, 200ms, branch follows
            0, 0x4000, 0, // branch back to slot 0
            12, 50, FLAG_LAST, // frame 12, 50ms, end of sequence
        ] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out
    }

    #[test]
    fn splits_sequences_on_their_slot_count() {
        let data = region();
        let sequences = parse_sequences(&data, 0..data.len(), 1);
        assert_eq!(sequences.len(), 1);
        assert_eq!(sequences[0].len(), 5);
        assert_eq!(sequences[0][0].id, 10);
    }

    #[test]
    fn resolves_sounds_branches_and_durations() {
        let data = region();
        let sequences = parse_sequences(&data, 0..data.len(), 1);
        let frames = build_frames(&sequences[0], 10, 5);

        // Sound and branch records are consumed, not mistaken for frames.
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].images[0].image_index, 0);
        assert_eq!(frames[0].audio_index, Some(3));
        assert_eq!(frames[0].duration, 10); // 100ms in hundredths
        assert_eq!(frames[1].images[0].image_index, 1);
        assert_eq!(frames[2].images[0].image_index, 2);

        // The only branch in a chain catches everything, and its target slot
        // resolves to the frame that slot became.
        assert_eq!(frames[1].branches.len(), 1);
        assert_eq!(frames[1].branches[0].frame_index, 0);
        assert_eq!(frames[1].branches[0].probability, 100);
    }

    #[test]
    fn treats_the_blank_id_as_an_empty_frame() {
        let mut data = region();
        data[FIRST_SLOT..FIRST_SLOT + 2].copy_from_slice(&BLANK.to_le_bytes());
        let sequences = parse_sequences(&data, 0..data.len(), 1);
        let frames = build_frames(&sequences[0], 10, 5);
        assert!(frames[0].images.is_empty());
        assert_eq!(frames[0].audio_index, Some(3));
    }
}
