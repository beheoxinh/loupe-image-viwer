// Copyright (c) 2026
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! A minimal painting canvas for the image editor.
//!
//! The canvas owns a Cairo image surface at the original image resolution.
//! Pointer input is translated into image coordinates, so strokes stay sharp
//! regardless of the zoom level used for displaying the image.

use std::cell::{Cell, RefCell};
use std::sync::LazyLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gettextrs::gettext;
use gtk::pango;

use crate::deps::*;

/// Tools available in the drawing canvas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DrawTool {
    #[default]
    Pencil,
    Brush,
    Eraser,
    Highlight,
    Line,
    Arrow,
    Rectangle,
    RoundedRectangle,
    Ellipse,
    Text,
    Bucket,
    Picker,
    Select,
    Move,
}

#[derive(Clone)]
struct FloatingSelection {
    surface: cairo::ImageSurface,
    start_pos: (f64, f64),
    current_pos: (f64, f64),
    width: f64,
    height: f64,
}

impl DrawTool {
    /// Tools that are dragged out into a shape instead of a freehand stroke.
    pub fn is_shape(self) -> bool {
        matches!(
            self,
            DrawTool::Line
                | DrawTool::Arrow
                | DrawTool::Rectangle
                | DrawTool::RoundedRectangle
                | DrawTool::Ellipse
                | DrawTool::Select
        )
    }
}

/// Image filters that can be applied to the whole canvas or to a selection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DrawFilter {
    /// Weighted average of the three color channels.
    Grayscale,
    /// Invert all color channels.
    Invert,
    /// Scale the distance from mid gray.
    Saturation(f64),
    /// Fraction added to every color channel.
    Brightness(f64),
    /// Scale every color channel.
    Contrast(f64),
    /// Horizontal and vertical pixel size.
    Pixelate(i32),
    /// Inverted, grayscale and blurred copy blended over the image.
    Sharpen(f64),
    /// Box blur radius in pixels.
    Blur(i32),
}

impl DrawFilter {
    pub fn label(self) -> String {
        match self {
            DrawFilter::Grayscale => gettext("Grayscale"),
            DrawFilter::Invert => gettext("Invert"),
            DrawFilter::Saturation(amount) => {
                format!("{} ({:+.0}%)", gettext("Saturation"), amount * 100.)
            }
            DrawFilter::Brightness(amount) => {
                format!("{} ({:+.0}%)", gettext("Brightness"), amount * 100.)
            }
            DrawFilter::Contrast(amount) => {
                format!("{} ({:+.0}%)", gettext("Contrast"), amount * 100.)
            }
            DrawFilter::Pixelate(block) => {
                format!("{} ({} px)", gettext("Pixelate"), block)
            }
            DrawFilter::Sharpen(amount) => {
                format!("{} ({:.1})", gettext("Sharpen"), amount)
            }
            DrawFilter::Blur(radius) => {
                format!("{} ({} px)", gettext("Blur"), radius)
            }
        }
    }
}

/// Upper bounds for the undo history.
const MAX_UNDO_STEPS: usize = 24;
const MAX_UNDO_BYTES: usize = 512 * 1024 * 1024;

/// Largest side cairo can back with an image surface. Anything bigger has to be
/// rejected before allocating, not after.
const MAX_CANVAS_SIDE: i32 = 32_767;

/// Why these dimensions cannot back a drawing canvas, if they cannot.
fn canvas_size_error(width: i32, height: i32) -> Option<String> {
    (width <= 0 || height <= 0 || width > MAX_CANVAS_SIDE || height > MAX_CANVAS_SIDE).then(|| {
        format!(
            "Image dimensions {width}x{height} are outside the {MAX_CANVAS_SIDE} px \
             per-side limit of the drawing canvas."
        )
    })
}

/// A full copy of the canvas is the safe fallback, but it costs one memcpy of
/// the whole image per stroke — 48 MB at 12 MP — which is what made drawing
/// feel laggy. When the touched region is known, only that rectangle is kept:
/// shapes, clipping and the filters know their bounds up front, and the
/// freehand tools record every segment they paint as the stroke goes on. Only
/// the tools whose reach cannot be predicted (bucket fill and text) still fall
/// back to a whole-surface copy.
#[derive(Clone, Copy)]
enum Dirty {
    /// Whole surface.
    All,
    /// Image coordinates as `(x0, y0, x1, y1)`, already clamped and ordered.
    Rect(f64, f64, f64, f64),
}

impl Dirty {
    /// Region covering a freehand segment, including the stroke's half width
    /// and a pixel of antialiasing.
    fn for_stroke(tool: DrawTool, line_width: f64, from: (f64, f64), to: (f64, f64)) -> Self {
        let margin = stroke_line_width(tool, line_width) / 2. + 2.;
        Dirty::Rect(
            from.0.min(to.0) - margin,
            from.1.min(to.1) - margin,
            from.0.max(to.0) + margin,
            from.1.max(to.1) + margin,
        )
    }
}

/// The pixels captured for one undo step. A freehand stroke paints a long,
/// thin trail, so storing its bounding box would copy far more than the stroke
/// actually touches — the trail is recorded as a list of segments instead.
enum UndoPixels {
    /// Whole surface, or one contiguous rectangle of it.
    Snapshot(UndoSnapshot),
    /// Pieces of the surface that were painted over during a single drag.
    Segments(Vec<UndoSnapshot>),
}

/// A rectangular piece of a surface, kept as a flat buffer of `width * 4`
/// bytes per row.
struct UndoSnapshot {
    x0: usize,
    y0: usize,
    width: usize,
    height: usize,
    /// `None` means the region held fully transparent pixels, so restoring it
    /// is a clear instead of a copy.
    data: Option<Vec<u8>>,
}

impl UndoSnapshot {
    /// Capture the rectangle `dirty` covers, in image pixels.
    fn capture(surface: &mut cairo::ImageSurface, dirty: Dirty) -> Self {
        let surface_width = surface.width() as usize;
        let surface_height = surface.height() as usize;
        let surface_stride = surface.stride() as usize;

        let (x0, y0, x1, y1) = match dirty {
            Dirty::All => (0, 0, surface_width, surface_height),
            Dirty::Rect(x0, y0, x1, y1) => {
                let x0 = (x0.max(0.) as usize).min(surface_width);
                let y0 = (y0.max(0.) as usize).min(surface_height);
                let x1 = (x1.max(0.) as usize).min(surface_width);
                let y1 = (y1.max(0.) as usize).min(surface_height);
                if x1 < x0 || y1 < y0 {
                    (x0, y0, x0, y0)
                } else {
                    (x0, y0, x1, y1)
                }
            }
        };

        let empty = UndoSnapshot {
            x0,
            y0,
            width: 0,
            height: 0,
            data: None,
        };

        let width = x1 - x0;
        let height = y1 - y0;
        if width == 0 || height == 0 {
            return empty;
        }

        let Ok(source) = surface.data() else {
            return empty;
        };

        // Cairo hands out premultiplied ARGB32, so an all-zero region is fully
        // transparent and does not need to be stored at all.
        let row_bytes = width * 4;
        let mut data = Vec::with_capacity(row_bytes * height);
        let mut is_transparent = true;

        for row in y0..y1 {
            let start = row * surface_stride + x0 * 4;
            let row_slice = &source[start..start + row_bytes];
            if is_transparent && row_slice.iter().any(|byte| *byte != 0) {
                is_transparent = false;
            }
            data.extend_from_slice(row_slice);
        }

        drop(source);

        UndoSnapshot {
            x0,
            y0,
            width,
            height,
            data: (!is_transparent).then_some(data),
        }
    }

    /// The rectangle this piece covers, as `(x0, y0, x1, y1)` in image pixels.
    fn rect(&self) -> (f64, f64, f64, f64) {
        (
            self.x0 as f64,
            self.y0 as f64,
            (self.x0 + self.width) as f64,
            (self.y0 + self.height) as f64,
        )
    }
}

impl UndoPixels {
    /// Put every captured piece back where it came from.
    ///
    /// Returns `false` when a piece does not fit the surface it is being
    /// restored into, so the caller can leave the canvas untouched instead of
    /// panicking on an out-of-bounds copy.
    fn restore_into(&self, surface: &mut cairo::ImageSurface) -> bool {
        match self {
            UndoPixels::Snapshot(snapshot) => restore_snapshot(surface, snapshot),
            UndoPixels::Segments(segments) => {
                // Back to front: a later segment was captured after the
                // earlier ones were painted, so it has to be laid down first
                // for overlapping pieces to end up in their original state.
                segments
                    .iter()
                    .rev()
                    .all(|snapshot| restore_snapshot(surface, snapshot))
            }
        }
    }

    /// Capture what `surface` holds right now over the same pixels this step
    /// covers, so the step can be reversed.
    fn capture_current(&self, surface: &mut cairo::ImageSurface) -> Self {
        let capture = |surface: &mut cairo::ImageSurface, snapshot: &UndoSnapshot| {
            let (x0, y0, x1, y1) = snapshot.rect();
            UndoSnapshot::capture(surface, Dirty::Rect(x0, y0, x1, y1))
        };

        match self {
            UndoPixels::Snapshot(snapshot) => UndoPixels::Snapshot(capture(surface, snapshot)),
            UndoPixels::Segments(segments) => UndoPixels::Segments(
                segments
                    .iter()
                    .map(|snapshot| capture(surface, snapshot))
                    .collect(),
            ),
        }
    }

    fn pixels(&self) -> u64 {
        let captured = |snapshot: &UndoSnapshot| {
            snapshot.data.as_ref().map_or(0, |data| data.len() as u64)
        };

        match self {
            UndoPixels::Snapshot(snapshot) => captured(snapshot),
            UndoPixels::Segments(segments) => segments.iter().map(captured).sum(),
        }
    }
}

fn restore_snapshot(surface: &mut cairo::ImageSurface, snapshot: &UndoSnapshot) -> bool {
    let surface_width = surface.width() as usize;
    let surface_height = surface.height() as usize;
    if snapshot.x0 + snapshot.width > surface_width || snapshot.y0 + snapshot.height > surface_height
    {
        return false;
    }

    let target_stride = surface.stride() as usize;
    let Ok(mut target) = surface.data() else {
        return false;
    };

    let Some(data) = snapshot.data.as_ref() else {
        for row in snapshot.y0..snapshot.y0 + snapshot.height {
            let start = row * target_stride + snapshot.x0 * 4;
            target[start..start + snapshot.width * 4].fill(0);
        }
        return true;
    };

    let row_bytes = snapshot.width * 4;
    for row in 0..snapshot.height {
        let source = row * row_bytes;
        let target_offset = (snapshot.y0 + row) * target_stride + snapshot.x0 * 4;
        target[target_offset..target_offset + row_bytes]
            .copy_from_slice(&data[source..source + row_bytes]);
    }

    true
}

#[derive(Clone, Copy)]
struct Preview {
    tool: DrawTool,
    start_widget: (f64, f64),
    start_image: (f64, f64),
    current_image: (f64, f64),
}

#[derive(Clone, Copy)]
struct Stroke {
    tool: DrawTool,
    start_widget: (f64, f64),
    last_image: (f64, f64),
    moved: bool,
}

mod imp {
    use super::*;

