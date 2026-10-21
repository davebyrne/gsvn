//! The text widgets of the diff window.
//!
//! [`DiffView`] is a read-only `GtkSourceView` that knows the [`Row`] of
//! every buffer line. It paints line backgrounds (added, removed, hunk
//! header, hatched filler) itself in the below-text layer, only for the
//! visible lines, so a 50 000-line diff needs no per-line tags.
//! [`LineNumbers`] is a gutter renderer drawing the old or the new line
//! number of each row from the same model.

use super::model::{Rendered, Row, RowKind};
use adw::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk, pango};
use sourceview5::prelude::*;
use sourceview5::subclass::prelude::*;
use std::cell::{Cell, RefCell};

/// Theme-dependent colours.
pub struct Palette {
    added: gdk::RGBA,
    removed: gdk::RGBA,
    added_strong: gdk::RGBA,
    removed_strong: gdk::RGBA,
    header_bg: gdk::RGBA,
    header_fg: gdk::RGBA,
    section_bg: gdk::RGBA,
    filler: gdk::RGBA,
    stripe: gdk::RGBA,
    meta_fg: gdk::RGBA,
}

fn rgba(r: u8, g: u8, b: u8, a: f32) -> gdk::RGBA {
    gdk::RGBA::new(
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        a,
    )
}

impl Palette {
    pub fn current() -> Self {
        if adw::StyleManager::default().is_dark() {
            Self {
                added: rgba(38, 162, 105, 0.22),
                removed: rgba(224, 27, 36, 0.24),
                added_strong: rgba(38, 162, 105, 0.55),
                removed_strong: rgba(237, 51, 59, 0.55),
                header_bg: rgba(53, 132, 228, 0.20),
                header_fg: rgba(153, 193, 241, 1.0),
                section_bg: rgba(255, 255, 255, 0.08),
                filler: rgba(255, 255, 255, 0.025),
                stripe: rgba(255, 255, 255, 0.06),
                meta_fg: rgba(255, 255, 255, 0.5),
            }
        } else {
            Self {
                added: rgba(46, 194, 126, 0.16),
                removed: rgba(224, 27, 36, 0.12),
                added_strong: rgba(46, 194, 126, 0.42),
                removed_strong: rgba(224, 27, 36, 0.30),
                header_bg: rgba(53, 132, 228, 0.12),
                header_fg: rgba(26, 95, 180, 1.0),
                section_bg: rgba(0, 0, 0, 0.06),
                filler: rgba(0, 0, 0, 0.025),
                stripe: rgba(0, 0, 0, 0.07),
                meta_fg: rgba(0, 0, 0, 0.5),
            }
        }
    }

    fn row_bg(&self, kind: RowKind) -> Option<&gdk::RGBA> {
        match kind {
            RowKind::Added => Some(&self.added),
            RowKind::Removed => Some(&self.removed),
            RowKind::Header => Some(&self.header_bg),
            RowKind::Section | RowKind::PropName => Some(&self.section_bg),
            RowKind::Filler => Some(&self.filler),
            RowKind::Context | RowKind::Meta => None,
        }
    }

    /// Paint the background of a row at `rect`. Fillers get diagonal
    /// hatching anchored to the buffer origin so it is continuous.
    fn paint_row(&self, snapshot: &gtk::Snapshot, kind: RowKind, rect: &graphene::Rect) {
        if let Some(bg) = self.row_bg(kind) {
            snapshot.append_color(bg, rect);
        }
        if kind == RowKind::Filler {
            let transparent = gdk::RGBA::new(0.0, 0.0, 0.0, 0.0);
            snapshot.append_repeating_linear_gradient(
                rect,
                &graphene::Point::new(0.0, 0.0),
                &graphene::Point::new(5.0, 5.0),
                &[
                    gsk::ColorStop::new(0.0, self.stripe),
                    gsk::ColorStop::new(0.35, self.stripe),
                    gsk::ColorStop::new(0.35, transparent),
                    gsk::ColorStop::new(1.0, transparent),
                ],
            );
        }
    }
}

/// Which line number a [`LineNumbers`] renderer shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Column {
    #[default]
    Old,
    New,
}

