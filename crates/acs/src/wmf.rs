//! A small Windows Metafile rasterizer.
//!
//! Microsoft Actor stores its artwork as placeable WMFs rather than bitmaps, so
//! the character has to be drawn rather than decoded. The files are narrow in
//! scope — across every Actor character we have they use only polygons,
//! polylines, ellipses and rounded rectangles, drawn with indirect pens and
//! brushes — so this covers that set and ignores the rest of the format.
//!
//! Output is straight-alpha RGBA: anything the metafile never paints stays
//! transparent, which is what lets frames composite over each other.

use crate::Error;

// Record functions we act on. Everything else is skipped.
const META_EOF: u16 = 0x0000;
const META_SAVEDC: u16 = 0x001E;
const META_SETBKMODE: u16 = 0x0102;
const META_SETMAPMODE: u16 = 0x0103;
const META_SETROP2: u16 = 0x0104;
const META_SETPOLYFILLMODE: u16 = 0x0106;
const META_RESTOREDC: u16 = 0x0127;
const META_SELECTOBJECT: u16 = 0x012D;
const META_DELETEOBJECT: u16 = 0x01F0;
const META_SETBKCOLOR: u16 = 0x0201;
const META_SETTEXTCOLOR: u16 = 0x0209;
const META_SETWINDOWORG: u16 = 0x020B;
const META_SETWINDOWEXT: u16 = 0x020C;
const META_OFFSETWINDOWORG: u16 = 0x020F;
const META_LINETO: u16 = 0x0213;
const META_MOVETO: u16 = 0x0214;
const META_CREATEPENINDIRECT: u16 = 0x02FA;
const META_CREATEFONTINDIRECT: u16 = 0x02FB;
const META_CREATEBRUSHINDIRECT: u16 = 0x02FC;
const META_POLYGON: u16 = 0x0324;
const META_POLYLINE: u16 = 0x0325;
const META_SCALEWINDOWEXT: u16 = 0x0410;
const META_ELLIPSE: u16 = 0x0418;
const META_RECTANGLE: u16 = 0x041B;
const META_POLYPOLYGON: u16 = 0x0538;
const META_ROUNDRECT: u16 = 0x061C;

const PS_NULL: u16 = 5;
const BS_SOLID: u16 = 0;
const BS_HOLLOW: u16 = 1;

/// Vertical sub-samples per pixel row. Four is enough to hide the stair-stepping
/// on artwork this small without making rasterizing the dominant cost.
const SUBSAMPLES: usize = 4;

/// A rasterized metafile, 8 bits per channel, non-premultiplied.
pub struct Bitmap {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

/// WMF stores colours as `0x00BBGGRR`.
fn colorref(v: u32) -> Rgb {
    Rgb(v as u8, (v >> 8) as u8, (v >> 16) as u8)
}

#[derive(Clone, Copy)]
enum Object {
    None,
    Pen { style: u16, width: i16, color: Rgb },
    Brush { style: u16, color: Rgb },
    /// Fonts are tracked only so object-table indices stay aligned; Actor's
    /// handful of text records are not drawn.
    Font,
}

#[derive(Clone, Copy)]
struct Dc {
    pen: Option<(u16, i16, Rgb)>,
    brush: Option<(u16, Rgb)>,
    winding_fill: bool,
    win_org: (i32, i32),
    win_ext: (i32, i32),
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn i16(&mut self) -> i16 {
        self.u16() as i16
    }

    fn u16(&mut self) -> u16 {
        let v = self
            .data
            .get(self.pos..self.pos + 2)
            .and_then(|b| b.try_into().ok())
            .map(u16::from_le_bytes)
            .unwrap_or(0);
        self.pos += 2;
        v
    }