    pub struct LpDrawCanvas {
        pub(super) surface: RefCell<Option<cairo::ImageSurface>>,
        pub(super) undo_stack: RefCell<Vec<UndoPixels>>,
        pub(super) redo_stack: RefCell<Vec<UndoPixels>>,
        pub(super) preview: RefCell<Option<Preview>>,
        pub(super) stroke: RefCell<Option<Stroke>>,
        pub(super) tool: Cell<DrawTool>,
        pub(super) color: RefCell<gdk::RGBA>,
        pub(super) line_width: Cell<f64>,
        pub(super) fill_shape: Cell<bool>,
        pub(super) changed: Cell<bool>,
        /// Active selection in image coordinates: `(x0, y0, x1, y1)`.
        pub(super) selection: RefCell<Option<(f64, f64, f64, f64)>>,
        /// Set when a drag committed an edit, so the click that GTK delivers
        /// right after the drag ends does not repeat it (an extra dot for the
        /// freehand tools, or clearing the selection for the select tool).
        pub(super) suppress_click: Cell<bool>,
        /// Pixels touched by the freehand stroke in progress, captured one
        /// segment at a time. They become a single undo step on drag end.
        pub(super) stroke_undo: RefCell<Vec<UndoSnapshot>>,
        /// Floating selection lifted from the canvas for moving.
        pub(super) floating_selection: RefCell<Option<FloatingSelection>>,
        /// Zoom multiplier (1.0 = best fit, up to 30.0).
        pub(super) zoom: Cell<f64>,
        /// Pan offset in display pixels.
        pub(super) pan: Cell<(f64, f64)>,
        /// Pan drag initial offset.
        pub(super) pan_drag_start: Cell<(f64, f64)>,
        pub(super) is_panning: Cell<bool>,
    }

    impl Default for LpDrawCanvas {
        fn default() -> Self {
            Self {
                surface: RefCell::new(None),
                undo_stack: RefCell::new(Vec::new()),
                redo_stack: RefCell::new(Vec::new()),
                preview: RefCell::new(None),
                stroke: RefCell::new(None),
                tool: Cell::new(DrawTool::default()),
                color: RefCell::new(gdk::RGBA::new(1., 0., 0., 1.)),
                line_width: Cell::new(4.),
                fill_shape: Cell::new(false),
                changed: Cell::new(false),
                selection: RefCell::new(None),
                suppress_click: Cell::new(false),
                stroke_undo: RefCell::new(Vec::new()),
                floating_selection: RefCell::new(None),
                zoom: Cell::new(1.0),
                pan: Cell::new((0.0, 0.0)),
                pan_drag_start: Cell::new((0.0, 0.0)),
                is_panning: Cell::new(false),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LpDrawCanvas {
        const NAME: &'static str = "LpDrawCanvas";
        type Type = super::LpDrawCanvas;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("lpdrawcanvas");
        }
    }

    impl ObjectImpl for LpDrawCanvas {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();

            let click = gtk::GestureClick::new();
            click.set_button(gdk::BUTTON_PRIMARY);
            // A new press starts a new sequence, so a suppression flag left
            // over from a drag that never produced a trailing click cannot
            // swallow this one.
            click.connect_pressed(glib::clone!(
                #[weak(rename_to = canvas)]
                obj,
                move |_, _, _, _| canvas.imp().suppress_click.set(false)
            ));
            click.connect_released(glib::clone!(
                #[weak(rename_to = canvas)]
                obj,
                move |_, _n, x, y| canvas.imp().handle_click(x, y)
            ));
            obj.add_controller(click);

            let drag = gtk::GestureDrag::new();
            drag.set_button(gdk::BUTTON_PRIMARY);
            drag.connect_drag_begin(glib::clone!(
                #[weak(rename_to = canvas)]
                obj,
                move |_, x, y| canvas.imp().handle_drag_begin(x, y)
            ));
            drag.connect_drag_update(glib::clone!(
                #[weak(rename_to = canvas)]
                obj,
                move |_, dx, dy| canvas.imp().handle_drag_update(dx, dy)
            ));
            drag.connect_drag_end(glib::clone!(
                #[weak(rename_to = canvas)]
                obj,
                move |_, dx, dy| canvas.imp().handle_drag_end(dx, dy)
            ));
            obj.add_controller(drag);

            let middle_drag = gtk::GestureDrag::new();
            middle_drag.set_button(gdk::BUTTON_MIDDLE);
            middle_drag.connect_drag_begin(glib::clone!(
                #[weak(rename_to = canvas)]
                obj,
                move |_, _, _| {
                    let imp = canvas.imp();
                    imp.pan_drag_start.set(imp.pan.get());
                    imp.is_panning.set(true);
                }
            ));
            middle_drag.connect_drag_update(glib::clone!(
                #[weak(rename_to = canvas)]
                obj,
                move |_, dx, dy| {
                    let imp = canvas.imp();
                    let start = imp.pan_drag_start.get();
                    imp.pan.set((start.0 + dx, start.1 + dy));
                    canvas.queue_draw();
                }
            ));
            middle_drag.connect_drag_end(glib::clone!(
                #[weak(rename_to = canvas)]
                obj,
                move |_, _, _| canvas.imp().is_panning.set(false)
            ));
            obj.add_controller(middle_drag);

            let scroll = gtk::EventControllerScroll::new(
                gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::HORIZONTAL,
            );
            scroll.connect_scroll(glib::clone!(
                #[weak(rename_to = canvas)]
                obj,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |controller, dx, dy| {
                    let modifiers = controller.current_event_state();
                    if modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
                        let center_x = canvas.width() as f64 / 2.;
                        let center_y = canvas.height() as f64 / 2.;
                        let factor = if dy < 0. { 1.15 } else if dy > 0. { 1. / 1.15 } else { 1. };
                        let current = canvas.imp().zoom.get();
                        canvas.imp().set_zoom_at(current * factor, center_x, center_y);
                        glib::Propagation::Stop
                    } else {
                        let cur_pan = canvas.imp().pan.get();
                        canvas.imp().pan.set((cur_pan.0 - dx * 30., cur_pan.1 - dy * 30.));
                        canvas.queue_draw();
                        glib::Propagation::Stop
                    }
                }
            ));
            obj.add_controller(scroll);
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
                vec![
                    glib::subclass::Signal::builder("changed").build(),
                    glib::subclass::Signal::builder("color-picked")
                        .param_types([gdk::RGBA::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("selection-changed").build(),
                    glib::subclass::Signal::builder("zoom-changed")
                        .param_types([f64::static_type()])
                        .build(),
                ]
            });
            &SIGNALS
        }
    }

    impl WidgetImpl for LpDrawCanvas {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let widget_width = obj.width() as f64;
            let widget_height = obj.height() as f64;
            let bounds = graphene::Rect::new(0., 0., widget_width as f32, widget_height as f32);

            let cr = snapshot.append_cairo(&bounds);
            draw_checkerboard(&cr, widget_width, widget_height);

            let surface = self.surface.borrow();
            let Some(surface) = surface.as_ref() else {
                return;
            };

            let image_width = surface.width() as f64;
            let image_height = surface.height() as f64;
            if image_width < 1. || image_height < 1. {
                return;
            }

            let Some((scale, offset_x, offset_y)) = self.fit() else {
                return;
            };

            let _ = cr.save();
            cr.translate(offset_x, offset_y);
            cr.scale(scale, scale);

            // `set_source_surface` uses the default GOOD filter, which is a
            // costly resample of the whole image on every pointer sample.
            // Bilinear is visually identical at these zoom levels and keeps
            // drawing responsive.
            let pattern = cairo::SurfacePattern::create(surface);
            pattern.set_filter(cairo::Filter::Bilinear);
            let _ = cr.set_source(&pattern);
            let _ = cr.paint();

            if let Some(ref float) = *self.floating_selection.borrow() {
                let _ = cr.save();
                let _ = cr.set_source_surface(&float.surface, float.current_pos.0, float.current_pos.1);
                let _ = cr.paint();
                let _ = cr.restore();
            }

            if let Some(preview) = *self.preview.borrow() {
                if preview.tool == DrawTool::Select {
                    draw_selection(&cr, &preview.start_image, &preview.current_image);
                } else {
                    draw_preview(&cr, &preview, &self.color.borrow(), self.line_width.get());
                }
            }

            if let Some((x0, y0, x1, y1)) = *self.selection.borrow() {
                draw_selection(&cr, &(x0, y0), &(x1, y1));
            }

            let _ = cr.restore();
        }

        fn realize(&self) {
            self.parent_realize();
            self.obj().set_cursor_from_name(Some("crosshair"));
        }
    }
}

