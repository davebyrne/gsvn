//! The blame gutter: a `GtkSourceGutterRenderer` that shows `r123 author`
//! on the first line of each run of lines from the same revision, bands
//! alternate runs, and marks each line's age with a thin accent stripe.

use super::blame_model::{BlameModel, Slot};
use crate::common;
use gtk::{gdk, glib, graphene, pango};
use sourceview5::prelude::*;
use gtk::subclass::prelude::*;
use sourceview5::subclass::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// Shared between the viewer (which fills it) and the gutter (which draws it).
pub type SharedModel = Rc<RefCell<BlameModel>>;

/// Author column width bounds, in characters.
const MIN_AUTHOR_CHARS: usize = 6;
const MAX_AUTHOR_CHARS: usize = 14;
const STRIPE_WIDTH: f32 = 3.0;
const PAD_START: f32 = 8.0;
const PAD_END: f32 = 10.0;

mod imp {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    pub struct BlameGutter {
        pub model: RefCell<SharedModel>,
        pub layout: RefCell<Option<pango::Layout>>,
        pub on_activate: RefCell<Option<Box<dyn Fn(u32)>>>,
        /// Digits of the largest revision that can appear (the file's rev).
        pub rev_digits: Cell<usize>,
        /// Columns currently reserved for the author.
        pub author_chars: Cell<usize>,
        /// Blame range start, for tooltips of lines older than it.
        pub start_rev: Cell<Option<svn_backend::RevNum>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BlameGutter {
        const NAME: &'static str = "GsvnBlameGutter";
        type Type = super::BlameGutter;
        type ParentType = sourceview5::GutterRenderer;
    }

    impl ObjectImpl for BlameGutter {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            self.rev_digits.set(1);
            self.author_chars.set(MIN_AUTHOR_CHARS);
            obj.set_cursor_from_name(Some("pointer"));
            obj.set_has_tooltip(true);
            obj.connect_query_tooltip(|obj, x, y, _keyboard, tooltip| {
                let Some(line) = obj.line_at(f64::from(x), f64::from(y)) else {
                    return false;
                };
                match obj.tooltip_for(line) {
                    Some(text) => {
                        tooltip.set_text(Some(&text));
                        true
                    }
                    None => false,
                }
            });
        }
    }

    impl WidgetImpl for BlameGutter {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Vertical {
                return (0, 0, -1, -1);
            }
            let layout = self.obj().create_pango_layout(Some("0000000000"));
            let cw = (layout.pixel_size().0 as f32 / 10.0).max(1.0);
            // "r" + digits, a gap of two, the author.
            let cols = 1 + self.rev_digits.get() + 2 + self.author_chars.get();
            let w = (STRIPE_WIDTH + PAD_START + cols as f32 * cw + PAD_END).ceil() as i32;
            (w, w, -1, -1)
        }
    }

    impl GutterRendererImpl for BlameGutter {
        fn begin(&self, lines: &sourceview5::GutterLines) {
            self.parent_begin(lines);
            // Fresh per pass so font changes (style, zoom) are picked up.
            self.layout.replace(Some(self.obj().create_pango_layout(None)));
        }

        fn query_activatable(&self, iter: &gtk::TextIter, _area: &gdk::Rectangle) -> bool {
            self.model.borrow().borrow().commit(iter.line() as usize).is_some()
        }

        fn activate(
            &self,
            iter: &gtk::TextIter,
            _area: &gdk::Rectangle,
            _button: u32,
            _state: gdk::ModifierType,
            _n_presses: i32,
        ) {
            if let Some(cb) = self.on_activate.borrow().as_ref() {
                cb(iter.line() as u32);
            }
        }

        fn snapshot_line(&self, snapshot: &gtk::Snapshot, lines: &sourceview5::GutterLines, line: u32) {
            self.obj().draw_line(snapshot, lines, line);
        }
    }
}