    fn u32(&mut self) -> u32 {
        let lo = self.u16() as u32;
        let hi = self.u16() as u32;
        lo | (hi << 16)
    }
}

/// Rasterizes a placeable metafile into a `width` x `height` RGBA bitmap.
pub fn render(wmf: &[u8], width: usize, height: usize) -> Result<Bitmap, Error> {
    if width == 0 || height == 0 {
        return Ok(Bitmap {
            width: width.max(1),
            height: height.max(1),
            rgba: vec![0; width.max(1) * height.max(1) * 4],
        });
    }

    // Placeable header: a 22-byte preamble carrying the bounding box, followed
    // by the ordinary 18-byte metafile header.
    let mut body = wmf;
    let mut bbox = None;
    if wmf.len() >= 22 && wmf[..4] == [0xD7, 0xCD, 0xC6, 0x9A] {
        let mut r = Reader { data: wmf, pos: 6 };
        let (l, t, right, b) = (r.i16(), r.i16(), r.i16(), r.i16());
        bbox = Some((l as i32, t as i32, right as i32, b as i32));
        body = &wmf[22..];
    }
    if body.len() < 18 {
        return Err(Error::Parse("truncated Actor metafile".into()));
    }
    let records = &body[18..];

    let (org, ext) = match bbox {
        Some((l, t, r, b)) if r != l && b != t => ((l, t), (r - l, b - t)),
        _ => ((0, 0), (width as i32, height as i32)),
    };

    let mut canvas = Canvas::new(width, height);
    let mut dc = Dc {
        pen: None,
        brush: None,
        winding_fill: false,
        win_org: org,
        win_ext: ext,
    };
    let mut stack: Vec<Dc> = Vec::new();
    let mut objects: Vec<Object> = Vec::new();
    let mut cursor = (0i32, 0i32);
    let mut points: Vec<(f32, f32)> = Vec::new();

    let mut p = 0usize;
    while p + 6 <= records.len() {
        let size = u32::from_le_bytes(records[p..p + 4].try_into().unwrap()) as usize;
        let func = u16::from_le_bytes(records[p + 4..p + 6].try_into().unwrap());
        if size < 3 {
            break; // A record is at least its own size and function words.
        }
        let end = match p.checked_add(size * 2) {
            Some(e) if e <= records.len() => e,
            _ => break,
        };
        let mut r = Reader {
            data: &records[..end],
            pos: p + 6,
        };

        match func {
            META_EOF => break,
            META_SAVEDC => stack.push(dc),
            META_RESTOREDC => {
                // The parameter is a level count, but Actor only ever nests by
                // one, so treating it as a plain pop matches the files.
                if let Some(saved) = stack.pop() {
                    dc = saved;
                }
            }
            META_SETPOLYFILLMODE => dc.winding_fill = r.u16() == 2,
            META_SETWINDOWORG => {
                let y = r.i16() as i32;
                let x = r.i16() as i32;
                dc.win_org = (x, y);
            }
            META_SETWINDOWEXT => {
                let y = r.i16() as i32;
                let x = r.i16() as i32;
                if x != 0 && y != 0 {
                    dc.win_ext = (x, y);
                }
            }
            META_OFFSETWINDOWORG => {
                let y = r.i16() as i32;
                let x = r.i16() as i32;
                dc.win_org = (dc.win_org.0 + x, dc.win_org.1 + y);
            }
            META_SCALEWINDOWEXT => {
                let (yd, yn, xd, xn) = (r.i16(), r.i16(), r.i16(), r.i16());
                if xd != 0 && yd != 0 {
                    dc.win_ext.0 = dc.win_ext.0 * xn as i32 / xd as i32;
                    dc.win_ext.1 = dc.win_ext.1 * yn as i32 / yd as i32;
                }
            }
            META_CREATEPENINDIRECT => {
                let style = r.u16();
                let w = r.i16();
                let _wy = r.i16();
                let color = colorref(r.u32());
                add_object(
                    &mut objects,
                    Object::Pen {
                        style,
                        width: w,
                        color,
                    },
                );
            }
            META_CREATEBRUSHINDIRECT => {
                let style = r.u16();
                let color = colorref(r.u32());
                add_object(&mut objects, Object::Brush { style, color });
            }
            META_CREATEFONTINDIRECT => add_object(&mut objects, Object::Font),
            META_DELETEOBJECT => {
                let i = r.u16() as usize;
                if let Some(slot) = objects.get_mut(i) {
                    *slot = Object::None;
                }
            }
            META_SELECTOBJECT => {
                let i = r.u16() as usize;
                match objects.get(i).copied().unwrap_or(Object::None) {
                    Object::Pen {
                        style,
                        width,
                        color,
                    } => dc.pen = Some((style, width, color)),
                    Object::Brush { style, color } => dc.brush = Some((style, color)),
                    _ => {}
                }
            }
            META_MOVETO => {
                let y = r.i16() as i32;
                let x = r.i16() as i32;
                cursor = (x, y);
            }
            META_LINETO => {
                let y = r.i16() as i32;
                let x = r.i16() as i32;
                let a = map(&dc, cursor, width, height);
                let b = map(&dc, (x, y), width, height);
                canvas.stroke(&[a, b], false, &dc);
                cursor = (x, y);
            }
            META_POLYGON | META_POLYLINE => {
                let n = r.u16() as usize;
                points.clear();
                points.reserve(n);
                for _ in 0..n {
                    let x = r.i16() as i32;
                    let y = r.i16() as i32;
                    points.push(map(&dc, (x, y), width, height));
                }
                if func == META_POLYGON {
                    canvas.fill(&[points.as_slice()], &dc);
                    canvas.stroke(&points, true, &dc);
                } else {
                    canvas.stroke(&points, false, &dc);
                }
            }
            META_POLYPOLYGON => {
                let polys = r.u16() as usize;
                let counts: Vec<usize> = (0..polys).map(|_| r.u16() as usize).collect();
                points.clear();
                let mut spans = Vec::with_capacity(polys);
                for &c in &counts {
                    let start = points.len();
                    for _ in 0..c {
                        let x = r.i16() as i32;
                        let y = r.i16() as i32;
                        points.push(map(&dc, (x, y), width, height));
                    }
                    spans.push(start..points.len());
                }
                // Sub-paths fill as one figure so interior holes punch through.
                let rings: Vec<&[(f32, f32)]> =
                    spans.iter().map(|s| &points[s.clone()]).collect();
                canvas.fill(&rings, &dc);
                for s in &spans {
                    canvas.stroke(&points[s.clone()], true, &dc);
                }
            }
            META_ELLIPSE | META_RECTANGLE => {
                let b = r.i16() as i32;
                let right = r.i16() as i32;
                let t = r.i16() as i32;
                let l = r.i16() as i32;
                points.clear();
                if func == META_ELLIPSE {
                    ellipse_points(&mut points, &dc, (l, t, right, b), width, height);
                } else {
                    for &(x, y) in &[(l, t), (right, t), (right, b), (l, b)] {
                        points.push(map(&dc, (x, y), width, height));
                    }
                }
                canvas.fill(&[points.as_slice()], &dc);
                canvas.stroke(&points, true, &dc);
            }
            META_ROUNDRECT => {
                let _eh = r.i16() as i32;
                let _ew = r.i16() as i32;
                let b = r.i16() as i32;
                let right = r.i16() as i32;
                let t = r.i16() as i32;
                let l = r.i16() as i32;
                // The corner rounding is a couple of logical units on artwork
                // this small, so a plain rectangle is visually identical.
                points.clear();
                for &(x, y) in &[(l, t), (right, t), (right, b), (l, b)] {
                    points.push(map(&dc, (x, y), width, height));
                }
                canvas.fill(&[points.as_slice()], &dc);
                canvas.stroke(&points, true, &dc);
            }
            META_SETBKMODE | META_SETMAPMODE | META_SETROP2 | META_SETBKCOLOR
            | META_SETTEXTCOLOR => {}
            _ => {}
        }

        p = end;
    }

    Ok(canvas.finish())
}

/// Windows hands out the lowest free slot in the object table.
fn add_object(objects: &mut Vec<Object>, object: Object) {
    if let Some(slot) = objects.iter_mut().find(|o| matches!(o, Object::None)) {
        *slot = object;
    } else {
        objects.push(object);
    }
}

/// Maps a logical point onto the output bitmap.
fn map(dc: &Dc, (x, y): (i32, i32), width: usize, height: usize) -> (f32, f32) {
    let sx = width as f32 / dc.win_ext.0 as f32;
    let sy = height as f32 / dc.win_ext.1 as f32;
    (
        (x - dc.win_org.0) as f32 * sx,
        (y - dc.win_org.1) as f32 * sy,
    )
}

fn ellipse_points(
    out: &mut Vec<(f32, f32)>,
    dc: &Dc,
    (l, t, r, b): (i32, i32, i32, i32),
    width: usize,
    height: usize,
) {
    let (cx, cy) = ((l + r) as f32 / 2.0, (t + b) as f32 / 2.0);
    let (rx, ry) = ((r - l) as f32 / 2.0, (b - t) as f32 / 2.0);
    let sx = width as f32 / dc.win_ext.0 as f32;
    let sy = height as f32 / dc.win_ext.1 as f32;
    // Enough segments that the curve stays smooth at the sizes Actor draws.
    let steps = 48;
    for i in 0..steps {
        let a = i as f32 / steps as f32 * std::f32::consts::TAU;
        out.push((
            (cx + rx * a.cos() - dc.win_org.0 as f32) * sx,
            (cy + ry * a.sin() - dc.win_org.1 as f32) * sy,
        ));
    }
}

// --- Rasterizing --------------------------------------------------------

struct Canvas {
    width: usize,
    height: usize,
    /// Straight-alpha RGBA in floats; converted on the way out.
    pixels: Vec<[f32; 4]>,
    coverage: Vec<f32>,
    edges: Vec<Edge>,
}

#[derive(Clone, Copy)]
struct Edge {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    dir: f32,
}

impl Edge {
    fn top(&self) -> f32 {
        self.y0.min(self.y1)
    }