glib::wrapper! {
    pub struct LpDrawCanvas(ObjectSubclass<imp::LpDrawCanvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl LpDrawCanvas {
    /// Load an image at its original resolution into the canvas.
    pub async fn load(&self, file: gio::File) -> Result<(), glycin::Error> {
        let mut loader = glycin::Loader::new(file);
        loader.accepted_memory_formats(glycin::MemoryFormatSelection::B8g8r8a8Premultiplied);

        let mut image = loader.load().await?;
        let frame = image.next_frame().await?;

        let width = frame.width() as i32;
        let height = frame.height() as i32;
        let source_stride = frame.stride() as usize;
        let source = frame.buf_slice();

        // A cairo image surface has a hard 32767 px side limit. Refuse
        // anything that would either fail to allocate or leave the canvas
        // unusable, instead of aborting the process from inside a template
        // callback.
        if let Some(message) = canvas_size_error(width, height) {
            return Err(glycin::Error::other(&message));
        }

        let row_bytes = width as usize * 4;
        if source.len() < source_stride * height as usize {
            return Err(glycin::Error::other(
                "The decoded image buffer was shorter than its own dimensions claim.",
            ));
        }

        // The surface is ARgb32 and the loader was asked for premultiplied
        // B8g8r8a8, so the rows are byte-for-byte compatible.
        let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)
            .map_err(|err| {
                glycin::Error::other(&format!(
                    "Failed to allocate a {width}x{height} drawing surface: {err}"
                ))
            })?;
        let stride = surface.stride() as usize;

        if let Ok(mut data) = surface.data() {
            for row in 0..height as usize {
                let source_row = &source[row * source_stride..row * source_stride + row_bytes];
                let target_row = &mut data[row * stride..row * stride + row_bytes];
                target_row.copy_from_slice(source_row);
            }
        }

        self.imp().surface.replace(Some(surface));
        self.imp().undo_stack.borrow_mut().clear();
        self.imp().redo_stack.borrow_mut().clear();
        self.imp().selection.borrow_mut().take();
        self.imp().changed.set(false);
        self.queue_draw();
        self.emit_by_name::<()>("changed", &[]);
        self.emit_by_name::<()>("selection-changed", &[]);

        Ok(())
    }

    pub fn has_image(&self) -> bool {
        self.imp().surface.borrow().is_some()
    }

    pub fn set_tool(&self, tool: DrawTool) {
        if tool != DrawTool::Move {
            self.commit_floating_selection();
        }
        self.imp().tool.set(tool);
        let cursor = match tool {
            DrawTool::Text => "text",
            DrawTool::Picker => "cell",
            DrawTool::Move => "move",
            _ => "crosshair",
        };
        self.set_cursor_from_name(Some(cursor));
        if tool != DrawTool::Select && tool != DrawTool::Move {
            self.clear_selection();
        }
    }

    /// Bake any floating selection into the main image surface.
    pub fn commit_floating_selection(&self) {
        if let Some(float) = self.imp().floating_selection.borrow_mut().take() {
            let mut slot = self.imp().surface.borrow_mut();
            if let Some(surface) = slot.as_mut() {
                if let Ok(cr) = cairo::Context::new(surface) {
                    let _ = cr.set_source_surface(&float.surface, float.current_pos.0, float.current_pos.1);
                    let _ = cr.paint();
                }
            }
            drop(slot);
            self.imp().mark_changed();
            self.queue_draw();
        }
    }

    pub fn tool(&self) -> DrawTool {
        self.imp().tool.get()
    }

    pub fn set_color(&self, color: gdk::RGBA) {
        self.imp().color.replace(color);
    }

    pub fn color(&self) -> gdk::RGBA {
        *self.imp().color.borrow()
    }

    pub fn set_line_width(&self, width: f64) {
        self.imp().line_width.set(width.clamp(1., 200.));
    }

    pub fn line_width(&self) -> f64 {
        self.imp().line_width.get()
    }

    pub fn set_fill_shape(&self, fill: bool) {
        self.imp().fill_shape.set(fill);
    }

    pub fn fill_shape(&self) -> bool {
        self.imp().fill_shape.get()
    }

    pub fn has_changes(&self) -> bool {
        self.imp().changed.get()
    }

    pub fn mark_saved(&self) {
        self.imp().changed.set(false);
    }

    pub fn can_undo(&self) -> bool {
        !self.imp().undo_stack.borrow().is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.imp().redo_stack.borrow().is_empty()
    }

    pub fn zoom_level(&self) -> f64 {
        self.imp().zoom.get()
    }

    pub fn zoom_in(&self) {
        let w = self.width() as f64 / 2.;
        let h = self.height() as f64 / 2.;
        let cur = self.imp().zoom.get();
        self.imp().set_zoom_at(cur * 1.25, w, h);
    }

    pub fn zoom_out(&self) {
        let w = self.width() as f64 / 2.;
        let h = self.height() as f64 / 2.;
        let cur = self.imp().zoom.get();
        self.imp().set_zoom_at(cur / 1.25, w, h);
    }

    pub fn zoom_reset(&self) {
        self.imp().zoom_reset();
    }

    pub fn undo(&self) {
        let imp = self.imp();
        let mut surface_slot = imp.surface.borrow_mut();
        let Some(surface) = surface_slot.as_mut() else {
            return;
        };

        let Some(previous) = imp.undo_stack.borrow_mut().pop() else {
            return;
        };

        // The step that is being undone is exactly what a redo has to restore.
        let current = previous.capture_current(surface);
        if !previous.restore_into(surface) {
            // The image was replaced under the history; drop the stale entry
            // instead of leaving the canvas half-restored.
            return;
        }
        imp.redo_stack.borrow_mut().push(current);

        imp.changed.set(true);
        drop(surface_slot);
        self.queue_draw();
        self.emit_by_name::<()>("changed", &[]);
    }

    pub fn redo(&self) {
        let imp = self.imp();
        let mut surface_slot = imp.surface.borrow_mut();
        let Some(surface) = surface_slot.as_mut() else {
            return;
        };

        let Some(next) = imp.redo_stack.borrow_mut().pop() else {
            return;
        };

        let current = next.capture_current(surface);
        if !next.restore_into(surface) {
            return;
        }
        imp.undo_stack.borrow_mut().push(current);

        imp.changed.set(true);
        drop(surface_slot);
        self.queue_draw();
        self.emit_by_name::<()>("changed", &[]);
    }

    /// Remove everything from the canvas.
    pub fn clear(&self) {
        self.imp().push_undo_all();

        let imp = self.imp();
        let mut slot = imp.surface.borrow_mut();
        let Some(surface) = slot.as_mut() else {
            return;
        };
        let Ok(cr) = cairo::Context::new(surface) else {
            return;
        };
        let _ = cr.set_operator(cairo::Operator::Clear);
        let _ = cr.paint();
        drop(slot);

        imp.mark_changed();
    }

    /// Return a copy of the current pixels, its dimensions and stride.
    pub fn surface_snapshot(&self) -> Option<(Vec<u8>, i32, i32, i32)> {
        self.commit_floating_selection();
        let mut slot = self.imp().surface.borrow_mut();
        let surface = slot.as_mut()?;
        surface.flush();
        let width = surface.width();
        let height = surface.height();
        let stride = surface.stride();
        let data = surface.data().ok()?.to_vec();

        Some((data, width, height, stride))
    }

    /// Current selection in image coordinates as `(x0, y0, x1, y1)`.
    pub fn selection(&self) -> Option<(f64, f64, f64, f64)> {
        *self.imp().selection.borrow()
    }

    pub fn has_selection(&self) -> bool {
        self.imp().selection.borrow().is_some()
    }

    /// Drop the current selection without touching the image.
    pub fn clear_selection(&self) {
        self.commit_floating_selection();
        if self.imp().selection.borrow_mut().take().is_some() {
            self.queue_draw();
            self.emit_by_name::<()>("selection-changed", &[]);
        }
    }

    /// Select the whole image.
    pub fn select_all(&self) {
        self.commit_floating_selection();
        let Some((width, height)) = self.imp().image_size() else {
            return;
        };

        self.imp().selection.replace(Some((0., 0., width, height)));
        self.queue_draw();
        self.emit_by_name::<()>("selection-changed", &[]);
    }

    /// Fill the selection with a color, or select the whole image when nothing
    /// is selected so a second invocation still does something useful.
    pub fn fill_selection(&self, color: gdk::RGBA) -> bool {
        let Some(selection) = self.selection() else {
            self.select_all();
            return false;
        };

        self.apply_selection(
            selection,
            |cr| {
                cr.set_source_rgba(
                    color.red() as f64,
                    color.green() as f64,
                    color.blue() as f64,
                    color.alpha() as f64,
                );
                let _ = cr.set_operator(cairo::Operator::Source);
                let _ = cr.paint();
            },
        );
        true
    }

    /// Delete the selected pixels. Returns `false` when there is no selection,
    /// so the Delete key stays a no-op instead of wiping the whole image.
    pub fn delete_selection(&self) -> bool {
        let Some(selection) = self.selection() else {
            return false;
        };

        self.apply_selection(
            selection,
            |cr| {
                let _ = cr.set_operator(cairo::Operator::Clear);
                let _ = cr.paint();
            },
        );
        true
    }

    /// Cut the selection and return it as PNG data for the clipboard.
    pub fn cut_selection(&self) -> Option<Vec<u8>> {
        let selection = self.selection()?;
        let cropped = self.cropped_png(selection)?;
        self.delete_selection();
        Some(cropped)
    }

    /// Copy the selection, or the whole image when nothing is selected.
    pub fn copy_selection(&self) -> Option<Vec<u8>> {
        self.commit_floating_selection();
        let selection = self.selection().or_else(|| {
            let (width, height) = self.imp().image_size()?;
            Some((0., 0., width, height))
        })?;

        self.cropped_png(selection)
    }

    /// Paste PNG data into the selection, or at the top left corner.
    ///
    /// The pasted image is scaled down when it does not fit into the
    /// destination, so a large clipboard image is never silently clipped.
    pub fn paste_png(&self, data: &[u8]) -> Result<(), glib::Error> {
        let pixbuf = gdk_pixbuf::Pixbuf::from_stream(
            &gio::MemoryInputStream::from_bytes(&glib::Bytes::from(data)),
            gio::Cancellable::NONE,
        )?;

        if pixbuf.width() < 1 || pixbuf.height() < 1 {
            return Ok(());
        }

        let Some((canvas_width, canvas_height)) = self.imp().image_size() else {
            return Ok(());
        };

        let (origin_x, origin_y, slot_width, slot_height) = match self.selection() {
            Some((x0, y0, x1, y1)) => {
                let (x0, y0, x1, y1) = normalized_selection((x0, y0, x1, y1));
                (x0, y0, x1 - x0, y1 - y0)
            }
            None => (0., 0., canvas_width, canvas_height),
        };

        // The paste must land inside the canvas, and inside the selection when
        // there is one.
        let slot_width = slot_width.min(canvas_width - origin_x).max(1.);
        let slot_height = slot_height.min(canvas_height - origin_y).max(1.);
        let scale = (slot_width / pixbuf.width() as f64)
            .min(slot_height / pixbuf.height() as f64)
            .min(1.);

        let width = ((pixbuf.width() as f64 * scale).round() as i32).clamp(1, slot_width as i32);
        let height = ((pixbuf.height() as f64 * scale).round() as i32).clamp(1, slot_height as i32);

        // `ok()` rather than `unwrap()`: scaling a huge clipboard image down is
        // the one step here that can fail, and returning early leaves the
        // canvas untouched.
        let Some(pixbuf) = pixbuf.scale_simple(width, height, gdk_pixbuf::InterpType::Bilinear)
        else {
            return Ok(());
        };

        let source = pixbuf.read_pixel_bytes();
        let source = source.as_ref();
        let source_stride = pixbuf.rowstride() as usize;
        let target_x = origin_x.round() as usize;
        let target_y = origin_y.round() as usize;

        {
            let mut slot = self.imp().surface.borrow_mut();
            let Some(surface) = slot.as_mut() else {
                return Ok(());
            };
            let stride = surface.stride() as usize;
            let (surface_width, surface_height) =
                (surface.width() as usize, surface.height() as usize);
            let Ok(mut canvas) = surface.data() else {
                return Ok(());
            };

            for row in 0..height as usize {
                let row_y = target_y + row;
                if row_y >= surface_height {
                    break;
                }
                for column in 0..width as usize {
                    let column_x = target_x + column;
                    if column_x >= surface_width {
                        break;
                    }

                    let source_offset = row * source_stride + column * 4;
                    let target_offset = row_y * stride + column_x * 4;
                    if source_offset + 4 > source.len() || target_offset + 4 > canvas.len() {
                        continue;
                    }

                    // gdk-pixbuf hands out RGBA while Cairo wants premultiplied BGRA
                    let red = source[source_offset] as u32;
                    let green = source[source_offset + 1] as u32;
                    let blue = source[source_offset + 2] as u32;
                    let alpha = source[source_offset + 3] as u32;
                    let premultiply = |channel: u32| ((channel * alpha + 127) / 255) as u8;

                    canvas[target_offset] = premultiply(blue);
                    canvas[target_offset + 1] = premultiply(green);
                    canvas[target_offset + 2] = premultiply(red);
                    canvas[target_offset + 3] = alpha as u8;
                }
            }
        }

        // Only the pasted rectangle needs to be remembered for undo.
        self.imp().push_undo(Dirty::Rect(
            target_x as f64,
            target_y as f64,
            (target_x + width as usize) as f64,
            (target_y + height as usize) as f64,
        ));
        self.clear_selection();
        self.imp().mark_changed();

        Ok(())
    }

    /// Filters that can be applied to the current selection or the whole image.
    pub fn available_filters(&self) -> Vec<DrawFilter> {
        vec![
            DrawFilter::Grayscale,
            DrawFilter::Invert,
            DrawFilter::Saturation(0.5),
            DrawFilter::Saturation(-1.),
            DrawFilter::Saturation(1.),
            DrawFilter::Brightness(0.15),
            DrawFilter::Brightness(-0.15),
            DrawFilter::Contrast(0.5),
            DrawFilter::Contrast(-0.5),
            DrawFilter::Pixelate(8),
            DrawFilter::Pixelate(16),
            DrawFilter::Sharpen(0.5),
            DrawFilter::Sharpen(1.),
            DrawFilter::Blur(2),
            DrawFilter::Blur(6),
            DrawFilter::Blur(12),
        ]
    }

    /// Apply a filter to the selection, or to the whole image when nothing is
    /// selected.
    pub fn apply_filter(&self, filter: DrawFilter) -> bool {
        let Some(selection) = self.selection().or_else(|| {
            let (width, height) = self.imp().image_size()?;
            Some((0., 0., width, height))
        }) else {
            return false;
        };

        // `filter_region` takes the undo snapshot and emits `changed` itself.
        self.imp().filter_region(filter, selection);
        true
    }

    /// Apply a filter without blocking the main thread.
    ///
    /// The region the filter touches is copied out on the main thread, the
    /// filter runs on a worker thread, and the result is written back once it
    /// is ready. Blur and sharpen are several orders of magnitude slower than
    /// the point filters, which is enough to freeze the window for seconds on
    /// a large photo, so those always take this path.
    ///
    /// `on_done` is called on the main thread with `true` when the canvas was
    /// modified.
    pub fn apply_filter_async<F>(&self, filter: DrawFilter, on_done: F)
    where
        F: Fn(bool) + 'static,
    {
        let Some(selection) = self.selection().or_else(|| {
            let (width, height) = self.imp().image_size()?;
            Some((0., 0., width, height))
        }) else {
            on_done(false);
            return;
        };

        let Some(region) = self.imp().read_region(selection) else {
            on_done(false);
            return;
        };

        let (send, receive) = async_channel::bounded(1);
        let spawn_result = std::thread::Builder::new()
            .name("filter".into())
            .spawn(move || {
                let mut region = region;
                filter_pixels(filter, &mut region);
                let _ = send.send_blocking(region);
            });

        if spawn_result.is_err() {
            // No worker thread available: filter in place rather than failing
            // the operation outright.
            on_done(self.apply_filter(filter));
            return;
        }

        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            async move {
                let Ok(region) = receive.recv().await else {
                    on_done(false);
                    return;
                };
                let applied = canvas.imp().write_region(region);
                on_done(applied);
            }
        ));
    }

    /// Run `paint` with a Cairo context clipped to `selection` in image
    /// coordinates. Undo is pushed once and the "changed" signal emitted.
    fn apply_selection<F>(&self, selection: (f64, f64, f64, f64), paint: F)
    where
        F: FnOnce(&cairo::Context),
    {
        let (x0, y0, x1, y1) = normalized_selection(selection);
        self.imp().push_undo(dirty_rect(selection));

        let mut slot = self.imp().surface.borrow_mut();
        let Some(surface) = slot.as_mut() else {
            return;
        };
        let Ok(cr) = cairo::Context::new(surface) else {
            return;
        };

        cr.rectangle(x0, y0, x1 - x0, y1 - y0);
        let _ = cr.clip();
        paint(&cr);

        drop(slot);
        self.imp().mark_changed();
    }

    /// Encode `selection` as PNG data, cropping it out of the surface.
    fn cropped_png(&self, selection: (f64, f64, f64, f64)) -> Option<Vec<u8>> {
        let (x0, y0, x1, y1) = normalized_selection(selection);
        let width = (x1 - x0).round().max(1.) as i32;
        let height = (y1 - y0).round().max(1.) as i32;

        let mut cropped = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height).ok()?;
        let cr = cairo::Context::new(&cropped).ok()?;

        if let Some(surface) = self.imp().surface.borrow().as_ref() {
            surface.flush();
            let _ = cr.set_source_surface(surface, -x0, -y0);
            let _ = cr.paint();
        }

        if let Some(ref float) = *self.imp().floating_selection.borrow() {
            float.surface.flush();
            let _ = cr.set_source_surface(&float.surface, float.current_pos.0 - x0, float.current_pos.1 - y0);
            let _ = cr.paint();
        }

        drop(cr);
        cropped.flush();

        let stride = cropped.stride() as usize;
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        if let Ok(source) = cropped.data() {
            for row in 0..height as usize {
                for col in 0..width as usize {
                    let src_idx = row * stride + col * 4;
                    let dst_idx = (row * width as usize + col) * 4;
                    let blue = source[src_idx] as u32;
                    let green = source[src_idx + 1] as u32;
                    let red = source[src_idx + 2] as u32;
                    let alpha = source[src_idx + 3] as u32;

                    let unpremultiply = |channel: u32| {
                        if alpha == 0 {
                            0u8
                        } else {
                            ((channel * 255 + alpha / 2) / alpha).min(255) as u8
                        }
                    };

                    pixels[dst_idx] = unpremultiply(red);
                    pixels[dst_idx + 1] = unpremultiply(green);
                    pixels[dst_idx + 2] = unpremultiply(blue);
                    pixels[dst_idx + 3] = alpha as u8;
                }
            }
        }
        let bytes = glib::Bytes::from_owned(pixels);
        let pixbuf = gdk_pixbuf::Pixbuf::from_bytes(
            &bytes,
            gdk_pixbuf::Colorspace::Rgb,
            true,
            8,
            width,
            height,
            width * 4,
        );
        pixbuf.save_to_bufferv("png", &[]).ok()
    }
}

