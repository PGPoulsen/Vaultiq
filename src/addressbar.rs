//! Breadcrumb address bar that turns into an editable path entry when
//! clicked (or on Ctrl+L), with Tab completion.

use crate::archive;
use gtk::{gdk, gio, glib, prelude::*};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub struct AddressBar {
    pub widget: gtk::Stack,
    crumbs: gtk::Box,
    entry: gtk::Entry,
    location: RefCell<Option<gio::File>>,
    on_navigate: Rc<dyn Fn(gio::File)>,
}

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

        let bar = Rc::new(AddressBar {
            widget,
            crumbs,
            entry,
            location: RefCell::new(None),
            on_navigate,
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

        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(&bar);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(bar) = w.upgrade() else { return glib::Propagation::Proceed };
            match key {
                gdk::Key::Escape => {
                    bar.show_crumbs();
                    glib::Propagation::Stop
                }
                gdk::Key::Tab => {
                    bar.complete();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
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
        self.entry.set_text(&text);
        self.widget.set_visible_child_name("entry");
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
    }

    fn show_crumbs(&self) {
        self.widget.set_visible_child_name("crumbs");
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
        self.entry.set_text(&shown);
        self.entry.set_position(-1);
    }
}
