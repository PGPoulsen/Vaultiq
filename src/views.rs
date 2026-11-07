//! Cell factories for the three file views. Every cell widget carries the
//! per-item interactions (context menu, middle click, drag and drop) so the
//! tab can tell which item was targeted. Name cells also hold a hidden entry
//! for inline renaming.

use crate::tab::Tab;
use crate::{thumbnails, util};
use gtk::{gdk, gio, glib, prelude::*};
use std::rc::Weak;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Name,
    Date,
    Type,
    Size,
    Location,
    Row,
    Grid,
}

pub const CELL_CLASS: &str = "vq-cell";

pub fn factory(kind: CellKind, tab: Weak<Tab>) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let t = tab.clone();
    factory.connect_setup(move |_, obj| {
        let widget = build(kind);
        widget.add_css_class(CELL_CLASS);
        obj.set_property("child", &widget);
        attach_interactions(&widget, obj, t.clone());
        if let Some(entry) = name_entry(&widget) {
            attach_editor(&entry, t.clone());
            if let Some(tab) = t.upgrade() {
                tab.register_name_cell(kind, &widget, obj);
            }
        }
    });
    factory.connect_bind(move |_, obj| {
        let Some(info) = item_info(obj) else { return };
        let Some(widget) = obj.property::<Option<gtk::Widget>>("child") else { return };
        // A recycled cell that was mid-rename now shows another file.
        if let (Some(entry), Some(tab)) = (name_entry(&widget), tab.upgrade())
            && WidgetExt::is_visible(&entry)
        {
            tab.cancel_rename_in(&entry);
        }
        bind(kind, &widget, &info, obj);
    });
    factory
}

/// The rename entry of a Name/Row/Grid cell (its third child).
pub fn name_entry(cell: &gtk::Widget) -> Option<gtk::Entry> {
    cell.first_child()?.next_sibling()?.next_sibling().and_downcast()
}

/// Enter confirms, Escape cancels, clicking elsewhere confirms (as in Explorer).
fn attach_editor(entry: &gtk::Entry, tab: Weak<Tab>) {
    let t = tab.clone();
    entry.connect_activate(move |_| {
        if let Some(tab) = t.upgrade() {
            tab.finish_rename(true, true);
        }
    });
    let keys = gtk::EventControllerKey::new();
    let t = tab.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key != gdk::Key::Escape {
            return glib::Propagation::Proceed;
        }
        if let Some(tab) = t.upgrade() {
            tab.finish_rename(false, true);
        }
        glib::Propagation::Stop
    });
    entry.add_controller(keys);
    let focus = gtk::EventControllerFocus::new();
    focus.connect_leave(move |_| {
        if let Some(tab) = tab.upgrade() {
            tab.finish_rename(true, false);
        }
    });
    entry.add_controller(focus);
}

pub fn item_info(obj: &glib::Object) -> Option<gio::FileInfo> {
    obj.property::<Option<glib::Object>>("item").and_downcast()
}

fn build(kind: CellKind) -> gtk::Widget {
    match kind {
        CellKind::Name | CellKind::Row => {
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            b.append(&gtk::Image::new());
            let label = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .hexpand(true)
                .build();
            b.append(&label);
            b.append(&gtk::Entry::builder().hexpand(true).visible(false).css_classes(["vq-rename"]).build());
            b.upcast()
        }
        CellKind::Grid => {
            let b = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(4)
                .width_request(96)
                .build();
            let image = gtk::Image::builder().pixel_size(64).build();
            b.append(&image);
            let label = gtk::Label::builder()
                .justify(gtk::Justification::Center)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .lines(2)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .max_width_chars(14)
                .build();
            b.append(&label);
            b.append(&gtk::Entry::builder().xalign(0.5).width_chars(12).visible(false).css_classes(["vq-rename"]).build());
            b.upcast()
        }
        CellKind::Size => gtk::Label::builder().xalign(1.0).build().upcast(),
        _ => gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build()
            .upcast(),
    }
}

