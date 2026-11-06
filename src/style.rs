//! Stylesheets: the built-in one, plus an optional user stylesheet at
//! `~/.config/vaultiq/style.css` that overrides it and reloads on save.

use gtk::{gdk, gio, glib, prelude::*};
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

thread_local! {
    /// Keeps the user provider and its file monitor alive.
    static USER: RefCell<Option<(gtk::CssProvider, gio::FileMonitor)>> = const { RefCell::new(None) };
    static PENDING_RELOAD: RefCell<Option<glib::SourceId>> = const { RefCell::new(None) };
}

pub fn user_css_path() -> PathBuf {
    glib::user_config_dir().join("vaultiq").join("style.css")
}

pub fn install(display: &gdk::Display) {
    let builtin = gtk::CssProvider::new();
    builtin.load_from_string(include_str!("../data/style.css"));
    gtk::style_context_add_provider_for_display(display, &builtin, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

    // USER priority beats both libadwaita's theme and the built-in sheet.
    let user = gtk::CssProvider::new();
    user.connect_parsing_error(|_, section, error| {
        eprintln!("vaultiq: {}: {}", section.to_str(), error.message());
    });
    gtk::style_context_add_provider_for_display(display, &user, gtk::STYLE_PROVIDER_PRIORITY_USER);
    load_user(&user);

    let path = user_css_path();
    // The monitor watches the parent folder, which therefore has to exist.
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let monitor = match gio::File::for_path(&path).monitor_file(gio::FileMonitorFlags::WATCH_MOVES, None::<&gio::Cancellable>) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("vaultiq: can't watch {}: {}", path.display(), e.message());
            return;
        }
    };
    let p = user.clone();
    monitor.connect_changed(move |_, _, _, _| {
        // Editors save in several steps (truncate, write, rename); reload
        // once things have settled.
        let p = p.clone();
        PENDING_RELOAD.with(|pending| {
            if let Some(id) = pending.borrow_mut().take() {
                id.remove();
            }
            let id = glib::timeout_add_local_once(Duration::from_millis(150), move || {
                PENDING_RELOAD.with(|pending| pending.borrow_mut().take());
                load_user(&p);
            });
            *pending.borrow_mut() = Some(id);
        });
    });
    USER.with(|u| *u.borrow_mut() = Some((user, monitor)));
}

fn load_user(provider: &gtk::CssProvider) {
    let path = user_css_path();
    if path.exists() {
        // Loading by path makes parse errors report "style.css:LINE:COL".
        provider.load_from_path(&path);
    } else {
        provider.load_from_string("");
    }
}