impl imp::LpDrawCanvas {
    /// Apply a per pixel filter to a rectangle of the surface, in place.
    ///
    /// Only used as the fallback when no worker thread can be spawned; the UI
    /// normally goes through `apply_filter_async`.
    fn filter_region(&self, filter: DrawFilter, selection: (f64, f64, f64, f64)) {
        if let Some(mut region) = self.read_region(selection) {
            filter_pixels(filter, &mut region);
            self.write_region(region);
        }
    }

    /// Copy the pixels a filter is about to touch into an owned rectangle.
    ///
    /// The rectangle covers the selection plus `margin` pixels on every side,
    /// so blur and sharpen can still sample the real neighbours just outside
    /// the selection.
    fn read_region(&self, selection: (f64, f64, f64, f64)) -> Option<ImageRegion> {
        let mut slot = self.surface.borrow_mut();
        let Some(surface) = slot.as_mut() else {
            return None;
        };
        let stride = surface.stride() as usize;
        let (surface_width, surface_height) = (surface.width(), surface.height());
        let (x0, y0, x1, y1) = normalized_selection(selection);
        let margin = FILTER_MARGIN;
        let left = (x0.round() as i64 - margin).clamp(0, surface_width as i64) as usize;
        let top = (y0.round() as i64 - margin).clamp(0, surface_height as i64) as usize;
        let right = (x1.round() as i64 + margin).clamp(0, surface_width as i64) as usize;
        let bottom = (y1.round() as i64 + margin).clamp(0, surface_height as i64) as usize;

        if right <= left || bottom <= top {
            return None;
        }

        let source = surface.data().ok()?;
        let width = right - left;
        let height = bottom - top;
        let mut data = vec![0u8; width * height * 4];
        for row in 0..height {
            let from = (top + row) * stride + left * 4;
            let to = row * width * 4;
            data[to..to + width * 4].copy_from_slice(&source[from..from + width * 4]);
        }

        // The part of the copy the filter is actually allowed to modify,
        // relative to the copied rectangle.
        let inner_left = (x0.round().max(0.) as usize).saturating_sub(left).min(width);
        let inner_top = (y0.round().max(0.) as usize).saturating_sub(top).min(height);
        let inner_right = (x1.round().max(0.) as usize).saturating_sub(left).min(width);
        let inner_bottom = (y1.round().max(0.) as usize).saturating_sub(top).min(height);

        Some(ImageRegion {
            left,
            top,
            width,
            height,
            inner: (inner_left, inner_top, inner_right, inner_bottom),
            data,
        })
    }

    /// Write a processed rectangle back into the surface as one undo step.
    fn write_region(&self, region: ImageRegion) -> bool {
        let ImageRegion { left, top, width, data, inner, .. } = region;
        let (inner_left, inner_top, inner_right, inner_bottom) = inner;
        if inner_right <= inner_left || inner_bottom <= inner_top {
            return false;
        }

        // The undo snapshot has to be taken before the write, otherwise it
        // would record the filtered pixels and undo would do nothing.
        self.push_undo(Dirty::Rect(
            (left + inner_left) as f64,
            (top + inner_top) as f64,
            (left + inner_right) as f64,
            (top + inner_bottom) as f64,
        ));

        {
            let mut slot = self.surface.borrow_mut();
            let Some(surface) = slot.as_mut() else {
                return false;
            };
            let stride = surface.stride() as usize;
            let Ok(mut target) = surface.data() else {
                return false;
            };

            // The surface may have been replaced while the worker was running,
            // so every row is bounds-checked instead of indexed blindly.
            for row in inner_top..inner_bottom {
                let from = row * width * 4 + inner_left * 4;
                let to = (top + row) * stride + (left + inner_left) * 4;
                let len = (inner_right - inner_left) * 4;
                if to + len > target.len() || from + len > data.len() {
                    continue;
                }
                target[to..to + len].copy_from_slice(&data[from..from + len]);
            }
        }

        self.mark_changed();
        true
    }

    fn image_size(&self) -> Option<(f64, f64)> {
        self.surface
            .borrow()
            .as_ref()
            .map(|surface| (surface.width() as f64, surface.height() as f64))
    }

    fn fit(&self) -> Option<(f64, f64, f64)> {
        let (image_width, image_height) = self.image_size()?;
        let widget_width = self.obj().width() as f64;
        let widget_height = self.obj().height() as f64;

        if widget_width < 1. || widget_height < 1. {
            return None;
        }

        let base_scale = (widget_width / image_width).min(widget_height / image_height);
        let zoom = self.zoom.get().clamp(0.05, 50.0);
        let scale = base_scale * zoom;
        let pan = self.pan.get();
        let offset_x = (widget_width - image_width * scale) / 2. + pan.0;
        let offset_y = (widget_height - image_height * scale) / 2. + pan.1;

        Some((scale, offset_x, offset_y))
    }

    pub(super) fn set_zoom_at(&self, new_zoom: f64, anchor_x: f64, anchor_y: f64) {
        let (image_width, image_height) = match self.image_size() {
            Some(size) => size,
            None => return,
        };
        let widget_width = self.obj().width() as f64;
        let widget_height = self.obj().height() as f64;
        if widget_width < 1. || widget_height < 1. {
            return;
        }

        let old_zoom = self.zoom.get();
        let base_scale = (widget_width / image_width).min(widget_height / image_height);
        let old_scale = base_scale * old_zoom;
        let old_pan = self.pan.get();
        let old_ox = (widget_width - image_width * old_scale) / 2. + old_pan.0;
        let old_oy = (widget_height - image_height * old_scale) / 2. + old_pan.1;

        let ix = (anchor_x - old_ox) / old_scale;
        let iy = (anchor_y - old_oy) / old_scale;

        let clamped = new_zoom.clamp(0.05, 50.0);
        let new_scale = base_scale * clamped;

        let new_pan_x = anchor_x - (widget_width - image_width * new_scale) / 2. - ix * new_scale;
        let new_pan_y = anchor_y - (widget_height - image_height * new_scale) / 2. - iy * new_scale;

        self.zoom.set(clamped);
        self.pan.set((new_pan_x, new_pan_y));
        self.obj().queue_draw();
        self.obj().emit_by_name::<()>("zoom-changed", &[&clamped]);
    }

    pub(super) fn zoom_reset(&self) {
        self.zoom.set(1.0);
        self.pan.set((0.0, 0.0));
        self.obj().queue_draw();
        self.obj().emit_by_name::<()>("zoom-changed", &[&1.0f64]);
    }

    fn widget_to_image(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (scale, offset_x, offset_y) = self.fit()?;
        let (image_width, image_height) = self.image_size()?;
        let x = (x - offset_x) / scale;
        let y = (y - offset_y) / scale;

        if x < 0. || y < 0. || x > image_width || y > image_height {
            return None;
        }

        Some((x, y))
    }