    fn bottom(&self) -> f32 {
        self.y0.max(self.y1)
    }
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![[0.0; 4]; width * height],
            coverage: vec![0.0; width * height],
            edges: Vec::new(),
        }
    }

    fn fill(&mut self, rings: &[&[(f32, f32)]], dc: &Dc) {
        let Some((style, color)) = dc.brush else {
            return;
        };
        if style == BS_HOLLOW || (style != BS_SOLID && style != 2) {
            return;
        }
        self.edges.clear();
        for ring in rings {
            push_ring(&mut self.edges, ring);
        }
        self.scan(dc.winding_fill, color);
    }

    fn stroke(&mut self, points: &[(f32, f32)], close: bool, dc: &Dc) {
        let Some((style, width, color)) = dc.pen else {
            return;
        };
        if style == PS_NULL || points.len() < 2 {
            return;
        }
        // Pen widths are logical units; scale into device space the same way
        // points are, and never let a visible pen vanish below one pixel.
        let sx = self.width as f32 / dc.win_ext.0 as f32;
        let sy = self.height as f32 / dc.win_ext.1 as f32;
        let t = (width as f32 * (sx.abs() + sy.abs()) / 2.0).max(1.0);

        self.edges.clear();
        let n = points.len();
        let segments = if close { n } else { n - 1 };
        for i in 0..segments {
            let a = points[i];
            let b = points[(i + 1) % n];
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len = (dx * dx + dy * dy).sqrt();
            if len < 1e-6 {
                continue;
            }
            let (nx, ny) = (-dy / len * t / 2.0, dx / len * t / 2.0);
            let quad = [
                (a.0 + nx, a.1 + ny),
                (b.0 + nx, b.1 + ny),
                (b.0 - nx, b.1 - ny),
                (a.0 - nx, a.1 - ny),
            ];
            push_ring_ccw(&mut self.edges, &quad);
        }
        // Square off the joints so corners do not show a notch.
        if t > 1.5 {
            let joints = if close { n } else { n - 1 };
            for &(x, y) in points.iter().take(joints + 1).skip(if close { 0 } else { 1 }) {
                let h = t / 2.0;
                push_ring_ccw(
                    &mut self.edges,
                    &[
                        (x - h, y - h),
                        (x + h, y - h),
                        (x + h, y + h),
                        (x - h, y + h),
                    ],
                );
            }
        }
        // Overlapping quads must union rather than cancel, so always non-zero.
        self.scan(true, color);
    }

    /// Accumulates coverage for the current edge list and composites `color`.
    fn scan(&mut self, winding: bool, color: Rgb) {
        if self.edges.is_empty() {
            return;
        }
        let (mut ymin, mut ymax) = (f32::MAX, f32::MIN);
        let (mut xmin, mut xmax) = (f32::MAX, f32::MIN);
        for e in &self.edges {
            ymin = ymin.min(e.y0.min(e.y1));
            ymax = ymax.max(e.y0.max(e.y1));
            xmin = xmin.min(e.x0.min(e.x1));
            xmax = xmax.max(e.x0.max(e.x1));
        }
        let y0 = (ymin.floor().max(0.0)) as usize;
        let y1 = (ymax.ceil().min(self.height as f32)).max(0.0) as usize;
        let x0 = (xmin.floor().max(0.0)) as usize;
        let x1 = (xmax.ceil().min(self.width as f32)).max(0.0) as usize;
        if y0 >= y1 || x0 >= x1 {
            return;
        }

        // Sweep with an active edge list; shapes here can carry hundreds of
        // edges and only a handful cross any one scanline.
        let mut order: Vec<u32> = (0..self.edges.len() as u32).collect();
        order.sort_by(|&a, &b| {
            let (ta, tb) = (self.edges[a as usize].top(), self.edges[b as usize].top());
            ta.partial_cmp(&tb).unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut pending = 0usize;
        let mut active: Vec<u32> = Vec::new();

        let mut crossings: Vec<(f32, f32)> = Vec::new();
        let weight = 1.0 / SUBSAMPLES as f32;
        for y in y0..y1 {
            let row = y * self.width;
            while pending < order.len() && self.edges[order[pending] as usize].top() < (y + 1) as f32
            {
                active.push(order[pending]);
                pending += 1;
            }
            let top = y as f32;
            active.retain(|&e| self.edges[e as usize].bottom() > top);
            if active.is_empty() {
                continue;
            }
            for s in 0..SUBSAMPLES {
                let sy = y as f32 + (s as f32 + 0.5) / SUBSAMPLES as f32;
                crossings.clear();
                for &i in &active {
                    let e = &self.edges[i as usize];
                    if sy < e.top() || sy >= e.bottom() {
                        continue;
                    }
                    let t = (sy - e.y0) / (e.y1 - e.y0);
                    crossings.push((e.x0 + t * (e.x1 - e.x0), e.dir));
                }
                if crossings.len() < 2 {
                    continue;
                }
                crossings.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

                let mut count = 0.0f32;
                for i in 0..crossings.len() - 1 {
                    count += crossings[i].1;
                    let inside = if winding {
                        count.abs() > 0.5
                    } else {
                        // Even-odd: every other span between crossings.
                        i % 2 == 0
                    };
                    if inside {
                        add_span(
                            &mut self.coverage[row..row + self.width],
                            crossings[i].0,
                            crossings[i + 1].0,
                            weight,
                        );
                    }
                }
            }
        }

        // Composite the coverage, clearing it as we go so the buffer is clean
        // for the next drawing operation.
        let (r, g, b) = (
            color.0 as f32 / 255.0,
            color.1 as f32 / 255.0,
            color.2 as f32 / 255.0,
        );
        for y in y0..y1 {
            let row = y * self.width;
            for x in x0..x1 {
                let a = std::mem::take(&mut self.coverage[row + x]).clamp(0.0, 1.0);
                if a <= 0.0 {
                    continue;
                }
                let dst = &mut self.pixels[row + x];
                let out_a = a + dst[3] * (1.0 - a);
                if out_a <= 0.0 {
                    continue;
                }
                for (i, c) in [r, g, b].into_iter().enumerate() {
                    dst[i] = (c * a + dst[i] * dst[3] * (1.0 - a)) / out_a;
                }
                dst[3] = out_a;
            }
        }
    }

    fn finish(self) -> Bitmap {
        let mut rgba = Vec::with_capacity(self.width * self.height * 4);
        for p in &self.pixels {
            for c in p {
                rgba.push((c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            }
        }
        Bitmap {
            width: self.width,
            height: self.height,
            rgba,
        }
    }
}

fn push_ring(edges: &mut Vec<Edge>, ring: &[(f32, f32)]) {
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        if (a.1 - b.1).abs() < 1e-9 {
            continue; // Horizontal edges never contribute a crossing.
        }
        edges.push(Edge {
            x0: a.0,
            y0: a.1,
            x1: b.0,
            y1: b.1,
            dir: if b.1 > a.1 { 1.0 } else { -1.0 },
        });
    }
}

/// Pushes a ring wound so its non-zero winding is positive, which lets
/// overlapping shapes union instead of cancelling.
fn push_ring_ccw(edges: &mut Vec<Edge>, ring: &[(f32, f32)]) {
    let mut area = 0.0;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        area += a.0 * b.1 - b.0 * a.1;
    }
    if area < 0.0 {
        let reversed: Vec<(f32, f32)> = ring.iter().rev().copied().collect();
        push_ring(edges, &reversed);
    } else {
        push_ring(edges, ring);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a placeable metafile that fills the middle half of a 100x100
    /// logical canvas with a solid red square and no outline.
    fn red_square() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0xD7, 0xCD, 0xC6, 0x9A]);
        out.extend_from_slice(&0u16.to_le_bytes()); // handle
        for v in [0i16, 0, 100, 100] {
            out.extend_from_slice(&v.to_le_bytes()); // bounding box
        }
        out.extend_from_slice(&100u16.to_le_bytes()); // units per inch
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved
        out.extend_from_slice(&0u16.to_le_bytes()); // checksum

        out.extend_from_slice(&1u16.to_le_bytes()); // type
        out.extend_from_slice(&9u16.to_le_bytes()); // header size, words
        out.extend_from_slice(&0x0300u16.to_le_bytes()); // version
        out.extend_from_slice(&0u32.to_le_bytes()); // total size
        out.extend_from_slice(&2u16.to_le_bytes()); // object count
        out.extend_from_slice(&0u32.to_le_bytes()); // max record
        out.extend_from_slice(&0u16.to_le_bytes()); // members

        let mut record = |func: u16, params: &[u16]| {
            out.extend_from_slice(&(3 + params.len() as u32).to_le_bytes());
            out.extend_from_slice(&func.to_le_bytes());
            for p in params {
                out.extend_from_slice(&p.to_le_bytes());
            }
        };
        // Solid red brush, then a null pen so only the fill shows.
        record(META_CREATEBRUSHINDIRECT, &[BS_SOLID, 0x00FF, 0x0000, 0]);
        record(META_SELECTOBJECT, &[0]);
        record(META_CREATEPENINDIRECT, &[PS_NULL, 0, 0, 0, 0]);
        record(META_SELECTOBJECT, &[1]);
        record(
            META_POLYGON,
            &[4, 25, 25, 75, 25, 75, 75, 25, 75].map(|v: i16| v as u16).as_slice(),
        );
        record(META_EOF, &[]);
        out
    }

    fn pixel(bitmap: &Bitmap, x: usize, y: usize) -> [u8; 4] {
        let o = (y * bitmap.width + x) * 4;
        bitmap.rgba[o..o + 4].try_into().unwrap()
    }

    #[test]
    fn fills_a_polygon_and_leaves_the_rest_transparent() {
        let bitmap = render(&red_square(), 100, 100).unwrap();
        assert_eq!(bitmap.width, 100);
        assert_eq!(pixel(&bitmap, 50, 50), [255, 0, 0, 255]);
        // Outside the polygon nothing was painted at all.
        assert_eq!(pixel(&bitmap, 5, 5)[3], 0);
        assert_eq!(pixel(&bitmap, 95, 95)[3], 0);
    }

    #[test]
    fn scales_to_the_requested_size() {
        let bitmap = render(&red_square(), 20, 20).unwrap();
        assert_eq!((bitmap.width, bitmap.height), (20, 20));
        assert_eq!(pixel(&bitmap, 10, 10), [255, 0, 0, 255]);
        assert_eq!(pixel(&bitmap, 1, 1)[3], 0);
    }

    #[test]
    fn antialiases_edges() {
        // The square's edge lands mid-pixel at this scale, so the boundary
        // column should be partly covered rather than hard on or off.
        let bitmap = render(&red_square(), 41, 41).unwrap();
        let edge = pixel(&bitmap, 10, 20)[3];
        assert!(edge > 0 && edge < 255, "expected a soft edge, got {}", edge);
    }
}

/// Adds horizontal coverage for `[x0, x1)` on one sub-scanline, weighting the
/// partially covered pixels at each end by how much of them the span covers.
fn add_span(row: &mut [f32], x0: f32, x1: f32, weight: f32) {
    let a = x0.max(0.0);
    let b = x1.min(row.len() as f32);
    if b <= a {
        return;
    }
    let first = a.floor() as usize;
    let last = (b.ceil() as usize).min(row.len());
    for (px, slot) in row.iter_mut().enumerate().take(last).skip(first) {
        let l = a.max(px as f32);
        let r = b.min(px as f32 + 1.0);
        if r > l {
            *slot += (r - l) * weight;
        }
    }
}
