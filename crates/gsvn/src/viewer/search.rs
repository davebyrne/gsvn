//! Ctrl+F search bar over the viewer's source buffer.

use gtk::glib;
use sourceview5::prelude::*;
use std::rc::Rc;

pub struct Search {
    pub bar: gtk::SearchBar,
    entry: gtk::SearchEntry,
    count: gtk::Label,
    view: sourceview5::View,
    context: sourceview5::SearchContext,
}

impl Search {
    pub fn new(view: &sourceview5::View, buffer: &sourceview5::Buffer) -> Rc<Self> {
        let settings = sourceview5::SearchSettings::builder().wrap_around(true).build();
        let context = sourceview5::SearchContext::new(buffer, Some(&settings));
        context.set_highlight(true);

        let entry = gtk::SearchEntry::builder()
            .placeholder_text("Find in file")
            .width_chars(28)
            .build();
        let count = gtk::Label::builder()
            .css_classes(["dim-label", "numeric"])
            .width_chars(12)
            .xalign(1.0)
            .build();
        let prev = gtk::Button::builder()
            .icon_name("go-up-symbolic")
            .tooltip_text("Previous Match (Shift+Enter)")
            .build();
        let next = gtk::Button::builder()
            .icon_name("go-down-symbolic")
            .tooltip_text("Next Match (Enter)")
            .build();
        let case = gtk::ToggleButton::builder()
            .icon_name("format-text-case-symbolic")
            .tooltip_text("Match Case")
            .build();
        let nav = gtk::Box::builder().css_classes(["linked"]).build();
        nav.append(&prev);
        nav.append(&next);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&entry);
        row.append(&count);
        row.append(&nav);
        row.append(&case);
        let bar = gtk::SearchBar::builder().child(&row).show_close_button(true).build();
        bar.connect_entry(&entry);

        let this = Rc::new(Self {
            bar,
            entry: entry.clone(),
            count,
            view: view.clone(),
            context: context.clone(),
        });

        case.bind_property("active", &settings, "case-sensitive").build();
        {
            let this = Rc::downgrade(&this);
            let settings = settings.clone();
            entry.connect_search_changed(move |entry| {
                let Some(this) = this.upgrade() else { return };
                let text = entry.text();
                settings.set_search_text((!text.is_empty()).then_some(text.as_str()));
                // Incremental: jump to the first match at or after the selection start.
                if !text.is_empty() {
                    let buffer = this.view.buffer();
                    let (start, _) = buffer.selection_bounds().unwrap_or_else(|| {
                        let it = buffer.iter_at_mark(&buffer.get_insert());
                        (it, it)
                    });
                    this.find_from(&start, true);
                }
                this.update_count();
            });
        }
        let weak = Rc::downgrade(&this);
        let go = move |forward: bool| {
            let weak = weak.clone();
            move || {
                if let Some(this) = weak.upgrade() {
                    this.step(forward);
                }
            }
        };
        {
            let f = go(true);
            entry.connect_activate(move |_| f());
            let f = go(true);
            entry.connect_next_match(move |_| f());
            let f = go(true);
            next.connect_clicked(move |_| f());
            let f = go(false);
            entry.connect_previous_match(move |_| f());
            let f = go(false);
            prev.connect_clicked(move |_| f());
        }
        {
            let f = go(false);
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(move |_, key, _, state| {
                if matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter)
                    && state.contains(gtk::gdk::ModifierType::SHIFT_MASK)
                {
                    f();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            entry.add_controller(keys);
        }
        {
            let this = Rc::downgrade(&this);
            entry.connect_stop_search(move |_| {
                if let Some(this) = this.upgrade() {
                    this.bar.set_search_mode(false);
                    this.view.grab_focus();
                }
            });
        }
        {
            let this = Rc::downgrade(&this);
            context.connect_occurrences_count_notify(move |_| {
                if let Some(this) = this.upgrade() {
                    this.update_count();
                }
            });
        }
        {
            let this = Rc::downgrade(&this);
            buffer.connect_mark_set(move |_, _, mark| {
                if mark.name().as_deref() == Some("insert")
                    && let Some(this) = this.upgrade()
                {
                    this.update_count();
                }
            });
        }
        this
    }

    /// Show the bar and focus the entry, seeded with the selection.
    pub fn open(&self) {
        let buffer = self.view.buffer();
        if let Some((start, end)) = buffer.selection_bounds()
            && start.line() == end.line()
        {
            self.entry.set_text(&buffer.text(&start, &end, false));
        }
        self.bar.set_search_mode(true);
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
    }

    #[cfg(debug_assertions)]
    pub fn set_text(&self, text: &str) {
        self.bar.set_search_mode(true);
        self.entry.set_text(text);
    }

    fn step(&self, forward: bool) {
        let buffer = self.view.buffer();
        let (start, end) = buffer.selection_bounds().unwrap_or_else(|| {
            let it = buffer.iter_at_mark(&buffer.get_insert());
            (it, it)
        });
        if forward {
            self.find_from(&end, true);
        } else {
            self.find_from(&start, false);
        }
    }

    fn find_from(&self, iter: &gtk::TextIter, forward: bool) {
        let found = if forward {
            self.context.forward(iter)
        } else {
            self.context.backward(iter)
        };
        if let Some((start, end, _wrapped)) = found {
            let buffer = self.view.buffer();
            buffer.select_range(&start, &end);
            let mark = buffer.get_insert();
            self.view.scroll_to_mark(&mark, 0.0, true, 0.5, 0.33);
        }
        self.update_count();
    }

    fn update_count(&self) {
        if self.context.settings().search_text().is_none() {
            self.count.set_text("");
            return;
        }
        let total = self.context.occurrences_count();
        let text = match total {
            n if n < 0 => String::new(),
            0 => "No matches".to_owned(),
            n => {
                let buffer = self.view.buffer();
                let pos = buffer
                    .selection_bounds()
                    .map(|(s, e)| self.context.occurrence_position(&s, &e))
                    .unwrap_or(0);
                if pos > 0 {
                    format!("{pos} of {n}")
                } else if n == 1 {
                    "1 match".to_owned()
                } else {
                    format!("{n} matches")
                }
            }
        };
        self.count.set_text(&text);
        if total == 0 {
            self.entry.add_css_class("error");
        } else {
            self.entry.remove_css_class("error");
        }
    }
}
