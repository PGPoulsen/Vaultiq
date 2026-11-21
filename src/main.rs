mod addressbar;
mod archive;
mod clipboard;
mod config;
mod filemanager1;
mod fileops;
mod openwith;
mod preview;
mod properties;
mod search;
mod settings;
mod sidebar;
mod style;
mod tab;
mod thumbnails;
mod tree;
mod util;
mod views;
mod window;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

const APP_ID: &str = "dk.accuratech.Vaultiq";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        // When D-Bus starts us for FileManager1 (`--gapplication-service`),
        // stay up long enough for the call that triggered it to arrive.
        .inactivity_timeout(10_000)
        .build();

    app.connect_startup(|app| {
        config::load();
        archive::register();
        archive::clean_cache();
        style::install(&gdk::Display::default().expect("no display"));
        style::apply_color_scheme();
        use_simple_input_method();
        window::set_accels(app);
        filemanager1::register(app);
    });
    app.connect_activate(|app| window::Explorer::open(app, Vec::new()));
    app.connect_open(|app, files, _| window::Explorer::open(app, files.to_vec()));
    app.run()
}

/// GTK picks the Wayland text-input method by default. With no input method
/// (fcitx, ibus) running behind it, dead keys compose nothing, so "~" can't be
/// typed on e.g. a Danish layout. GTK's built-in method composes them. A
/// choice made through `GTK_IM_MODULE` or GTK settings is left alone.
fn use_simple_input_method() {
    let Some(settings) = gtk::Settings::default() else { return };
    let chosen = settings.gtk_im_module().is_some_and(|m| !m.is_empty());
    if !chosen && std::env::var_os("GTK_IM_MODULE").is_none() {
        settings.set_gtk_im_module(Some("gtk-im-context-simple"));
    }
}