    fn clamped_image_point(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (scale, offset_x, offset_y) = self.fit()?;
        let (image_width, image_height) = self.image_size()?;
        let x = ((x - offset_x) / scale).clamp(0., image_width);
        let y = ((y - offset_y) / scale).clamp(0., image_height);

        Some((x, y))
    }

    /// Remember the pixels the next edit is about to touch.
    ///
    /// `dirty` must cover every pixel the edit can change; passing a region
    /// that is too small leaves stale pixels behind after an undo.
    fn push_undo(&self, dirty: Dirty) {
        let mut slot = self.surface.borrow_mut();
        let Some(surface) = slot.as_mut() else {
            return;
        };

        let snapshot = UndoSnapshot::capture(surface, dirty);
        drop(slot);

        self.push_undo_pixels(UndoPixels::Snapshot(snapshot));
    }

    /// Push an undo step covering the whole surface.
    fn push_undo_all(&self) {
        self.push_undo(Dirty::All);
    }

    fn push_undo_pixels(&self, pixels: UndoPixels) {
        let mut undo = self.undo_stack.borrow_mut();
        undo.push(pixels);

        while undo.len() > MAX_UNDO_STEPS
            || undo.iter().map(UndoPixels::pixels).sum::<u64>() > MAX_UNDO_BYTES as u64
        {
            undo.remove(0);
        }

        self.redo_stack.borrow_mut().clear();
    }

    fn mark_changed(&self) {
        self.changed.set(true);
        self.obj().queue_draw();
        self.obj().emit_by_name::<()>("changed", &[]);
    }

    fn handle_click(&self, x: f64, y: f64) {
        let tool = self.tool.get();
        let Some((image_x, image_y)) = self.widget_to_image(x, y) else {
            return;
        };

        // A drag is always followed by a click, so a drag that already applied
        // its edit swallows this one instead of drawing a second time.
        if self.suppress_click.take() {
            return;
        }

        match tool {
            DrawTool::Pencil | DrawTool::Brush | DrawTool::Eraser | DrawTool::Highlight => {
                let (image_x, image_y) = self.clamp_to_selection(image_x, image_y);
                let dirty = Dirty::for_stroke(
                    tool,
                    self.line_width.get(),
                    (image_x, image_y),
                    (image_x, image_y),
                );
                self.push_undo(dirty);
                self.draw_dot(tool, image_x, image_y);
                self.mark_changed();
            }
            DrawTool::Bucket => {
                if !self.point_in_selection(image_x, image_y) {
                    return;
                }
                let color = *self.color.borrow();
                // A flood fill can reach any pixel of the image, so the whole
                // surface has to be kept for undo.
                self.push_undo_all();
                self.bucket_fill(image_x, image_y, color);
                self.mark_changed();
            }
            DrawTool::Picker => {
                if let Some(color) = self.pick_color(image_x, image_y) {
                    self.color.replace(color);
                    self.obj()
                        .emit_by_name::<()>("color-picked", &[&color]);
                }
            }
            DrawTool::Text => self.show_text_popover(x, y, image_x, image_y),
            DrawTool::Select => {
                // A click that did not drag clears the selection
                self.selection.borrow_mut().take();
                self.obj().queue_draw();
                self.obj().emit_by_name::<()>("selection-changed", &[]);
            }
            DrawTool::Move => {}
            DrawTool::Line | DrawTool::Arrow | DrawTool::Rectangle
            | DrawTool::RoundedRectangle | DrawTool::Ellipse => {}
        }
    }

    fn handle_drag_begin(&self, x: f64, y: f64) {
        let tool = self.tool.get();
        let Some((image_x, image_y)) = self.widget_to_image(x, y) else {
            if tool == DrawTool::Move {
                self.pan_drag_start.set(self.pan.get());
                self.is_panning.set(true);
            }
            return;
        };

        if tool == DrawTool::Move {
            if let Some((sx0, sy0, sx1, sy1)) = *self.selection.borrow() {
                let (x0, y0, x1, y1) = normalized_selection((sx0, sy0, sx1, sy1));
                let (img_w, img_h) = self.image_size().unwrap_or((0., 0.));
                let x0 = x0.clamp(0., img_w);
                let y0 = y0.clamp(0., img_h);
                let x1 = x1.clamp(0., img_w);
                let y1 = y1.clamp(0., img_h);
                let w = (x1 - x0).round() as i32;
                let h = (y1 - y0).round() as i32;
                let in_selection = image_x >= x0 && image_x <= x1 && image_y >= y0 && image_y <= y1;

                if in_selection && w > 0 && h > 0 {
                    self.push_undo_all();
                    let mut slot = self.surface.borrow_mut();
                    if let Some(surface) = slot.as_mut() {
                        if let Ok(float_surf) = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h) {
                            if let Ok(cr_float) = cairo::Context::new(&float_surf) {
                                let _ = cr_float.set_source_surface(&*surface, -x0, -y0);
                                let _ = cr_float.paint();
                            }
                            if let Ok(cr_main) = cairo::Context::new(&*surface) {
                                cr_main.rectangle(x0, y0, w as f64, h as f64);
                                let _ = cr_main.set_operator(cairo::Operator::Clear);
                                let _ = cr_main.fill();
                            }
                            drop(slot);
                            self.floating_selection.replace(Some(FloatingSelection {
                                surface: float_surf,
                                start_pos: (x0, y0),
                                current_pos: (x0, y0),
                                width: w as f64,
                                height: h as f64,
                            }));
                            self.obj().queue_draw();
                            return;
                        }
                    }
                }
            }
            // If no selection or dragging outside selection, drag pans the view
            self.pan_drag_start.set(self.pan.get());
            self.is_panning.set(true);
            return;
        }