const TAG_INTRA_REMOVED: &str = "intra-removed";
const TAG_INTRA_ADDED: &str = "intra-added";
const TAG_HEADER: &str = "header";
const TAG_META: &str = "meta";
const TAG_BOLD: &str = "bold";

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct DiffView {
        pub rows: RefCell<Vec<Row>>,
        /// Largest old/new line number, for gutter widths.
        pub max_numbers: Cell<(u32, u32)>,
        pub renderers: RefCell<Vec<super::LineNumbers>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DiffView {
        const NAME: &'static str = "GsvnDiffView";
        type Type = super::DiffView;
        type ParentType = sourceview5::View;
    }

    impl ObjectImpl for DiffView {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let buffer = sourceview5::Buffer::new(None);
            buffer.set_enable_undo(false);
            buffer.set_highlight_matching_brackets(false);
            let table = buffer.tag_table();
            for name in [TAG_INTRA_REMOVED, TAG_INTRA_ADDED, TAG_HEADER, TAG_META] {
                table.add(&gtk::TextTag::new(Some(name)));
            }
            table.add(&gtk::TextTag::builder().name(TAG_BOLD).weight(700).build());
            if let Some(meta) = table.lookup(TAG_META) {
                meta.set_style(pango::Style::Italic);
            }
            crate::common::follow_style_scheme(&buffer);
            obj.set_buffer(Some(&buffer));
            obj.set_editable(false);
            obj.set_cursor_visible(false);
            obj.set_monospace(true);
            obj.set_show_line_numbers(false);
            obj.set_highlight_current_line(false);
            obj.set_left_margin(8);
            obj.set_top_margin(4);
            obj.set_bottom_margin(4);
            obj.apply_palette();

            let view = obj.downgrade();
            adw::StyleManager::default().connect_dark_notify(move |_| {
                if let Some(view) = view.upgrade() {
                    view.apply_palette();
                }
            });
        }
    }

    impl WidgetImpl for DiffView {}

    impl TextViewImpl for DiffView {
        fn snapshot_layer(&self, layer: gtk::TextViewLayer, snapshot: gtk::Snapshot) {
            self.parent_snapshot_layer(layer, snapshot.clone());
            if layer == gtk::TextViewLayer::BelowText {
                self.paint_backgrounds(&snapshot);
            }
        }
    }

    impl ViewImpl for DiffView {}

    impl DiffView {
        /// Row backgrounds of the visible lines (buffer coordinates).
        fn paint_backgrounds(&self, snapshot: &gtk::Snapshot) {
            let rows = self.rows.borrow();
            if rows.is_empty() {
                return;
            }
            let obj = self.obj();
            let visible = obj.visible_rect();
            let bottom = visible.y() + visible.height();
            let palette = Palette::current();
            let (mut iter, _) = obj.line_at_y(visible.y());
            loop {
                let (y, height) = obj.line_yrange(&iter);
                if y >= bottom {
                    break;
                }
                if let Some(row) = rows.get(iter.line() as usize) {
                    let rect = graphene::Rect::new(
                        visible.x() as f32,
                        y as f32,
                        visible.width() as f32,
                        height as f32,
                    );
                    palette.paint_row(snapshot, row.kind, &rect);
                }
                if !iter.forward_line() {
                    break;
                }
            }
        }
    }

    #[derive(Default)]
    pub struct LineNumbers {
        pub column: Cell<Column>,
        pub layout: RefCell<Option<pango::Layout>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LineNumbers {
        const NAME: &'static str = "GsvnDiffLineNumbers";
        type Type = super::LineNumbers;
        type ParentType = sourceview5::GutterRenderer;
    }

    impl ObjectImpl for LineNumbers {}

    impl WidgetImpl for LineNumbers {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Vertical {
                return self.parent_measure(orientation, for_size);
            }
            let obj = self.obj();
            let (old, new) = obj
                .view()
                .downcast::<super::DiffView>()
                .map(|v| v.imp().max_numbers.get())
                .unwrap_or((0, 0));
            let max = match self.column.get() {
                Column::Old => old,
                Column::New => new,
            };
            let digits = max.max(1).ilog10() as usize + 1;
            let layout = obj.create_pango_layout(Some(&"0".repeat(digits.max(2))));
            let width = layout.pixel_size().0 + 2 * obj.xpad();
            (width, width, -1, -1)
        }
    }

    impl GutterRendererImpl for LineNumbers {
        fn begin(&self, lines: &sourceview5::GutterLines) {
            self.parent_begin(lines);
            self.layout
                .replace(Some(self.obj().create_pango_layout(None)));
        }

        fn end(&self) {
            self.layout.replace(None);
            self.parent_end();
        }

        fn snapshot_line(
            &self,
            snapshot: &gtk::Snapshot,
            lines: &sourceview5::GutterLines,
            line: u32,
        ) {
            let obj = self.obj();
            let Ok(view) = obj.view().downcast::<super::DiffView>() else {
                return;
            };
            let rows = view.imp().rows.borrow();
            let Some(row) = rows.get(line as usize) else {
                return;
            };
            let (y, height) =
                lines.line_yrange(line, sourceview5::GutterRendererAlignmentMode::Cell);
            let width = obj.width();
            let rect = graphene::Rect::new(0.0, y as f32, width as f32, height as f32);
            Palette::current().paint_row(snapshot, row.kind, &rect);

            let number = match self.column.get() {
                Column::Old => row.old_no,
                Column::New => row.new_no,
            };
            let Some(number) = number else { return };
            let layout = self.layout.borrow();
            let Some(layout) = layout.as_ref() else {
                return;
            };
            layout.set_text(&number.to_string());
            let (lw, lh) = layout.pixel_size();
            // Align with the first display line when wrapped.
            let (fy, fh) = lines.line_yrange(line, sourceview5::GutterRendererAlignmentMode::First);
            let x = width - obj.xpad() - lw;
            let ty = fy + (fh - lh) / 2;
            let mut color = obj.color();
            color.set_alpha(color.alpha() * 0.7);
            snapshot.save();
            snapshot.translate(&graphene::Point::new(x as f32, ty as f32));
            snapshot.append_layout(layout, &color);
            snapshot.restore();
        }
    }
}

