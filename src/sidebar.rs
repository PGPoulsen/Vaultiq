//! Quick access (home, XDG user folders, pinned folders) and drives.

use crate::config;
use gtk::{gdk, gio, glib, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;

/// Called with (location, open in new tab).
pub type OpenFn = Rc<dyn Fn(gio::File, bool)>;

enum Target {
    Header,
    File { file: gio::File, pinned: bool },
    Volume(gio::Volume),
}

pub struct Sidebar {
    pub widget: gtk::ScrolledWindow,
    list: gtk::ListBox,
    entries: RefCell<Vec<Target>>,
    monitor: gio::VolumeMonitor,
    on_open: OpenFn,
}

impl Sidebar {
    pub fn new(on_open: OpenFn) -> Rc<Self> {
        let list = gtk::ListBox::builder().css_classes(["navigation-sidebar"]).build();
        let widget = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(420)
            .build();
        let sb = Rc::new(Sidebar {
            widget,
            list,
            entries: RefCell::default(),
            monitor: gio::VolumeMonitor::get(),
            on_open,
        });

        let w = Rc::downgrade(&sb);
        sb.list.connect_row_activated(move |_, row| {
            if let Some(sb) = w.upgrade() {
                sb.activate(row.index() as usize, false);
            }
        });

        let click = gtk::GestureClick::builder().button(0).build();
        let w = Rc::downgrade(&sb);
        click.connect_pressed(move |g, _, x, y| {
            let Some(sb) = w.upgrade() else { return };
            let Some(row) = sb.list.row_at_y(y as i32) else { return };
            match g.current_button() {
                gdk::BUTTON_MIDDLE => sb.activate(row.index() as usize, true),
                gdk::BUTTON_SECONDARY => sb.context_menu(&row, x, y),
                _ => return,
            }
            g.set_state(gtk::EventSequenceState::Claimed);
        });
        sb.list.add_controller(click);

        macro_rules! rebuild_on {
            ($($sig:ident),*) => {$(
                let w = Rc::downgrade(&sb);
                sb.monitor.$sig(move |_, _| {
                    if let Some(sb) = w.upgrade() {
                        sb.rebuild();
                    }
                });
            )*};
        }
        rebuild_on!(
            connect_mount_added,
            connect_mount_removed,
            connect_mount_changed,
            connect_volume_added,
            connect_volume_removed,
            connect_volume_changed
        );
        sb.rebuild();
        sb
    }

    pub fn rebuild(self: &Rc<Self>) {
        self.list.remove_all();
        self.entries.borrow_mut().clear();

        self.header("Quick access");
        let home = glib::home_dir();
        self.item("Home", &gio::ThemedIcon::new("user-home-symbolic").upcast(), Target::File { file: gio::File::for_path(&home), pinned: false }, None);
        for (dir, icon) in [
            (glib::UserDirectory::Desktop, "user-desktop-symbolic"),
            (glib::UserDirectory::Documents, "folder-documents-symbolic"),
            (glib::UserDirectory::Downloads, "folder-download-symbolic"),
            (glib::UserDirectory::Music, "folder-music-symbolic"),
            (glib::UserDirectory::Pictures, "folder-pictures-symbolic"),
            (glib::UserDirectory::Videos, "folder-videos-symbolic"),
        ] {
            let Some(path) = glib::user_special_dir(dir).filter(|p| *p != home && p.is_dir()) else { continue };
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            self.item(&name, &gio::ThemedIcon::new(icon).upcast(), Target::File { file: gio::File::for_path(&path), pinned: false }, None);
        }
        for path in config::with(|c| c.pinned.clone()) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
            self.item(&name, &gio::ThemedIcon::new("folder-symbolic").upcast(), Target::File { file: gio::File::for_path(&path), pinned: true }, None);
        }

        self.header("Drives");
        self.item("File System", &gio::ThemedIcon::new("drive-harddisk-symbolic").upcast(), Target::File { file: gio::File::for_path("/"), pinned: false }, None);
        for mount in self.monitor.mounts() {
            if mount.is_shadowed() {
                continue;
            }
            let eject = mount.can_eject() || mount.can_unmount();
            let action = eject.then(|| {
                let b = gtk::Button::builder()
                    .icon_name("media-eject-symbolic")
                    .tooltip_text(if mount.can_eject() { "Eject" } else { "Unmount" })
                    .css_classes(["flat", "circular"])
                    .valign(gtk::Align::Center)
                    .build();
                let m = mount.clone();
                b.connect_clicked(move |b| unmount(&m, b));
                b
            });
            self.item(&mount.name(), &mount.symbolic_icon(), Target::File { file: mount.root(), pinned: false }, action);
        }
        for volume in self.monitor.volumes() {
            if volume.get_mount().is_none() {
                self.item(&volume.name(), &volume.symbolic_icon(), Target::Volume(volume.clone()), None);
            }
        }
    }

    fn header(&self, title: &str) {
        let label = gtk::Label::builder().label(title).xalign(0.0).css_classes(["vq-sidebar-header"]).build();
        let row = gtk::ListBoxRow::builder().child(&label).activatable(false).selectable(false).build();
        self.list.append(&row);
        self.entries.borrow_mut().push(Target::Header);
    }

    fn item(&self, name: &str, icon: &gio::Icon, target: Target, action: Option<gtk::Button>) {
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        b.append(&gtk::Image::from_gicon(icon));
        b.append(&gtk::Label::builder().label(name).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build());
        if let Some(a) = action {
            b.append(&a);
        }
        if let Target::File { file, .. } = &target {
            b.set_tooltip_text(file.path().map(|p| p.display().to_string()).as_deref());
        }
        self.list.append(&gtk::ListBoxRow::builder().child(&b).build());
        self.entries.borrow_mut().push(target);
    }

    fn activate(&self, index: usize, new_tab: bool) {
        let entries = self.entries.borrow();
        match entries.get(index) {
            Some(Target::File { file, .. }) => (self.on_open)(file.clone(), new_tab),
            Some(Target::Volume(volume)) => {
                let volume = volume.clone();
                let on_open = self.on_open.clone();
                let op = gtk::MountOperation::new(self.widget.root().and_downcast_ref::<gtk::Window>());
                volume.clone().mount(gio::MountMountFlags::NONE, Some(&op), None::<&gio::Cancellable>, move |res| {
                    match res {
                        Ok(()) => {
                            if let Some(m) = volume.get_mount() {
                                on_open(m.root(), new_tab);
                            }
                        }
                        Err(e) => eprintln!("vaultiq: mount failed: {}", e.message()),
                    }
                });
            }
            _ => {}
        }
    }

    fn context_menu(self: &Rc<Self>, row: &gtk::ListBoxRow, x: f64, y: f64) {
        let index = row.index() as usize;
        let (file, pinned) = match self.entries.borrow().get(index) {
            Some(Target::File { file, pinned }) => (file.clone(), *pinned),
            _ => return,
        };
        let popover = gtk::Popover::builder().has_arrow(false).halign(gtk::Align::Start).build();
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let button = |label: &str| {
            let b = gtk::Button::builder().label(label).css_classes(["flat"]).build();
            b.child().and_downcast::<gtk::Label>().unwrap().set_xalign(0.0);
            menu.append(&b);
            b
        };
        let (p, f, open) = (popover.clone(), file.clone(), self.on_open.clone());
        button("Open in new tab").connect_clicked(move |_| {
            p.popdown();
            open(f.clone(), true);
        });
        if pinned {
            let (p, w) = (popover.clone(), Rc::downgrade(self));
            button("Unpin from Quick access").connect_clicked(move |_| {
                p.popdown();
                let path = file.path();
                config::update(|c| c.pinned.retain(|x| Some(x) != path.as_ref()));
                if let Some(sb) = w.upgrade() {
                    sb.rebuild();
                }
            });
        }
        popover.set_child(Some(&menu));
        popover.set_parent(&self.list);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    /// Highlight the entry matching the active tab's location, if any.
    pub fn select_location(&self, location: &gio::File) {
        let idx = self.entries.borrow().iter().position(|t| matches!(t, Target::File { file, .. } if file.equal(location)));
        match idx.and_then(|i| self.list.row_at_index(i as i32)) {
            Some(row) => self.list.select_row(Some(&row)),
            None => self.list.unselect_all(),
        }
    }
}

fn unmount(mount: &gio::Mount, button: &gtk::Button) {
    let op = gtk::MountOperation::new(button.root().and_downcast_ref::<gtk::Window>());
    let cb = |res: Result<(), glib::Error>| {
        if let Err(e) = res {
            eprintln!("vaultiq: unmount failed: {}", e.message());
        }
    };
    if mount.can_eject() {
        mount.eject_with_operation(gio::MountUnmountFlags::NONE, Some(&op), None::<&gio::Cancellable>, cb);
    } else {
        mount.unmount_with_operation(gio::MountUnmountFlags::NONE, Some(&op), None::<&gio::Cancellable>, cb);
    }
}