        match tool {
            DrawTool::Pencil | DrawTool::Brush | DrawTool::Eraser | DrawTool::Highlight => {
                let (image_x, image_y) = self.clamp_to_selection(image_x, image_y);
                // The first dot is captured here; the rest of the stroke is
                // captured segment by segment in `handle_drag_update`.
                self.stroke_undo.borrow_mut().clear();
                let dirty = Dirty::for_stroke(
                    tool,
                    self.line_width.get(),
                    (image_x, image_y),
                    (image_x, image_y),
                );
                if let Some(surface) = self.surface.borrow_mut().as_mut() {
                    let snapshot = UndoSnapshot::capture(surface, dirty);
                    self.stroke_undo.borrow_mut().push(snapshot);
                }
                self.draw_dot(tool, image_x, image_y);
                self.stroke.replace(Some(Stroke {
                    tool,
                    start_widget: (x, y),
                    last_image: (image_x, image_y),
                    moved: false,
                }));
                self.mark_changed();
            }
            DrawTool::Line
            | DrawTool::Arrow
            | DrawTool::Rectangle
            | DrawTool::RoundedRectangle
            | DrawTool::Ellipse
            | DrawTool::Select => {
                self.preview.replace(Some(Preview {
                    tool,
                    start_widget: (x, y),
                    start_image: (image_x, image_y),
                    current_image: (image_x, image_y),
                }));
                self.obj().queue_draw();
            }
            _ => {}
        }
    }

    fn handle_drag_update(&self, offset_x: f64, offset_y: f64) {
        if self.tool.get() == DrawTool::Move {
            let mut float_slot = self.floating_selection.borrow_mut();
            if let Some(float) = float_slot.as_mut() {
                let (scale, _, _) = self.fit().unwrap_or((1., 0., 0.));
                float.current_pos = (
                    float.start_pos.0 + offset_x / scale,
                    float.start_pos.1 + offset_y / scale,
                );
                let new_sel = (
                    float.current_pos.0,
                    float.current_pos.1,
                    float.current_pos.0 + float.width,
                    float.current_pos.1 + float.height,
                );
                drop(float_slot);
                self.selection.replace(Some(new_sel));
                self.obj().queue_draw();
                return;
            } else if self.is_panning.get() {
                let start = self.pan_drag_start.get();
                self.pan.set((start.0 + offset_x, start.1 + offset_y));
                self.obj().queue_draw();
                return;
            }
        }

        let current_preview = *self.preview.borrow();
        if let Some(mut preview) = current_preview {
            let widget_x = preview.start_widget.0 + offset_x;
            let widget_y = preview.start_widget.1 + offset_y;

            if let Some(point) = self.clamped_image_point(widget_x, widget_y) {
                preview.current_image = point;
                self.preview.replace(Some(preview));
                self.obj().queue_draw();
            }
            return;
        }

        let Some(mut stroke) = *self.stroke.borrow() else {
            return;
        };

        let widget_x = stroke.start_widget.0 + offset_x;
        let widget_y = stroke.start_widget.1 + offset_y;

        if let Some(point) = self.clamped_image_point(widget_x, widget_y) {
            let dirty = Dirty::for_stroke(
                stroke.tool,
                self.line_width.get(),
                stroke.last_image,
                point,
            );
            if let Some(surface) = self.surface.borrow_mut().as_mut() {
                let snapshot = UndoSnapshot::capture(surface, dirty);
                self.stroke_undo.borrow_mut().push(snapshot);
            }

            self.draw_segment(stroke.tool, stroke.last_image, point);
            stroke.last_image = point;
            stroke.moved = true;
            self.stroke.replace(Some(stroke));

            // While a stroke is being drawn, only repaint the canvas itself.
            // Emitting `changed` on every motion event rebuilds the header and
            // the undo/save buttons on each pointer sample, which is what makes
            // drawing feel sluggish.
            self.changed.set(true);
            self.obj().queue_draw();
        }
    }

    fn handle_drag_end(&self, offset_x: f64, offset_y: f64) {
        if self.tool.get() == DrawTool::Move {
            if let Some(float) = self.floating_selection.borrow_mut().take() {
                let mut slot = self.surface.borrow_mut();
                if let Some(surface) = slot.as_mut() {
                    if let Ok(cr) = cairo::Context::new(surface) {
                        let _ = cr.set_source_surface(&float.surface, float.current_pos.0, float.current_pos.1);
                        let _ = cr.paint();
                    }
                }
                drop(slot);
                self.suppress_click.set(true);
                self.mark_changed();
                self.obj().emit_by_name::<()>("selection-changed", &[]);
            }
            if self.is_panning.get() {
                self.is_panning.set(false);
                self.suppress_click.set(true);
            }
            return;
        }

        let stroke = self.stroke.borrow_mut().take();

        // Every segment captured during the drag becomes one undo step, so a
        // single Ctrl+Z removes the whole stroke.
        let segments = std::mem::take(&mut *self.stroke_undo.borrow_mut());
        if !segments.is_empty() {
            self.push_undo_pixels(UndoPixels::Segments(segments));
            // The drag already drew the stroke; the click that follows must
            // not add a stray dot.
            self.suppress_click.set(true);
        }

        if let Some(stroke) = stroke
            && stroke.moved
        {
            // Undo/Save state is only refreshed once the stroke is finished.
            self.mark_changed();
        }

        if let Some(preview) = self.preview.borrow_mut().take() {
            let widget_x = preview.start_widget.0 + offset_x;
            let widget_y = preview.start_widget.1 + offset_y;
            let end = self
                .clamped_image_point(widget_x, widget_y)
                .unwrap_or(preview.current_image);

            if preview.tool == DrawTool::Select {
                let (x0, y0, x1, y1) =
                    normalized_selection((preview.start_image.0, preview.start_image.1, end.0, end.1));
                if x1 - x0 > 1. && y1 - y0 > 1. {
                    self.selection.replace(Some((x0, y0, x1, y1)));
                } else {
                    self.selection.borrow_mut().take();
                }
                // Keep the selection the drag just made instead of letting the
                // trailing click clear it.
                self.suppress_click.set(true);
                self.obj().queue_draw();
                self.obj().emit_by_name::<()>("selection-changed", &[]);
                return;
            }

            if (end.0 - preview.start_image.0).abs() > 0.5
                || (end.1 - preview.start_image.1).abs() > 0.5
            {
                // Shapes are drawn inside their own bounding box, grown by the
                // stroke width and the arrow head.
                self.push_undo(shape_dirty(preview.tool, self.line_width.get(), preview.start_image, end));
                self.commit_shape(preview.tool, preview.start_image, end);
                self.suppress_click.set(true);
                self.mark_changed();
            }

            self.obj().queue_draw();
        }
    }

    /// Keep a point inside the selection when one exists.
    fn clamp_to_selection(&self, x: f64, y: f64) -> (f64, f64) {
        let Some((x0, y0, x1, y1)) = *self.selection.borrow() else {
            return (x, y);
        };

        (
            x.clamp(x0.min(x1), x0.max(x1)),
            y.clamp(y0.min(y1), y0.max(y1)),
        )
    }

    fn point_in_selection(&self, x: f64, y: f64) -> bool {
        let Some((x0, y0, x1, y1)) = *self.selection.borrow() else {
            return true;
        };

        x >= x0.min(x1) && x <= x0.max(x1) && y >= y0.min(y1) && y <= y0.max(y1)
    }

    fn surface_color(&self) -> [f64; 4] {
        let color = self.color.borrow();
        [color.red() as f64, color.green() as f64, color.blue() as f64, color.alpha() as f64]
    }

    fn draw_dot(&self, tool: DrawTool, x: f64, y: f64) {
        self.draw_segment(tool, (x, y), (x, y));
    }

    fn draw_segment(&self, tool: DrawTool, from: (f64, f64), to: (f64, f64)) {
        let mut slot = self.surface.borrow_mut();
        let Some(surface) = slot.as_mut() else {
            return;
        };
        let Ok(cr) = cairo::Context::new(&*surface) else {
            return;
        };

        let [red, green, blue, alpha] = self.surface_color();
        let width = self.line_width.get();

        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_join(cairo::LineJoin::Round);
        cr.set_line_width(stroke_line_width(tool, width));

        match tool {
            DrawTool::Eraser => {
                let _ = cr.set_operator(cairo::Operator::Clear);
            }
            DrawTool::Highlight => {
                cr.set_source_rgba(red, green, blue, 0.35);
            }
            _ => {
                cr.set_source_rgba(red, green, blue, alpha);
            }
        }

        cr.move_to(from.0, from.1);
        cr.line_to(to.0, to.1);
        let _ = cr.stroke();
        drop(cr);
        surface.flush();
    }

    fn commit_shape(&self, tool: DrawTool, start: (f64, f64), end: (f64, f64)) {
        let mut slot = self.surface.borrow_mut();
        let Some(surface) = slot.as_mut() else {
            return;
        };
        let Ok(cr) = cairo::Context::new(&*surface) else {
            return;
        };

        let [red, green, blue, alpha] = self.surface_color();
        let width = self.line_width.get();

        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_join(cairo::LineJoin::Round);
        cr.set_line_width(width);
        cr.set_source_rgba(red, green, blue, alpha);

        if !shape_path(&cr, tool, start, end, width) {
            return;
        }

        if self.fill_shape.get()
            && matches!(
                tool,
                DrawTool::Rectangle | DrawTool::RoundedRectangle | DrawTool::Ellipse
            )
        {
            let _ = cr.fill_preserve();
        } else if tool == DrawTool::Arrow {
            // The arrowhead is a closed triangle: fill it so the mark reads
            // clearly even at small stroke widths.
            let _ = cr.fill_preserve();
        }

        let _ = cr.stroke();
        drop(cr);
        surface.flush();
    }

    fn bucket_fill(&self, x: f64, y: f64, color: gdk::RGBA) {
        let mut slot = self.surface.borrow_mut();
        let Some(surface) = slot.as_mut() else {
            return;
        };
        let width = surface.width() as usize;
        let height = surface.height() as usize;
        let stride = surface.stride() as usize;

        let Ok(mut data) = surface.data() else {
            return;
        };

        let x = x as usize;
        let y = y as usize;
        if x >= width || y >= height {
            return;
        }

        let alpha = (color.alpha() * 255.).round() as u8;
        let new_pixel = [
            (color.blue() * 255. * color.alpha()).round() as u8,
            (color.green() * 255. * color.alpha()).round() as u8,
            (color.red() * 255. * color.alpha()).round() as u8,
            alpha,
        ];

        flood_fill(&mut data, width, height, stride, x, y, new_pixel);
    }

    fn pick_color(&self, x: f64, y: f64) -> Option<gdk::RGBA> {
        let mut slot = self.surface.borrow_mut();
        let surface = slot.as_mut()?;
        let width = surface.width() as usize;
        let height = surface.height() as usize;
        let stride = surface.stride() as usize;

        let x = x as usize;
        let y = y as usize;
        if x >= width || y >= height {
            return None;
        }

        let data = surface.data().ok()?;
        let index = y * stride + x * 4;
        let blue = data[index] as f64 / 255.;
        let green = data[index + 1] as f64 / 255.;
        let red = data[index + 2] as f64 / 255.;
        let alpha = data[index + 3] as f64 / 255.;

        let (red, green, blue) = if alpha > 0. {
            (
                (red / alpha).min(1.),
                (green / alpha).min(1.),
                (blue / alpha).min(1.),
            )
        } else {
            (0., 0., 0.)
        };

        Some(gdk::RGBA::new(red as f32, green as f32, blue as f32, alpha as f32))
    }

    fn show_text_popover(&self, widget_x: f64, widget_y: f64, image_x: f64, image_y: f64) {
        let obj = self.obj();
        let popover = gtk::Popover::new();
        popover.set_parent(&*obj);

        let root_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root_box.set_margin_top(8);
        root_box.set_margin_bottom(8);
        root_box.set_margin_start(8);
        root_box.set_margin_end(8);

        // 1. Controls bar
        let controls_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);

        let families = ["Sans", "Serif", "Monospace", "Cantarell", "DejaVu Sans", "Ubuntu", "Roboto"];
        let font_model = gtk::StringList::new(&families);
        let font_dropdown = gtk::DropDown::new(Some(font_model), gtk::Expression::NONE);
        font_dropdown.set_selected(0);
        font_dropdown.set_tooltip_text(Some(&gettext("Font Family")));
        controls_box.append(&font_dropdown);

        let size_spin = gtk::SpinButton::with_range(8., 200., 2.);
        let init_size = (self.line_width.get() * 5.).clamp(14., 96.);
        size_spin.set_value(init_size);
        size_spin.set_tooltip_text(Some(&gettext("Font Size (px)")));
        controls_box.append(&size_spin);

        let bold_btn = gtk::ToggleButton::new();
        bold_btn.set_icon_name("format-text-bold-symbolic");
        bold_btn.set_tooltip_text(Some(&gettext("Bold")));
        controls_box.append(&bold_btn);

        let italic_btn = gtk::ToggleButton::new();
        italic_btn.set_icon_name("format-text-italic-symbolic");
        italic_btn.set_tooltip_text(Some(&gettext("Italic")));
        controls_box.append(&italic_btn);

        let wrap_btn = gtk::ToggleButton::new();
        wrap_btn.set_icon_name("format-justify-left-symbolic");
        wrap_btn.set_active(true);
        wrap_btn.set_tooltip_text(Some(&gettext("Wrap Text")));
        controls_box.append(&wrap_btn);

        root_box.append(&controls_box);

        // 2. Multiline Text View
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_min_content_width(320);
        scroll.set_min_content_height(100);
        scroll.set_max_content_height(220);
        scroll.set_propagate_natural_height(true);

        let text_view = gtk::TextView::new();
        text_view.set_wrap_mode(gtk::WrapMode::WordChar);
        text_view.set_accepts_tab(false);
        text_view.set_margin_top(4);
        text_view.set_margin_bottom(4);
        text_view.set_margin_start(4);
        text_view.set_margin_end(4);

        wrap_btn.connect_toggled(glib::clone!(
            #[weak]
            text_view,
            move |btn| {
                text_view.set_wrap_mode(if btn.is_active() {
                    gtk::WrapMode::WordChar
                } else {
                    gtk::WrapMode::None
                });
            }
        ));

        scroll.set_child(Some(&text_view));
        root_box.append(&scroll);

        // 3. Actions Row
        let actions_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let hint = gtk::Label::new(Some(&gettext("Ctrl+Enter to apply • Esc to cancel")));
        hint.add_css_class("caption");
        hint.add_css_class("dimmed");
        hint.set_hexpand(true);
        hint.set_xalign(0.);
        actions_box.append(&hint);

        let cancel_btn = gtk::Button::with_label(&gettext("Cancel"));
        cancel_btn.connect_clicked(glib::clone!(
            #[weak]
            popover,
            move |_| popover.popdown()
        ));
        actions_box.append(&cancel_btn);

        let apply_btn = gtk::Button::with_label(&gettext("Apply"));
        apply_btn.add_css_class("suggested-action");
        actions_box.append(&apply_btn);

        root_box.append(&actions_box);
        popover.set_child(Some(&root_box));

        let rect = gdk::Rectangle::new(widget_x as i32 - 8, widget_y as i32 - 8, 16, 16);
        popover.set_pointing_to(Some(&rect));
        popover.connect_closed(|p| p.unparent());

        let commit_text = glib::clone!(
            #[weak(rename_to = canvas)]
            obj,
            #[weak]
            popover,
            #[weak]
            text_view,
            #[weak]
            font_dropdown,
            #[weak]
            size_spin,
            #[weak]
            bold_btn,
            #[weak]
            italic_btn,
            #[weak]
            wrap_btn,
            move || {
                let buffer = text_view.buffer();
                let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
                if !text.trim().is_empty() {
                    let idx = font_dropdown.selected() as usize;
                    let family = families.get(idx).unwrap_or(&"Sans");
                    let font_size = size_spin.value();
                    let bold = bold_btn.is_active();
                    let italic = italic_btn.is_active();
                    let wrap = wrap_btn.is_active();

                    canvas.imp().push_undo_all();
                    canvas.imp().draw_text_styled(
                        &text,
                        image_x,
                        image_y,
                        family,
                        font_size,
                        bold,
                        italic,
                        wrap,
                    );
                    canvas.imp().mark_changed();
                }
                popover.popdown();
            }
        );

        apply_btn.connect_clicked(glib::clone!(
            #[strong]
            commit_text,
            move |_| commit_text()
        ));

        let key_controller = gtk::EventControllerKey::new();
        key_controller.connect_key_pressed(glib::clone!(
            #[weak]
            popover,
            #[strong]
            commit_text,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, keyval, _, state| {
                if state.contains(gdk::ModifierType::CONTROL_MASK)
                    && (keyval == gdk::Key::Return || keyval == gdk::Key::KP_Enter)
                {
                    commit_text();
                    glib::Propagation::Stop
                } else if keyval == gdk::Key::Escape {
                    popover.popdown();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        text_view.add_controller(key_controller);

        popover.popup();
        text_view.grab_focus();
    }

    fn draw_text_styled(
        &self,
        text: &str,
        x: f64,
        y: f64,
        family: &str,
        font_size: f64,
        bold: bool,
        italic: bool,
        wrap: bool,
    ) {
        let mut slot = self.surface.borrow_mut();
        let Some(surface) = slot.as_mut() else {
            return;
        };
        let Ok(cr) = cairo::Context::new(&*surface) else {
            return;
        };

        let [red, green, blue, alpha] = self.surface_color();
        cr.set_source_rgba(red, green, blue, alpha);

        let layout = pangocairo::functions::create_layout(&cr);
        layout.set_text(text);

        let mut font = pango::FontDescription::new();
        font.set_family(family);
        font.set_absolute_size(font_size * pango::SCALE as f64);
        if bold {
            font.set_weight(pango::Weight::Bold);
        }
        if italic {
            font.set_style(pango::Style::Italic);
        }
        layout.set_font_description(Some(&font));

        if wrap {
            let (image_width, _) = self.image_size().unwrap_or((1920., 1080.));
            let wrap_width = (image_width - x).max(120.);
            layout.set_width((wrap_width * pango::SCALE as f64) as i32);
            layout.set_wrap(pango::WrapMode::WordChar);
        }

        cr.move_to(x, y);
        pangocairo::functions::show_layout(&cr, &layout);
        drop(cr);
        surface.flush();
    }
}

fn draw_checkerboard(cr: &cairo::Context, width: f64, height: f64) {
    const CELL: f64 = 12.;

    thread_local! {
        /// 2×2 cell tile, built once and reused as a repeating pattern.
        static TILE: std::cell::RefCell<Option<cairo::ImageSurface>> =
            const { std::cell::RefCell::new(None) };
    }

    let tile = TILE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let size = (CELL * 2.) as i32;
            let Ok(surface) = cairo::ImageSurface::create(cairo::Format::ARgb32, size, size)
            else {
                return None;
            };
            let Ok(tile_cr) = cairo::Context::new(&surface) else {
                return None;
            };
            tile_cr.set_source_rgb(0.16, 0.16, 0.18);
            let _ = tile_cr.paint();
            tile_cr.set_source_rgb(0.22, 0.22, 0.25);
            tile_cr.rectangle(0., 0., CELL, CELL);
            tile_cr.rectangle(CELL, CELL, CELL, CELL);
            let _ = tile_cr.fill();
            *slot = Some(surface);
        }
        slot.as_ref().cloned()
    });

    let Some(tile) = tile else {
        return;
    };

    let pattern = cairo::SurfacePattern::create(&tile);
    pattern.set_extend(cairo::Extend::Repeat);
    pattern.set_filter(cairo::Filter::Nearest);
    let _ = cr.set_source(&pattern);
    let _ = cr.rectangle(0., 0., width, height);
    let _ = cr.fill();
}

