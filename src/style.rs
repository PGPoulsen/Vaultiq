//! Stylesheets: the built-in one, plus an optional user stylesheet at
//! `~/.config/vaultiq/style.css` that overrides it and reloads on save.

use crate::config::ColorScheme;
use gtk::{gdk, gio, glib, prelude::*};
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

thread_local! {
    /// Keeps the user provider and its file monitor alive.
    static USER: RefCell<Option<(gtk::CssProvider, gio::FileMonitor)>> = const { RefCell::new(None) };
    static PENDING_RELOAD: RefCell<Option<glib::SourceId>> = const { RefCell::new(None) };
}

/// Apply the configured light/dark preference to every window.
pub fn apply_color_scheme() {
    let scheme = match crate::config::with(|c| c.color_scheme) {
        ColorScheme::System => adw::ColorScheme::Default,
        ColorScheme::Light => adw::ColorScheme::ForceLight,
        ColorScheme::Dark => adw::ColorScheme::ForceDark,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

pub fn user_css_path() -> PathBuf {
    glib::user_config_dir().join("vaultiq").join("style.css")
}

/// Surface colours for light mode. The desktop's `~/.config/gtk-4.0/gtk.css` pins the dark surfaces for
/// every libadwaita app, which would keep Vaultiq dark after Alt+Shift+D switches it to light.
const LIGHT_SURFACES: &str = "\
@define-color window_bg_color #f7f3f5;
@define-color window_fg_color #2a2338;
@define-color view_bg_color #ffffff;
@define-color view_fg_color #2a2338;
@define-color headerbar_bg_color #efe9ed;
@define-color headerbar_fg_color #2a2338;
@define-color headerbar_border_color #d9d0d6;
@define-color headerbar_backdrop_color #f7f3f5;
@define-color card_bg_color #ffffff;
@define-color card_fg_color #2a2338;
@define-color popover_bg_color #ffffff;
@define-color popover_fg_color #2a2338;
@define-color dialog_bg_color #f7f3f5;
@define-color dialog_fg_color #2a2338;
@define-color sidebar_bg_color #ece6ea;
@define-color sidebar_fg_color #2a2338;
@define-color sidebar_backdrop_color #f1ecef;
@define-color sidebar_border_color #d9d0d6;
@define-color accent_color #8d5a86;
@define-color destructive_color #c0453f;
@define-color success_color #4f7a21;
@define-color warning_color #b86e12;
@define-color error_color #c0453f;
";

/// Apply `LIGHT_SURFACES` only while the app is in light mode.
fn install_light_surfaces(display: &gdk::Display) {
    let provider = gtk::CssProvider::new();
    // One step above USER, so it beats the pinned dark colours.
    gtk::style_context_add_provider_for_display(display, &provider, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);
    let sync = move |manager: &adw::StyleManager| provider.load_from_string(if manager.is_dark() { "" } else { LIGHT_SURFACES });
    let manager = adw::StyleManager::default();
    sync(&manager);
    manager.connect_dark_notify(sync);
}

pub fn install(display: &gdk::Display) {
    install_light_surfaces(display);
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
