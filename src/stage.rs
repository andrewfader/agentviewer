//! The character stage: a GtkWidget that renders the composited frame and the
//! word balloon as GSK nodes, so drawing goes straight to the Vulkan renderer
//! as a textured quad plus a handful of shapes.

use std::cell::{Cell, RefCell};

use gtk::gdk;
use gtk::glib;
use gtk::graphene;
use gtk::gsk;
use gtk::pango;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// Backdrop drawn behind the character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backdrop {
    /// Alternating grey squares, the usual way to show transparency.
    #[default]
    Checker,
    Dark,
    Light,
}

/// Everything needed to draw the word balloon.
#[derive(Debug, Clone)]
pub struct Balloon {
    pub text: String,
    /// How much of `text` to reveal, in characters, for paced output.
    pub visible_chars: usize,
    pub foreground: gdk::RGBA,
    pub background: gdk::RGBA,
    pub border: gdk::RGBA,
    pub font_family: String,
    pub font_size_pt: f64,
    pub italic: bool,
    pub bold: bool,
    pub chars_per_line: usize,
}

const CHECKER_SIZE: f32 = 12.0;
const BALLOON_PADDING: f32 = 12.0;
const BALLOON_RADIUS: f32 = 10.0;
const BALLOON_GAP: f32 = 14.0;
const TAIL_WIDTH: f32 = 18.0;
const TAIL_HEIGHT: f32 = 14.0;
const BORDER_WIDTH: f32 = 1.5;

mod imp {
    use super::*;

    pub struct Stage {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub balloon: RefCell<Option<Balloon>>,
        pub backdrop: Cell<Backdrop>,
        /// Artwork extent within the texture, in texture pixels, as
        /// `(left, top, right, bottom)`. Used to anchor the balloon to the
        /// character rather than to the transparent canvas around it.
        pub content: Cell<Option<(f32, f32, f32, f32)>>,
        /// Zoom multiplier applied on top of fit-to-window scaling.
        pub zoom: Cell<f64>,
        pub fit: Cell<bool>,
        pub smooth: Cell<bool>,
    }

