//! Breadcrumb address bar that turns into an editable path entry when
//! clicked (or on Ctrl+L), with suggestions while typing and Tab completion.

use crate::archive;
use gtk::{gdk, gio, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub struct AddressBar {
    pub widget: gtk::Stack,
    crumbs: gtk::Box,
    entry: gtk::Entry,
    location: RefCell<Option<gio::File>>,
    on_navigate: Rc<dyn Fn(gio::File)>,
    popover: gtk::Popover,
    list: gtk::ListBox,
    /// Full entry text of each suggestion row, and whether it's a folder.
    suggestions: RefCell<Vec<Name>>,
    /// The last folder read for suggestions, so typing doesn't re-read it.
    listing: RefCell<Option<(PathBuf, Vec<Name>)>>,
    /// Set while we change the text ourselves, so it isn't treated as typing.
    quiet: Cell<bool>,
}

/// A file name (or path text) and whether it's a folder.
type Name = (String, bool);

const MAX_SUGGESTIONS: usize = 50;

impl AddressBar {
    /// `on_navigate` is called for crumb clicks; `on_submit` with the text
    /// typed into the entry.
    pub fn new(on_navigate: Rc<dyn Fn(gio::File)>, on_submit: Rc<dyn Fn(String)>) -> Rc<Self> {
        let crumbs = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&crumbs)
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .build();
        // Keep the deepest folder visible when the path is long.
        // (Deferred: the viewport re-applies its old value after "changed".)
        scroller.hadjustment().connect_changed(|adj| {
            let adj = adj.clone();
            glib::idle_add_local_once(move || adj.set_value(adj.upper()));
        });
        let frame = gtk::Box::builder().css_classes(["vq-crumbs"]).hexpand(true).build();
        frame.append(&scroller);
        let filler = gtk::Box::builder().hexpand(true).build();
        crumbs.append(&filler);

        // A large natural width makes the header bar give the address bar
        // all the space it has (it allocates title widgets their natural size).
        let entry = gtk::Entry::builder()
            .hexpand(true)
            .max_width_chars(400)
            .input_purpose(gtk::InputPurpose::Url)
            .build();
        let widget = gtk::Stack::builder().hexpand(true).hhomogeneous(true).build();
        widget.add_named(&frame, Some("crumbs"));
        widget.add_named(&entry, Some("entry"));

        // Not focusable and not modal, so typing stays in the entry.
        let list = gtk::ListBox::builder().css_classes(["navigation-sidebar"]).focusable(false).build();
        let popover = gtk::Popover::builder()
            .child(
                &gtk::ScrolledWindow::builder()
                    .child(&list)
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .propagate_natural_height(true)
                    .max_content_height(320)
                    .build(),
            )
            .autohide(false)
            .has_arrow(false)
            .can_focus(false)
            .position(gtk::PositionType::Bottom)
            .halign(gtk::Align::Start)
            .build();
        popover.set_parent(&entry);

        let bar = Rc::new(AddressBar {
            widget,
            crumbs,
            entry,
            location: RefCell::new(None),
            on_navigate,
            popover,
            list,
            suggestions: RefCell::default(),
            listing: RefCell::default(),
            quiet: Cell::new(false),
        });

        let w = Rc::downgrade(&bar);
        bar.entry.connect_changed(move |_| {
            if let Some(bar) = w.upgrade()
                && !bar.quiet.get()
            {
                bar.suggest();
            }
        });
        let w = Rc::downgrade(&bar);
        bar.list.connect_row_activated(move |_, row| {
            if let Some(bar) = w.upgrade() {
                bar.accept(row.index(), true);
            }
        });

        // Clicking the empty part of the crumb bar switches to editing.
        let click = gtk::GestureClick::new();
        let w = Rc::downgrade(&bar);
        click.connect_released(move |g, _, x, y| {
            let Some(bar) = w.upgrade() else { return };
            let picked = g.widget().unwrap().pick(x, y, gtk::PickFlags::DEFAULT);
            let on_button = std::iter::successors(picked, |p| p.parent()).any(|p| p.is::<gtk::Button>());
            if !on_button {
                bar.edit();
            }
        });
        frame.add_controller(click);

        let w = Rc::downgrade(&bar);
        bar.entry.connect_activate(move |e| {
            let text = e.text().to_string();
            if let Some(bar) = w.upgrade() {
                bar.show_crumbs();
            }
            on_submit(text);
        });

        // Captured, so Up/Down reach the suggestions rather than the entry.
        let keys = gtk::EventControllerKey::builder().propagation_phase(gtk::PropagationPhase::Capture).build();
        let w = Rc::downgrade(&bar);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(bar) = w.upgrade() else { return glib::Propagation::Proceed };
            let open = bar.popover.is_visible();
            let selected = bar.list.selected_row().map(|r| r.index());
            match key {
                gdk::Key::Escape if open => bar.hide_suggestions(),
                gdk::Key::Escape => bar.show_crumbs(),
                gdk::Key::Down | gdk::Key::KP_Down if open => bar.move_selection(1),
                gdk::Key::Up | gdk::Key::KP_Up if open => bar.move_selection(-1),
                gdk::Key::Return | gdk::Key::KP_Enter if open && selected.is_some() => bar.accept(selected.unwrap(), true),
                gdk::Key::Right | gdk::Key::KP_Right if open && selected.is_some() => bar.accept(selected.unwrap(), false),
                gdk::Key::Tab => match selected {
                    Some(i) if open => bar.accept(i, false),
                    _ => bar.complete(),
                },
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        bar.entry.add_controller(keys);

        let focus = gtk::EventControllerFocus::new();
        let w = Rc::downgrade(&bar);
        focus.connect_leave(move |_| {
            if let Some(bar) = w.upgrade() {
                bar.show_crumbs();
            }
        });
        bar.entry.add_controller(focus);
        bar
    }

    pub fn edit(&self) {
        let text = self
            .location
            .borrow()
            .as_ref()
            .map(|f| match f.path() {
                Some(p) => p.display().to_string(),
                None if archive::is_member(f) => f.parse_name().to_string(),
                None => f.uri().to_string(),
            })
            .unwrap_or_default();
        self.set_text_quietly(&text);
        self.widget.set_visible_child_name("entry");
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
    }

    fn show_crumbs(&self) {
        self.hide_suggestions();
        self.widget.set_visible_child_name("crumbs");
    }

    fn set_text_quietly(&self, text: &str) {
        self.quiet.set(true);
        self.entry.set_text(text);
        self.quiet.set(false);
    }

    /// Show the names in the typed folder that start with the last typed
    /// component (ignoring case), folders first.
    fn suggest(&self) {
        let text = self.entry.text().to_string();
        let split = text.rfind('/').map_or(0, |i| i + 1);
        let (typed_dir, prefix) = text.split_at(split);
        let dir = match typed_dir.strip_prefix('~') {
            Some(rest) => PathBuf::from(format!("{}{rest}", glib::home_dir().display())),
            None if typed_dir.starts_with('/') => PathBuf::from(typed_dir),
            None if text.contains("://") => return self.hide_suggestions(),
            None => match self.location.borrow().as_ref().and_then(|l| l.path()) {
                Some(cwd) => cwd.join(typed_dir),
                None => return self.hide_suggestions(),
            },
        };
        if text.is_empty() || !dir.is_dir() {
            return self.hide_suggestions();
        }
        if self.listing.borrow().as_ref().is_none_or(|(d, _)| d != &dir) {
            let mut names: Vec<Name> = std::fs::read_dir(&dir)
                .map(|it| it.flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), e.path().is_dir())).collect())
                .unwrap_or_default();
            names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| crate::util::natural_cmp(&a.0, &b.0)));
            *self.listing.borrow_mut() = Some((dir, names));
        }
        let lower = prefix.to_lowercase();
        let found: Vec<Name> = self
            .listing
            .borrow()
            .iter()
            .flat_map(|(_, names)| names)
            .filter(|(n, _)| (prefix.starts_with('.') || !n.starts_with('.')) && n.to_lowercase().starts_with(&lower))
            .filter(|(n, _)| n != prefix)
            .take(MAX_SUGGESTIONS)
            .map(|(n, is_dir)| (format!("{typed_dir}{n}"), *is_dir))
            .collect();
        if found.is_empty() {
            return self.hide_suggestions();
        }
        self.list.remove_all();
        for (full, is_dir) in &found {
            let name = &full[typed_dir.len()..];
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.append(&gtk::Image::from_icon_name(if *is_dir { "folder-symbolic" } else { "text-x-generic-symbolic" }));
            row.append(&gtk::Label::builder().label(name).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build());
            self.list.append(&gtk::ListBoxRow::builder().child(&row).focusable(false).build());
        }
        *self.suggestions.borrow_mut() = found;
        self.popover.set_size_request(self.entry.width(), -1);
        if self.popover.is_visible() {
            // Re-measure so the popup shrinks when fewer names match.
            self.popover.present();
        } else {
            self.popover.popup();
        }
    }

    fn hide_suggestions(&self) {
        self.popover.popdown();
        self.list.unselect_all();
    }

    fn move_selection(&self, by: i32) {
        let n = self.suggestions.borrow().len() as i32;
        let next = match self.list.selected_row() {
            Some(r) => r.index() + by,
            None if by > 0 => 0,
            None => n - 1,
        };
        match self.list.row_at_index(next).filter(|_| (0..n).contains(&next)) {
            Some(row) => {
                self.list.select_row(Some(&row));
                // Keep the highlighted row scrolled into view.
                if let (Some(bounds), Some(adj)) = (
                    row.compute_bounds(&self.list),
                    self.popover.child().and_downcast::<gtk::ScrolledWindow>().map(|s| s.vadjustment()),
                ) {
                    let (top, bottom) = (bounds.y() as f64, (bounds.y() + bounds.height()) as f64);
                    if top < adj.value() {
                        adj.set_value(top);
                    } else if bottom > adj.value() + adj.page_size() {
                        adj.set_value(bottom - adj.page_size());
                    }
                }
            }
            None => self.list.unselect_all(),
        }
    }

    /// Put suggestion `i` in the entry. `go` opens it; otherwise a folder
    /// gets a trailing "/" and its contents are suggested next.
    fn accept(&self, i: i32, go: bool) {
        let Some((full, is_dir)) = self.suggestions.borrow().get(i as usize).cloned() else { return };
        if go {
            self.set_text_quietly(&full);
            self.entry.emit_activate();
            return;
        }
        let text = if is_dir { format!("{full}/") } else { full };
        self.set_text_quietly(&text);
        self.entry.grab_focus_without_selecting();
        self.entry.set_position(-1);
        self.suggest();
    }

    pub fn set_location(&self, file: &gio::File, search: Option<&str>) {
        *self.location.borrow_mut() = Some(file.clone());
        while let Some(child) = self.crumbs.first_child() {
            self.crumbs.remove(&child);
        }
        let mut parts: Vec<(String, Option<&str>, gio::File)> = Vec::new();
        // Inside an archive: crumbs for the real folder holding it, then the
        // archive and the folders within.
        let mut inner = Vec::new();
        let mut local = file.clone();
        while archive::is_member(&local) {
            let Some(up) = local.parent() else { break };
            inner.push(std::mem::replace(&mut local, up));
        }
        if let Some(path) = local.path() {
            let home = glib::home_dir();
            let (mut cur, rest, root_label, root_icon): (PathBuf, &Path, &str, &str) = if path.starts_with(&home) {
                (home.clone(), path.strip_prefix(&home).unwrap(), "Home", "user-home-symbolic")
            } else {
                ("/".into(), path.strip_prefix("/").unwrap_or(&path), "", "drive-harddisk-symbolic")
            };
            parts.push((root_label.into(), Some(root_icon), gio::File::for_path(&cur)));
            for comp in rest.components() {
                cur.push(comp);
                parts.push((comp.as_os_str().to_string_lossy().into_owned(), None, gio::File::for_path(&cur)));
            }
            for f in inner.into_iter().rev() {
                let name = f.basename().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
                parts.push((name, None, f));
            }
        } else {
            parts.push((file.uri().to_string(), Some("folder-remote-symbolic"), file.clone()));
        }

        for (i, (label, icon, target)) in parts.into_iter().enumerate() {
            if i > 0 {
                self.crumbs.append(&gtk::Image::builder().icon_name("go-next-symbolic").css_classes(["dim-label"]).pixel_size(12).build());
            }
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            if let Some(icon) = icon {
                content.append(&gtk::Image::from_icon_name(icon));
            }
            if !label.is_empty() {
                content.append(&gtk::Label::new(Some(&label)));
            }
            let button = gtk::Button::builder().child(&content).css_classes(["flat"]).build();
            let nav = self.on_navigate.clone();
            button.connect_clicked(move |_| nav(target.clone()));
            self.crumbs.append(&button);
        }
        if let Some(q) = search {
            self.crumbs.append(&gtk::Image::builder().icon_name("go-next-symbolic").css_classes(["dim-label"]).pixel_size(12).build());
            self.crumbs.append(&gtk::Label::builder().label(format!("Search results for “{q}”")).margin_start(6).css_classes(["dim-label"]).build());
        }
        self.crumbs.append(&gtk::Box::builder().hexpand(true).build());
        self.show_crumbs();
    }

    /// Complete the last path component to the longest common prefix.
    fn complete(&self) {
        let text = self.entry.text().to_string();
        let expanded = match text.strip_prefix('~') {
            Some(rest) => format!("{}{rest}", glib::home_dir().display()),
            None => text.clone(),
        };
        let (dir, prefix) = match expanded.rfind('/') {
            Some(i) => (&expanded[..=i], &expanded[i + 1..]),
            None => return,
        };
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut matches: Vec<(String, bool)> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                (name.starts_with(prefix) && (prefix.starts_with('.') || !name.starts_with('.')))
                    .then(|| (name, e.path().is_dir()))
            })
            .collect();
        if matches.is_empty() {
            self.entry.error_bell();
            return;
        }
        matches.sort();
        let first = &matches[0].0;
        let mut common = first.len();
        for (m, _) in &matches[1..] {
            common = common.min(first.bytes().zip(m.bytes()).take_while(|(a, b)| a == b).count());
        }
        while !first.is_char_boundary(common) {
            common -= 1;
        }
        let mut completed = format!("{dir}{}", &first[..common]);
        if matches.len() == 1 && matches[0].1 {
            completed.push('/');
        }
        let shown = if text.starts_with('~') {
            completed.replacen(&glib::home_dir().display().to_string(), "~", 1)
        } else {
            completed
        };
        self.set_text_quietly(&shown);
        self.entry.set_position(-1);
        self.suggest();
    }
}