glib::wrapper! {
    pub struct DiffView(ObjectSubclass<imp::DiffView>)
        @extends sourceview5::View, gtk::TextView, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Scrollable;
}

glib::wrapper! {
    pub struct LineNumbers(ObjectSubclass<imp::LineNumbers>)
        @extends sourceview5::GutterRenderer, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for DiffView {
    fn default() -> Self {
        Self::new()
    }
}

impl DiffView {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn source_buffer(&self) -> sourceview5::Buffer {
        self.buffer()
            .downcast()
            .expect("DiffView has a sourceview5::Buffer")
    }

    /// Add a gutter column showing old or new line numbers.
    pub fn add_line_numbers(&self, column: Column) {
        let renderer: LineNumbers = glib::Object::builder().property("xpad", 6).build();
        renderer.imp().column.set(column);
        let position = self.imp().renderers.borrow().len() as i32;
        sourceview5::prelude::ViewExt::gutter(self, gtk::TextWindowType::Left)
            .insert(&renderer, position);
        self.imp().renderers.borrow_mut().push(renderer);
    }

    /// Re-colour tags and repaint after a light/dark switch.
    fn apply_palette(&self) {
        let p = Palette::current();
        let table = self.buffer().tag_table();
        let set_bg = |name: &str, color: &gdk::RGBA| {
            if let Some(tag) = table.lookup(name) {
                tag.set_background_rgba(Some(color));
            }
        };
        set_bg(TAG_INTRA_REMOVED, &p.removed_strong);
        set_bg(TAG_INTRA_ADDED, &p.added_strong);
        if let Some(tag) = table.lookup(TAG_HEADER) {
            tag.set_foreground_rgba(Some(&p.header_fg));
        }
        if let Some(tag) = table.lookup(TAG_META) {
            tag.set_foreground_rgba(Some(&p.meta_fg));
        }
        self.queue_draw();
        for r in self.imp().renderers.borrow().iter() {
            r.queue_draw();
        }
    }

    /// Show `content` with syntax highlighting for `language`.
    pub fn set_content(&self, content: Rendered, language: Option<&sourceview5::Language>) {
        let Rendered { text, rows, spans } = content;
        let buffer = self.source_buffer();
        // No highlighting while the text goes in; it restarts lazily below.
        buffer.set_language(None);
        let imp = self.imp();
        imp.max_numbers.set(rows.iter().fold((0, 0), |(o, n), r| {
            (o.max(r.old_no.unwrap_or(0)), n.max(r.new_no.unwrap_or(0)))
        }));
        buffer.set_text(&text);
        drop(text);

        // Line tags for runs of heading/meta rows.
        let mut start = 0;
        while start < rows.len() {
            let kind = rows[start].kind;
            let mut end = start + 1;
            while end < rows.len() && rows[end].kind == kind {
                end += 1;
            }
            let tag = match kind {
                RowKind::Header => Some(TAG_HEADER),
                RowKind::Meta => Some(TAG_META),
                RowKind::Section | RowKind::PropName => Some(TAG_BOLD),
                _ => None,
            };
            if let Some(tag) = tag
                && let Some(from) = buffer.iter_at_line(start as i32)
            {
                let to = buffer
                    .iter_at_line(end as i32)
                    .unwrap_or_else(|| buffer.end_iter());
                buffer.apply_tag_by_name(tag, &from, &to);
            }
            start = end;
        }
        for span in &spans {
            let tag = match rows.get(span.row as usize).map(|r| r.kind) {
                Some(RowKind::Removed) => TAG_INTRA_REMOVED,
                Some(RowKind::Added) => TAG_INTRA_ADDED,
                _ => continue,
            };
            let line = span.row as i32;
            if let (Some(from), Some(to)) = (
                buffer.iter_at_line_offset(line, span.start as i32),
                buffer.iter_at_line_offset(line, span.end as i32),
            ) {
                buffer.apply_tag_by_name(tag, &from, &to);
            }
        }
        imp.rows.replace(rows);

        buffer.set_language(language);
        buffer.set_highlight_syntax(language.is_some());
        buffer.place_cursor(&buffer.start_iter());
        for r in imp.renderers.borrow().iter() {
            r.queue_resize();
            r.queue_draw();
        }
        self.queue_draw();
    }

    /// Empty the view (frees the text of the previous file).
    pub fn clear(&self) {
        self.imp().rows.replace(Vec::new());
        self.source_buffer().set_text("");
    }
}