glib::wrapper! {
    pub struct BlameGutter(ObjectSubclass<imp::BlameGutter>)
        @extends sourceview5::GutterRenderer, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl BlameGutter {
    pub fn new(model: SharedModel) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().model.replace(model);
        obj.set_alignment_mode(sourceview5::GutterRendererAlignmentMode::First);
        obj
    }

    /// Called with the 0-based line when a blamed line's cell is clicked.
    pub fn connect_line_activated(&self, f: impl Fn(u32) + 'static) {
        self.imp().on_activate.replace(Some(Box::new(f)));
    }

    /// Reserve room for revision numbers up to `max_rev`.
    pub fn set_max_rev(&self, max_rev: svn_backend::RevNum) {
        let digits = max_rev.max(1).ilog10() as usize + 1;
        if self.imp().rev_digits.replace(digits) != digits {
            self.queue_resize();
        }
    }

    pub fn set_start_rev(&self, start: Option<svn_backend::RevNum>) {
        self.imp().start_rev.set(start);
    }

    /// Redraw after the model changed; resizes if longer authors arrived.
    pub fn model_changed(&self) {
        let chars = self
            .imp()
            .model
            .borrow()
            .borrow()
            .max_author_chars()
            .clamp(MIN_AUTHOR_CHARS, MAX_AUTHOR_CHARS);
        if self.imp().author_chars.replace(chars) != chars {
            self.queue_resize();
        }
        self.queue_draw();
        // The gutter draws renderers in its own snapshot; make sure it reruns.
        if let Some(parent) = self.parent() {
            parent.queue_draw();
        }
    }

    fn layout(&self) -> pango::Layout {
        let imp = self.imp();
        if let Some(layout) = imp.layout.borrow().as_ref() {
            return layout.clone();
        }
        let layout = self.create_pango_layout(None);
        imp.layout.replace(Some(layout.clone()));
        layout
    }

    fn char_width(&self) -> f32 {
        let layout = self.layout();
        layout.set_text("0000000000");
        let (w, _) = layout.pixel_size();
        (w as f32 / 10.0).max(1.0)
    }

    /// 0-based buffer line at a point in this widget's coordinates.
    fn line_at(&self, x: f64, y: f64) -> Option<usize> {
        let view = self.view();
        let p = self.compute_point(&view, &graphene::Point::new(x as f32, y as f32))?;
        let (_, by) = view.window_to_buffer_coords(gtk::TextWindowType::Widget, p.x() as i32, p.y() as i32);
        let (iter, _) = view.line_at_y(by);
        Some(iter.line() as usize)
    }

    fn tooltip_for(&self, line: usize) -> Option<String> {
        let model = self.imp().model.borrow().clone();
        let model = model.borrow();
        match model.slot(line) {
            Slot::Pending => None,
            Slot::Old => Some(match self.imp().start_rev.get() {
                Some(start) => format!("Last changed before r{start}"),
                None => "Last changed before the blame range".to_owned(),
            }),
            Slot::Commit(_) => {
                let c = model.commit(line)?;
                let mut text = format!("r{} by {}", c.rev, c.author.as_deref().unwrap_or("(no author)"));
                if let Some(date) = c.date {
                    text.push_str(&format!(
                        "\n{} ({})",
                        common::format_datetime(date),
                        common::format_relative(date)
                    ));
                }
                text.push_str("\nClick for changes, log and more");
                Some(text)
            }
        }
    }

    fn draw_line(&self, snapshot: &gtk::Snapshot, lines: &sourceview5::GutterLines, line: u32) {
        let model = self.imp().model.borrow().clone();
        let model = model.borrow();
        let idx = line as usize;
        let slot = model.slot(idx);
        if slot == Slot::Pending {
            return;
        }
        let width = self.width() as f32;
        let fg = self.color();
        let (cell_y, cell_h) = lines.line_yrange(line, sourceview5::GutterRendererAlignmentMode::Cell);
        let (cell_y, cell_h) = (cell_y as f32, cell_h as f32);

        if model.band(idx) {
            let band = gdk::RGBA::new(fg.red(), fg.green(), fg.blue(), 0.06);
            snapshot.append_color(&band, &graphene::Rect::new(0.0, cell_y, width, cell_h));
        }

        let commit = model.commit(idx);
        if let Some(c) = commit {
            let mut accent = adw::StyleManager::default().accent_color_rgba();
            accent.set_alpha(0.15 + 0.75 * model.age(c.rev));
            snapshot.append_color(&accent, &graphene::Rect::new(0.0, cell_y, STRIPE_WIDTH, cell_h));
        }

        // Label the first line of a run, and the top visible line so the
        // revision of a run scrolled half out of view stays visible.
        if !(model.is_run_start(idx) || line == lines.first()) {
            return;
        }
        let (y, h) = lines.line_yrange(line, sourceview5::GutterRendererAlignmentMode::First);
        let cw = self.char_width();
        let layout = self.layout();
        let x0 = STRIPE_WIDTH + PAD_START;
        let rev_cols = 1 + self.imp().rev_digits.get();

        let draw = |text: &str, x: f32, alpha: f32| {
            layout.set_text(text);
            let (_, th) = layout.pixel_size();
            snapshot.save();
            snapshot.translate(&graphene::Point::new(x, y as f32 + (h - th) as f32 / 2.0));
            snapshot.append_layout(&layout, &gdk::RGBA::new(fg.red(), fg.green(), fg.blue(), alpha));
            snapshot.restore();
        };
        match commit {
            None => draw("…", x0, 0.55),
            Some(c) => {
                let rev = format!("r{}", c.rev);
                // Right-align the revision in its column.
                let pad = rev_cols.saturating_sub(rev.chars().count()) as f32 * cw;
                draw(&rev, x0 + pad, 0.9);
                let author = ellipsize(c.author.as_deref().unwrap_or("—"), self.imp().author_chars.get());
                draw(&author, x0 + (rev_cols + 2) as f32 * cw, 0.6);
            }
        }
    }
}

/// Cut `s` to at most `max` characters, ending in `…` when shortened.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::ellipsize;

    #[test]
    fn ellipsizing() {
        assert_eq!(ellipsize("alice", 6), "alice");
        assert_eq!(ellipsize("jürgen.schmidt", 8), "jürgen.…");
        assert_eq!(ellipsize("abc", 3), "abc");
    }
}