    impl Default for Stage {
        fn default() -> Self {
            Self {
                texture: RefCell::new(None),
                balloon: RefCell::new(None),
                backdrop: Cell::new(Backdrop::default()),
                content: Cell::new(None),
                zoom: Cell::new(1.0),
                fit: Cell::new(true),
                smooth: Cell::new(true),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Stage {
        const NAME: &'static str = "AgentViewStage";
        type Type = super::Stage;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Stage {}

    impl WidgetImpl for Stage {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let natural = match (&*self.texture.borrow(), orientation) {
                (Some(t), gtk::Orientation::Horizontal) => t.width(),
                (Some(t), _) => t.height(),
                (None, _) => 160,
            };
            // Minimum stays small so the window can still be shrunk.
            (48, natural.max(48), -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let w = widget.width() as f32;
            let h = widget.height() as f32;
            if w <= 0.0 || h <= 0.0 {
                return;
            }

            self.draw_backdrop(snapshot, w, h);

            let texture = self.texture.borrow();
            let Some(texture) = texture.as_ref() else { return };

            let (tw, th) = (texture.width() as f32, texture.height() as f32);
            if tw <= 0.0 || th <= 0.0 {
                return;
            }

            let mut scale = self.zoom.get() as f32;
            if self.fit.get() {
                // Leave headroom so the balloon has somewhere to go.
                let available_h = h * 0.72;
                scale *= (w / tw).min(available_h / th).min(4.0);
            }
            let dw = (tw * scale).max(1.0);
            let dh = (th * scale).max(1.0);

            // Sit the character on the lower half, balloon above it.
            let x = ((w - dw) / 2.0).round();
            let y = ((h - dh) * 0.72).round().max(0.0);

            let filter =
                if self.smooth.get() { gsk::ScalingFilter::Trilinear } else { gsk::ScalingFilter::Nearest };
            snapshot.append_scaled_texture(texture, filter, &graphene::Rect::new(x, y, dw, dh));

            if let Some(balloon) = self.balloon.borrow().as_ref() {
                // Point the balloon at the top-centre of the artwork itself.
                let (anchor_x, anchor_y) = match self.content.get() {
                    Some((left, top, right, _)) => {
                        (x + (left + right) / 2.0 * scale, y + top * scale)
                    }
                    None => (x + dw / 2.0, y),
                };
                self.draw_balloon(snapshot, balloon, w, anchor_x, anchor_y);
            }
        }
    }

    impl Stage {
        fn draw_backdrop(&self, snapshot: &gtk::Snapshot, w: f32, h: f32) {
            let full = graphene::Rect::new(0.0, 0.0, w, h);
            match self.backdrop.get() {
                Backdrop::Dark => {
                    snapshot.append_color(&gdk::RGBA::new(0.13, 0.13, 0.14, 1.0), &full);
                }
                Backdrop::Light => {
                    snapshot.append_color(&gdk::RGBA::new(0.98, 0.98, 0.98, 1.0), &full);
                }
                Backdrop::Checker => {
                    let a = gdk::RGBA::new(0.60, 0.60, 0.62, 1.0);
                    let b = gdk::RGBA::new(0.52, 0.52, 0.54, 1.0);
                    snapshot.append_color(&a, &full);
                    snapshot.push_clip(&full);
                    let cols = (w / CHECKER_SIZE).ceil() as i32;
                    let rows = (h / CHECKER_SIZE).ceil() as i32;
                    for row in 0..rows {
                        for col in 0..cols {
                            if (row + col) % 2 == 0 {
                                continue;
                            }
                            snapshot.append_color(
                                &b,
                                &graphene::Rect::new(
                                    col as f32 * CHECKER_SIZE,
                                    row as f32 * CHECKER_SIZE,
                                    CHECKER_SIZE,
                                    CHECKER_SIZE,
                                ),
                            );
                        }
                    }
                    snapshot.pop();
                }
            }
        }

        /// Draws the balloon above the character, with its tail pointing at
        /// `anchor_x` / `anchor_y` (the top-centre of the character).
        fn draw_balloon(
            &self,
            snapshot: &gtk::Snapshot,
            balloon: &Balloon,
            widget_w: f32,
            anchor_x: f32,
            anchor_y: f32,
        ) {
            let visible: String = balloon.text.chars().take(balloon.visible_chars).collect();
            if visible.trim().is_empty() {
                return;
            }

            let widget = self.obj();
            let layout = widget.create_pango_layout(Some(&visible));

            let mut font = pango::FontDescription::new();
            if !balloon.font_family.is_empty() {
                font.set_family(&balloon.font_family);
            }
            font.set_size((balloon.font_size_pt * pango::SCALE as f64) as i32);
            if balloon.italic {
                font.set_style(pango::Style::Italic);
            }
            if balloon.bold {
                font.set_weight(pango::Weight::Bold);
            }
            layout.set_font_description(Some(&font));

            // Width follows the character's authored line length, bounded by
            // the space actually available.
            let by_chars = (balloon.chars_per_line as f64 * balloon.font_size_pt * 0.62) as f32;
            let max_w = (widget_w - 2.0 * BALLOON_PADDING - 16.0).max(80.0);
            // In a very narrow window the preferred minimum can exceed the
            // space available, so the lower bound yields to the upper one.
            let wrap_w = by_chars.clamp(120.0f32.min(max_w), max_w);
            layout.set_wrap(pango::WrapMode::WordChar);
            layout.set_width((wrap_w * pango::SCALE as f32) as i32);

            let (text_w, text_h) = layout.pixel_size();
            let box_w = text_w as f32 + BALLOON_PADDING * 2.0;
            let box_h = text_h as f32 + BALLOON_PADDING * 2.0;

            let mut box_x = anchor_x - box_w / 2.0;
            box_x = box_x.clamp(8.0, (widget_w - box_w - 8.0).max(8.0));
            let box_y = (anchor_y - BALLOON_GAP - TAIL_HEIGHT - box_h).max(8.0);

            let rect = graphene::Rect::new(box_x, box_y, box_w, box_h);
            let rounded = gsk::RoundedRect::from_rect(rect, BALLOON_RADIUS);

            // Tail, drawn first so the body's border overlaps its base cleanly.
            let tail_base_y = box_y + box_h;
            // Keep the tail clear of the rounded corners; centre it when the
            // balloon is too narrow to offer any choice.
            let tail_lo = box_x + BALLOON_RADIUS + TAIL_WIDTH / 2.0;
            let tail_hi = box_x + box_w - BALLOON_RADIUS - TAIL_WIDTH / 2.0;
            let tail_x =
                if tail_lo <= tail_hi { anchor_x.clamp(tail_lo, tail_hi) } else { box_x + box_w / 2.0 };
            let tail_tip_y = (tail_base_y + TAIL_HEIGHT).min(anchor_y);

            let builder = gsk::PathBuilder::new();
            builder.move_to(tail_x - TAIL_WIDTH / 2.0, tail_base_y - 2.0);
            builder.line_to(tail_x + TAIL_WIDTH / 2.0, tail_base_y - 2.0);
            builder.line_to(tail_x - TAIL_WIDTH / 6.0, tail_tip_y);
            builder.close();
            let tail = builder.to_path();

            snapshot.push_rounded_clip(&rounded);
            snapshot.append_color(&balloon.background, &rect);
            snapshot.pop();
            snapshot.append_fill(&tail, gsk::FillRule::Winding, &balloon.background);

            // Outline the tail's two slanted edges, not its base.
            let edge = gsk::PathBuilder::new();
            edge.move_to(tail_x - TAIL_WIDTH / 2.0, tail_base_y - 2.0);
            edge.line_to(tail_x - TAIL_WIDTH / 6.0, tail_tip_y);
            edge.line_to(tail_x + TAIL_WIDTH / 2.0, tail_base_y - 2.0);
            snapshot.append_stroke(&edge.to_path(), &gsk::Stroke::new(BORDER_WIDTH), &balloon.border);

            snapshot.append_border(
                &rounded,
                &[BORDER_WIDTH; 4],
                &[balloon.border, balloon.border, balloon.border, balloon.border],
            );

            snapshot.save();
            snapshot.translate(&graphene::Point::new(
                box_x + BALLOON_PADDING,
                box_y + BALLOON_PADDING,
            ));
            snapshot.append_layout(&layout, &balloon.foreground);
            snapshot.restore();
        }
    }
}

glib::wrapper! {
    pub struct Stage(ObjectSubclass<imp::Stage>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Stage {
    fn default() -> Self {
        Self::new()
    }
}

impl Stage {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_texture(&self, texture: Option<gdk::Texture>) {
        let previous_size = self.imp().texture.borrow().as_ref().map(|t| (t.width(), t.height()));
        let new_size = texture.as_ref().map(|t| (t.width(), t.height()));
        self.imp().texture.replace(texture);
        if previous_size != new_size {
            self.queue_resize();
        }
        self.queue_draw();
    }

    /// Records where the artwork sits inside the texture, in texture pixels.
    pub fn set_content_bounds(&self, bounds: Option<(u32, u32, u32, u32)>) {
        self.imp().content.set(
            bounds.map(|(l, t, r, b)| (l as f32, t as f32, r as f32, b as f32)),
        );
    }

    pub fn set_balloon(&self, balloon: Option<Balloon>) {
        self.imp().balloon.replace(balloon);
        self.queue_draw();
    }

    pub fn set_backdrop(&self, backdrop: Backdrop) {
        self.imp().backdrop.set(backdrop);
        self.queue_draw();
    }

    pub fn backdrop(&self) -> Backdrop {
        self.imp().backdrop.get()
    }

    pub fn set_zoom(&self, zoom: f64) {
        self.imp().zoom.set(zoom.clamp(0.1, 8.0));
        self.queue_resize();
        self.queue_draw();
    }

    pub fn zoom(&self) -> f64 {
        self.imp().zoom.get()
    }

    pub fn set_fit(&self, fit: bool) {
        self.imp().fit.set(fit);
        self.queue_resize();
        self.queue_draw();
    }

    pub fn set_smooth(&self, smooth: bool) {
        self.imp().smooth.set(smooth);
        self.queue_draw();
    }

    pub fn smooth(&self) -> bool {
        self.imp().smooth.get()
    }
}