fn bind(kind: CellKind, widget: &gtk::Widget, info: &gio::FileInfo, obj: &glib::Object) {
    match kind {
        CellKind::Name | CellKind::Row | CellKind::Grid => {
            let image = widget.first_child().and_downcast::<gtk::Image>().unwrap();
            let label = image.next_sibling().and_downcast::<gtk::Label>().unwrap();
            label.set_text(&info.display_name());
            widget.set_opacity(if util::is_hidden(info) { 0.6 } else { 1.0 });
            if kind == CellKind::Grid {
                thumbnails::apply(&image, info, obj);
            } else if let Some(icon) = info.symbolic_icon().or_else(|| info.icon()) {
                // Symbolic icons recolor with the theme (legible in dark mode).
                image.set_from_gicon(&icon);
            }
            if kind == CellKind::Grid {
                widget.set_tooltip_text(Some(&info.display_name()));
            }
        }
        CellKind::Date => set_label(widget, &util::format_time(info.modification_date_time())),
        CellKind::Type => set_label(widget, &util::type_description(info)),
        CellKind::Size => set_label(
            widget,
            &if util::is_dir(info) { String::new() } else { util::format_size(info.size() as u64) },
        ),
        CellKind::Location => {
            let parent = util::file_of(info).parent().and_then(|p| p.path());
            let text = parent.map(|p| p.display().to_string()).unwrap_or_default();
            set_label(widget, &text);
            widget.set_tooltip_text(Some(&text));
        }
    }
}

fn set_label(widget: &gtk::Widget, text: &str) {
    widget.downcast_ref::<gtk::Label>().unwrap().set_text(text);
}

fn attach_interactions(widget: &gtk::Widget, obj: &glib::Object, tab: Weak<Tab>) {
    let item = obj.downgrade();

    let click = gtk::GestureClick::builder().button(0).build();
    let (t, it) = (tab.clone(), item.clone());
    click.connect_pressed(move |g, _, x, y| {
        let (Some(tab), Some(obj)) = (t.upgrade(), it.upgrade()) else { return };
        let pos = obj.property::<u32>("position");
        match g.current_button() {
            gdk::BUTTON_SECONDARY => {
                g.set_state(gtk::EventSequenceState::Claimed);
                let widget = g.widget().unwrap();
                tab.item_context_menu(pos, &widget, x, y);
            }
            gdk::BUTTON_MIDDLE => {
                g.set_state(gtk::EventSequenceState::Claimed);
                tab.item_middle_click(pos);
            }
            _ => {}
        }
    });
    widget.add_controller(click);

    let drag = gtk::DragSource::builder()
        .actions(gdk::DragAction::COPY | gdk::DragAction::MOVE)
        .build();
    let (t, it) = (tab.clone(), item.clone());
    drag.connect_prepare(move |_, _, _| {
        let (tab, obj) = (t.upgrade()?, it.upgrade()?);
        let mut files = tab.drag_files(obj.property::<u32>("position"));
        if files.is_empty() {
            return None;
        }
        // Other apps can't read inside archives: hand them extracted copies.
        if files.iter().any(crate::archive::is_member) {
            files = files.iter().map(crate::archive::local_path).collect::<Result<Vec<_>, _>>().ok()?.into_iter().map(gio::File::for_path).collect();
        }
        Some(gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value()))
    });
    drag.connect_drag_begin(|src, _| {
        if let Some(w) = src.widget() {
            src.set_icon(Some(&gtk::WidgetPaintable::new(Some(&w))), 0, 0);
        }
    });
    widget.add_controller(drag);

    // Dropping onto a folder item moves/copies into that folder; anything
    // else falls through to the view's own drop target (the current folder).
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY | gdk::DragAction::MOVE);
    let it = item.clone();
    drop.connect_accept(move |_, d| {
        it.upgrade()
            .and_then(|o| item_info(&o))
            .is_some_and(|i| util::is_dir(&i))
            && d.formats().contains_type(gdk::FileList::static_type())
    });
    let (t, it) = (tab, item);
    drop.connect_drop(move |target, value, _, _| {
        let (Some(tab), Some(info)) = (t.upgrade(), it.upgrade().and_then(|o| item_info(&o))) else {
            return false;
        };
        let Ok(list) = value.get::<gdk::FileList>() else { return false };
        tab.drop_files(list.files(), util::file_of(&info), target.current_event_state());
        true
    });
    widget.add_controller(drop);
}