fn draw_selection(cr: &cairo::Context, start: &(f64, f64), end: &(f64, f64)) {
    let (x0, y0, x1, y1) = normalized_selection((start.0, start.1, end.0, end.1));

    cr.set_line_width(1.);
    cr.set_dash(&[6., 4.], 0.);

    cr.set_source_rgba(0., 0., 0., 0.8);
    cr.rectangle(x0, y0, x1 - x0, y1 - y0);
    let _ = cr.stroke();

    cr.set_dash(&[6., 4.], 6.);
    cr.set_source_rgba(1., 1., 1., 0.9);
    cr.rectangle(x0, y0, x1 - x0, y1 - y0);
    let _ = cr.stroke();

    cr.set_dash(&[], 0.);
}

fn normalized_selection((x0, y0, x1, y1): (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    (x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
}

/// Turn an image-space rectangle into an undo region.
fn dirty_rect((x0, y0, x1, y1): (f64, f64, f64, f64)) -> Dirty {
    let (x0, y0, x1, y1) = normalized_selection((x0, y0, x1, y1));
    Dirty::Rect(x0, y0, x1, y1)
}

/// Region a drag-created shape can touch, including its stroke width and the
/// arrow head, which sticks out beyond the drag itself.
fn shape_dirty(tool: DrawTool, line_width: f64, start: (f64, f64), end: (f64, f64)) -> Dirty {
    let margin = match tool {
        DrawTool::Arrow => (line_width * 4.).clamp(8., 64.),
        _ => line_width,
    } / 2.
        + 2.;

    Dirty::Rect(
        start.0.min(end.0) - margin,
        start.1.min(end.1) - margin,
        start.0.max(end.0) + margin,
        start.1.max(end.1) + margin,
    )
}

/// Stroke width actually painted for a tool, in image pixels.
fn stroke_line_width(tool: DrawTool, line_width: f64) -> f64 {
    match tool {
        DrawTool::Pencil => 1.5,
        DrawTool::Highlight => line_width * 2.,
        _ => line_width,
    }
}

/// Largest blur radius the UI offers, so filters always copy enough context
/// around the selection to sample their neighbourhood correctly.
const FILTER_MARGIN: i64 = 32;

/// A rectangle of RGBA pixels copied out of the canvas surface.
///
/// `inner` is the part the filter may modify, relative to the rectangle: a
/// blur reads `FILTER_MARGIN` pixels outside it but must not smear the
/// surrounding pixels beyond the selection.
struct ImageRegion {
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    inner: (usize, usize, usize, usize),
    data: Vec<u8>,
}

/// Run one filter over an isolated copy of the affected rectangle.
///
/// This runs on a worker thread and owns its pixels outright, so a filter can
/// take as long as it needs without freezing the window.
fn filter_pixels(filter: DrawFilter, region: &mut ImageRegion) {
    let ImageRegion { width, height, data, inner, .. } = region;
    let (width, height) = (*width, *height);
    let (left, top, right, bottom) = *inner;
    let stride = width * 4;
    let data: &mut [u8] = data;

    match filter {
                DrawFilter::Grayscale | DrawFilter::Invert => {
                    for y in top..bottom {
                        for x in left..right {
                            let offset = y * stride + x * 4;
                            let blue = data[offset] as f64;
                            let green = data[offset + 1] as f64;
                            let red = data[offset + 2] as f64;

                            let (red, green, blue) = match filter {
                                DrawFilter::Grayscale => {
                                    let luma = 0.299 * red + 0.587 * green + 0.114 * blue;
                                    (luma, luma, luma)
                                }
                                _ => (255. - red, 255. - green, 255. - blue),
                            };

                            data[offset] = blue.clamp(0., 255.) as u8;
                            data[offset + 1] = green.clamp(0., 255.) as u8;
                            data[offset + 2] = red.clamp(0., 255.) as u8;
                        }
                    }
                }
                DrawFilter::Saturation(amount) => {
                    for y in top..bottom {
                        for x in left..right {
                            let offset = y * stride + x * 4;
                            let blue = data[offset] as f64;
                            let green = data[offset + 1] as f64;
                            let red = data[offset + 2] as f64;
                            let luma = 0.299 * red + 0.587 * green + 0.114 * blue;

                            data[offset] = (luma + (blue - luma) * (1. + amount)).clamp(0., 255.) as u8;
                            data[offset + 1] =
                                (luma + (green - luma) * (1. + amount)).clamp(0., 255.) as u8;
                            data[offset + 2] = (luma + (red - luma) * (1. + amount)).clamp(0., 255.) as u8;
                        }
                    }
                }
                DrawFilter::Brightness(amount) => {
                    let offset_value = amount * 255.;
                    for y in top..bottom {
                        for x in left..right {
                            let offset = y * stride + x * 4;
                            for channel in 0..3 {
                                data[offset + channel] =
                                    (data[offset + channel] as f64 + offset_value).clamp(0., 255.) as u8;
                            }
                        }
                    }
                }
                DrawFilter::Contrast(amount) => {
                    let factor = 1. + amount;
                    for y in top..bottom {
                        for x in left..right {
                            let offset = y * stride + x * 4;
                            for channel in 0..3 {
                                let value = data[offset + channel] as f64;
                                data[offset + channel] =
                                    (127.5 + (value - 127.5) * factor).clamp(0., 255.) as u8;
                            }
                        }
                    }
                }
                DrawFilter::Pixelate(block) => {
                    let block = block.max(1) as usize;
                    let mut y = top;
                    while y < bottom {
                        let end_y = (y + block).min(bottom);
                        let mut x = left;
                        while x < right {
                            let end_x = (x + block).min(right);
                            let mut sums = [0u32; 4];
                            let mut samples = 0u32;

                            for sample_y in y..end_y {
                                for sample_x in x..end_x {
                                    let offset = sample_y * stride + sample_x * 4;
                                    for (index, sum) in sums.iter_mut().enumerate() {
                                        *sum += data[offset + index] as u32;
                                    }
                                    samples += 1;
                                }
                            }

                            if samples > 0 {
                                for sample_y in y..end_y {
                                    for sample_x in x..end_x {
                                        let offset = sample_y * stride + sample_x * 4;
                                        for (index, sum) in sums.iter().enumerate() {
                                            data[offset + index] = (sum / samples) as u8;
                                        }
                                    }
                                }
                            }

                            x = end_x;
                        }
                        y = end_y;
                    }
                }
                DrawFilter::Blur(radius) => {
                    let radius = radius.clamp(1, 32) as usize;
                    let source = data.to_vec();
                    for y in top..bottom {
                        for x in left..right {
                            let mut sums = [0u32; 4];
                            let mut samples = 0u32;

                            for sample_y in
                                y.saturating_sub(radius)..(y + radius + 1).min(height)
                            {
                                for sample_x in
                                    x.saturating_sub(radius)..(x + radius + 1).min(width)
                                {
                                    let offset = sample_y * stride + sample_x * 4;
                                    for (index, sum) in sums.iter_mut().enumerate() {
                                        *sum += source[offset + index] as u32;
                                    }
                                    samples += 1;
                                }
                            }

                            let offset = y * stride + x * 4;
                            for (index, sum) in sums.iter().enumerate() {
                                data[offset + index] = (sum / samples.max(1)) as u8;
                            }
                        }
                    }
                }
                DrawFilter::Sharpen(amount) => {
                    let source = data.to_vec();
                    for y in top..bottom {
                        for x in left..right {
                            let mut sums = [0u32; 4];
                            for sample_y in y.saturating_sub(1)..(y + 2).min(height) {
                                for sample_x in x.saturating_sub(1)..(x + 2).min(width) {
                                    let offset = sample_y * stride + sample_x * 4;
                                    for (index, sum) in sums.iter_mut().enumerate() {
                                        *sum += source[offset + index] as u32;
                                    }
                                }
                            }

                            let offset = y * stride + x * 4;
                            for index in 0..4 {
                                let blurred = sums[index] as f64 / 9.;
                                let value = source[offset + index] as f64;
                                data[offset + index] =
                                    (value + (value - blurred) * amount).clamp(0., 255.) as u8;
                            }
                        }
                    }
                }
    }
}

/// Build the Cairo path for a drag-created shape. Returns `false` when the
/// tool is not a shape.
fn shape_path(
    cr: &cairo::Context,
    tool: DrawTool,
    start: (f64, f64),
    end: (f64, f64),
    line_width: f64,
) -> bool {
    let (x0, y0) = start;
    let (x1, y1) = end;

    match tool {
        DrawTool::Line => {
            cr.move_to(x0, y0);
            cr.line_to(x1, y1);
        }
        DrawTool::Arrow => arrow_path(cr, start, end, line_width),
        DrawTool::Rectangle => {
            cr.rectangle(x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs());
        }
        DrawTool::RoundedRectangle => rounded_rect_path(cr, x0, y0, x1, y1, line_width),
        DrawTool::Ellipse => ellipse_path(cr, x0, y0, x1, y1),
        _ => return false,
    }

    true
}

/// A line from `start` to `end` with a filled triangular head at `end`.
///
/// The head is sized from the stroke width but never smaller than a few
/// pixels, so even a 1 px arrow stays visible when marking up a screenshot.
fn arrow_path(cr: &cairo::Context, start: (f64, f64), end: (f64, f64), line_width: f64) {
    let (x0, y0) = start;
    let (x1, y1) = end;
    let dx = x1 - x0;
    let dy = y1 - y0;
    let length = dx.hypot(dy);

    if length < 0.5 {
        return;
    }

    let head = (line_width * 4.).clamp(8., 64.).min(length);
    let angle = dy.atan2(dx);
    let spread = std::f64::consts::PI / 7.;

    // Stop the shaft where the head begins so the join stays clean.
    let shaft_x = x1 - head * 0.85 * angle.cos();
    let shaft_y = y1 - head * 0.85 * angle.sin();

    cr.move_to(x0, y0);
    cr.line_to(shaft_x, shaft_y);

    let left = (x1 - head * (angle - spread).cos(), y1 - head * (angle - spread).sin());
    let right = (x1 - head * (angle + spread).cos(), y1 - head * (angle + spread).sin());

    cr.move_to(x1, y1);
    cr.line_to(left.0, left.1);
    cr.line_to(right.0, right.1);
    cr.close_path();
}

fn rounded_rect_path(cr: &cairo::Context, x0: f64, y0: f64, x1: f64, y1: f64, line_width: f64) {
    let left = x0.min(x1);
    let top = y0.min(y1);
    let width = (x1 - x0).abs();
    let height = (y1 - y0).abs();
    let radius = (line_width * 3.).clamp(6., 48.).min(width / 2.).min(height / 2.);

    if radius <= 0.5 {
        cr.rectangle(left, top, width, height);
        return;
    }

    const KAPPA: f64 = 0.552_284_749_8;
    let r = radius;
    let right = left + width;
    let bottom = top + height;

    cr.new_sub_path();
    cr.move_to(left + r, top);
    cr.line_to(right - r, top);
    cr.curve_to(right - r + KAPPA * r, top, right, top + r - KAPPA * r, right, top + r);
    cr.line_to(right, bottom - r);
    cr.curve_to(right, bottom - r + KAPPA * r, right - r + KAPPA * r, bottom, right - r, bottom);
    cr.line_to(left + r, bottom);
    cr.curve_to(left + r - KAPPA * r, bottom, left, bottom - r + KAPPA * r, left, bottom - r);
    cr.line_to(left, top + r);
    cr.curve_to(left, top + r - KAPPA * r, left + r - KAPPA * r, top, left + r, top);
    cr.close_path();
}

fn draw_preview(cr: &cairo::Context, preview: &Preview, color: &gdk::RGBA, width: f64) {
    cr.set_line_width(width);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    cr.set_source_rgba(
        color.red() as f64,
        color.green() as f64,
        color.blue() as f64,
        0.8,
    );

    if !shape_path(
        cr,
        preview.tool,
        preview.start_image,
        preview.current_image,
        width,
    ) {
        return;
    }

    let _ = cr.stroke();
}

fn ellipse_path(cr: &cairo::Context, x0: f64, y0: f64, x1: f64, y1: f64) {
    const KAPPA: f64 = 0.552_284_749_8;

    let center_x = (x0 + x1) / 2.;
    let center_y = (y0 + y1) / 2.;
    let radius_x = (x1 - x0).abs() / 2.;
    let radius_y = (y1 - y0).abs() / 2.;

    cr.new_sub_path();
    cr.move_to(center_x + radius_x, center_y);
    cr.curve_to(
        center_x + radius_x,
        center_y + KAPPA * radius_y,
        center_x + KAPPA * radius_x,
        center_y + radius_y,
        center_x,
        center_y + radius_y,
    );
    cr.curve_to(
        center_x - KAPPA * radius_x,
        center_y + radius_y,
        center_x - radius_x,
        center_y + KAPPA * radius_y,
        center_x - radius_x,
        center_y,
    );
    cr.curve_to(
        center_x - radius_x,
        center_y - KAPPA * radius_y,
        center_x - KAPPA * radius_x,
        center_y - radius_y,
        center_x,
        center_y - radius_y,
    );
    cr.curve_to(
        center_x + KAPPA * radius_x,
        center_y - radius_y,
        center_x + radius_x,
        center_y - KAPPA * radius_y,
        center_x + radius_x,
        center_y,
    );
    cr.close_path();
}

fn flood_fill(
    data: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    start_x: usize,
    start_y: usize,
    new_pixel: [u8; 4],
) {
    let index = start_y * stride + start_x * 4;
    let target = [data[index], data[index + 1], data[index + 2], data[index + 3]];

    if target == new_pixel {
        return;
    }

    let tolerance = 24u8;
    let mut visited = vec![false; width * height];
    let mut stack = vec![(start_x, start_y)];

    while let Some((x, y)) = stack.pop() {
        let visited_index = y * width + x;
        if visited[visited_index] {
            continue;
        }
        visited[visited_index] = true;

        let index = y * stride + x * 4;
        let pixel = [data[index], data[index + 1], data[index + 2], data[index + 3]];

        let matches = pixel
            .iter()
            .zip(target.iter())
            .all(|(a, b)| a.abs_diff(*b) <= tolerance);
        if !matches {
            continue;
        }

        data[index..index + 4].copy_from_slice(&new_pixel);

        if x > 0 {
            stack.push((x - 1, y));
        }
        if x + 1 < width {
            stack.push((x + 1, y));
        }
        if y > 0 {
            stack.push((x, y - 1));
        }
        if y + 1 < height {
            stack.push((x, y + 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a region filled by `pixel`, with `inner` marking the sub rectangle
    /// the filter is allowed to modify.
    fn region(
        width: usize,
        height: usize,
        inner: (usize, usize, usize, usize),
        pixel: impl Fn(usize, usize) -> [u8; 4],
    ) -> ImageRegion {
        let mut data = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                data.extend_from_slice(&pixel(x, y));
            }
        }
        ImageRegion { left: 0, top: 0, width, height, inner, data }
    }

    fn pixel_at(region: &ImageRegion, x: usize, y: usize) -> [u8; 4] {
        let offset = (y * region.width + x) * 4;
        region.data[offset..offset + 4].try_into().unwrap()
    }

    /// Spatially uniform regions must survive the neighbourhood filters
    /// unchanged: blur, sharpen and pixelate have nothing to vary.
    #[test]
    fn uniform_region_is_unchanged() {
        for filter in [
            DrawFilter::Grayscale,
            DrawFilter::Saturation(0.5),
            DrawFilter::Pixelate(4),
            DrawFilter::Blur(2),
            DrawFilter::Sharpen(1.),
        ] {
            let mut area = region(6, 6, (0, 0, 6, 6), |_, _| [120, 120, 120, 255]);
            filter_pixels(filter, &mut area);
            assert_eq!(
                pixel_at(&area, 3, 3),
                [120, 120, 120, 255],
                "{filter:?} changed a uniform region"
            );
        }

        // The point filters are checked with their exact arithmetic, which also
        // pins down the channel interpretation.
        let mut area = region(3, 3, (0, 0, 3, 3), |_, _| [120, 120, 120, 255]);
        filter_pixels(DrawFilter::Brightness(0.1), &mut area);
        assert_eq!(pixel_at(&area, 1, 1), [145, 145, 145, 255]);

        // 127.5 + (120 - 127.5) * 1.2 = 118.5, truncated to 118.
        let mut area = region(3, 3, (0, 0, 3, 3), |_, _| [120, 120, 120, 255]);
        filter_pixels(DrawFilter::Contrast(0.2), &mut area);
        assert_eq!(pixel_at(&area, 1, 1), [118, 118, 118, 255]);
    }

    /// Grayscale must use the luma weights, and invert must flip each channel.
    #[test]
    fn grayscale_and_invert_channels() {
        let mut area = region(2, 2, (0, 0, 2, 2), |_, _| [10, 20, 40, 255]);
        filter_pixels(DrawFilter::Grayscale, &mut area);
        // 0.299 * 40 + 0.587 * 20 + 0.114 * 10 = 24.74
        assert_eq!(pixel_at(&area, 0, 0), [24, 24, 24, 255]);

        let mut area = region(2, 2, (0, 0, 2, 2), |_, _| [10, 20, 40, 255]);
        // Region pixels are stored BGRA, so red is the third byte.
        filter_pixels(DrawFilter::Invert, &mut area);
        assert_eq!(pixel_at(&area, 0, 0), [255 - 10, 255 - 20, 255 - 40, 255]);
    }

    /// The margin exists so blur can read real neighbours; the pixels outside
    /// `inner` must be left exactly as they were.
    #[test]
    fn filter_leaves_the_margin_untouched() {
        // Alternating light and dark rows, so blurring really does change the
        // inner pixels.
        let mut area = region(8, 8, (2, 2, 6, 6), |x, y| {
            let value = if (x + y) % 2 == 0 { 0 } else { 240 };
            [value, value, value, 255]
        });
        let before = area.data.clone();

        filter_pixels(DrawFilter::Blur(1), &mut area);

        for y in 0..8 {
            for x in 0..8 {
                let inside = (2..6).contains(&x) && (2..6).contains(&y);
                let offset = (y * 8 + x) * 4;
                if inside {
                    assert_ne!(
                        area.data[offset..offset + 4],
                        before[offset..offset + 4],
                        "blur left the inner pixel at ({x}, {y}) unchanged"
                    );
                } else {
                    assert_eq!(
                        area.data[offset..offset + 4],
                        before[offset..offset + 4],
                        "blur modified the margin pixel at ({x}, {y})"
                    );
                }
            }
        }
    }

    /// Blur has to sample pixels just outside the inner rectangle, which is the
    /// whole reason `read_region` copies a margin.
    #[test]
    fn blur_samples_beyond_the_inner_rectangle() {
        // A dark image with a single bright pixel row *outside* the inner area.
        let mut area = region(6, 6, (1, 3, 5, 5), |_, y| {
            if y == 2 {
                [240, 240, 240, 255]
            } else {
                [0, 0, 0, 255]
            }
        });
        filter_pixels(DrawFilter::Blur(2), &mut area);

        // The inner rows sit directly under the bright row, so they must have
        // been brightened by it.
        assert!(pixel_at(&area, 3, 3)[0] > 0, "blur ignored the row outside inner");
    }

    /// Pixelate averages whole blocks inside the inner rectangle.
    #[test]
    fn pixelate_averages_blocks() {
        let mut area = region(4, 4, (0, 0, 4, 4), |x, _| {
            if x < 2 { [0, 0, 0, 255] } else { [200, 200, 200, 255] }
        });
        filter_pixels(DrawFilter::Pixelate(2), &mut area);

        assert_eq!(pixel_at(&area, 0, 0)[0], 0);
        assert_eq!(pixel_at(&area, 0, 3)[0], 0);
        assert_eq!(pixel_at(&area, 3, 0)[0], 200);
        assert_eq!(pixel_at(&area, 2, 2)[0], 200);
    }

    #[test]
    fn oversized_and_empty_images_are_rejected() {
        assert!(canvas_size_error(0, 480).is_some());
        assert!(canvas_size_error(640, 0).is_some());
        assert!(canvas_size_error(-1, 480).is_some());
        assert!(canvas_size_error(MAX_CANVAS_SIDE + 1, 480).is_some());
        assert!(canvas_size_error(640, MAX_CANVAS_SIDE + 1).is_some());

        assert!(canvas_size_error(1, 1).is_none());
        assert!(canvas_size_error(640, 480).is_none());
        assert!(canvas_size_error(MAX_CANVAS_SIDE, MAX_CANVAS_SIDE).is_none());
    }
}
