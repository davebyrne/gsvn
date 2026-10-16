//! Column factories for the revision and changed-path lists.

use super::logic::first_line;
use super::objects::{PathItem, RevisionItem};
use crate::common;
use gtk::prelude::*;
use gtk::{gdk, glib};
use std::rc::Rc;
use svn_backend::{ChangeAction, NodeKind};

/// Called on a secondary click on a cell: `(row position, cell widget, x, y)`.
pub type ContextHandler = Rc<dyn Fn(u32, &gtk::Widget, f64, f64)>;

/// A column whose cells are `W` widgets bound to `T` items. Every cell
/// reports secondary clicks to `context`.
fn column<T, W>(
    title: &str,
    create: impl Fn() -> W + 'static,
    bind: impl Fn(&W, &T) + 'static,
    context: &ContextHandler,
) -> gtk::ColumnViewColumn
where
    T: IsA<glib::Object>,
    W: IsA<gtk::Widget>,
{
    let factory = gtk::SignalListItemFactory::new();
    let context = context.clone();
    factory.connect_setup(move |_, obj| {
        let Some(cell) = obj.downcast_ref::<gtk::ColumnViewCell>() else { return };
        let widget = create();
        let click = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        let weak_cell = cell.downgrade();
        let context = context.clone();
        click.connect_pressed(move |gesture, _, x, y| {
            let (Some(cell), Some(widget)) = (weak_cell.upgrade(), gesture.widget()) else { return };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            context(cell.position(), &widget, x, y);
        });
        widget.add_controller(click);
        cell.set_child(Some(&widget));
    });
    factory.connect_bind(move |_, obj| {
        let Some(cell) = obj.downcast_ref::<gtk::ColumnViewCell>() else { return };
        if let (Some(widget), Some(item)) = (cell.child().and_downcast::<W>(), cell.item().and_downcast::<T>()) {
            bind(&widget, &item);
        }
    });
    gtk::ColumnViewColumn::new(Some(title), Some(factory))
}

fn cell_label(ellipsize: gtk::pango::EllipsizeMode, xalign: f32) -> gtk::Label {
    gtk::Label::builder()
        .ellipsize(ellipsize)
        .xalign(xalign)
        .single_line_mode(true)
        .hexpand(true)
        .build()
}

pub fn revision_columns(view: &gtk::ColumnView, context: &ContextHandler) {
    let rev = column(
        "Revision",
        || {
            let label = cell_label(gtk::pango::EllipsizeMode::None, 0.0);
            label.add_css_class("numeric");
            label
        },
        |label: &gtk::Label, item: &RevisionItem| label.set_text(&item.entry().rev.to_string()),
        context,
    );
    rev.set_fixed_width(90);
    rev.set_resizable(true);

    let author = column(
        "Author",
        || cell_label(gtk::pango::EllipsizeMode::End, 0.0),
        |label: &gtk::Label, item: &RevisionItem| match &item.entry().author {
            Some(author) => {
                label.set_text(author);
                label.remove_css_class("dim-label");
            }
            None => {
                label.set_text("(no author)");
                label.add_css_class("dim-label");
            }
        },
        context,
    );
    author.set_fixed_width(130);
    author.set_resizable(true);

    let date = column(
        "Date",
        || cell_label(gtk::pango::EllipsizeMode::End, 0.0),
        |label: &gtk::Label, item: &RevisionItem| match item.entry().date {
            Some(ts) => {
                label.set_text(&common::format_relative(ts));
                label.set_tooltip_text(Some(&common::format_datetime(ts)));
            }
            None => {
                label.set_text("");
                label.set_tooltip_text(None);
            }
        },
        context,
    );
    date.set_fixed_width(140);
    date.set_resizable(true);

    let message = column(
        "Message",
        || cell_label(gtk::pango::EllipsizeMode::End, 0.0),
        |label: &gtk::Label, item: &RevisionItem| label.set_text(first_line(&item.entry().message)),
        context,
    );
    message.set_expand(true);
    message.set_resizable(true);

    for c in [rev, author, date, message] {
        view.append_column(&c);
    }
}

/// Style class and tooltip for a change action.
fn action_style(action: ChangeAction) -> (&'static str, &'static str) {
    match action {
        ChangeAction::Added => ("success", "Added"),
        ChangeAction::Modified => ("warning", "Modified"),
        ChangeAction::Deleted => ("error", "Deleted"),
        ChangeAction::Replaced => ("accent", "Replaced"),
    }
}

const ACTION_CLASSES: [&str; 4] = ["success", "warning", "error", "accent"];

pub fn path_columns(view: &gtk::ColumnView, context: &ContextHandler) {
    let action = column(
        "",
        || {
            let label = cell_label(gtk::pango::EllipsizeMode::None, 0.5);
            label.add_css_class("heading");
            label.add_css_class("monospace");
            label
        },
        |label: &gtk::Label, item: &PathItem| {
            let action = item.path().action;
            let (class, tooltip) = action_style(action);
            for c in ACTION_CLASSES {
                label.remove_css_class(c);
            }
            label.add_css_class(class);
            label.set_text(&action.letter().to_string());
            label.set_tooltip_text(Some(tooltip));
        },
        context,
    );
    action.set_fixed_width(36);

    let kind = column(
        "",
        || gtk::Image::builder().pixel_size(16).build(),
        |image: &gtk::Image, item: &PathItem| {
            let (icon, tooltip) = match item.path().kind {
                Some(NodeKind::Dir) => ("folder-symbolic", "Directory"),
                Some(NodeKind::File) => ("text-x-generic-symbolic", "File"),
                None => ("dialog-question-symbolic", "Unknown kind"),
            };
            image.set_icon_name(Some(icon));
            image.set_tooltip_text(Some(tooltip));
        },
        context,
    );
    kind.set_fixed_width(32);

    let path = column(
        "Path",
        || cell_label(gtk::pango::EllipsizeMode::Start, 0.0),
        |label: &gtk::Label, item: &PathItem| {
            let p = &item.path().path;
            label.set_text(p);
            label.set_tooltip_text(Some(p));
        },
        context,
    );
    path.set_expand(true);
    path.set_resizable(true);

    let copy = column(
        "Copied From",
        || {
            let label = cell_label(gtk::pango::EllipsizeMode::Start, 0.0);
            label.add_css_class("dim-label");
            label
        },
        |label: &gtk::Label, item: &PathItem| match &item.path().copy_from {
            Some((path, rev)) => {
                let text = format!("{path}@{rev}");
                label.set_tooltip_text(Some(&text));
                label.set_text(&text);
            }
            None => {
                label.set_text("");
                label.set_tooltip_text(None);
            }
        },
        context,
    );
    copy.set_fixed_width(260);
    copy.set_resizable(true);

    for c in [action, kind, path, copy] {
        view.append_column(&c);
    }
}
