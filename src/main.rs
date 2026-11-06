mod addressbar;
mod clipboard;
mod config;
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
        .build();

    app.connect_startup(|app| {
        config::load();
        style::install(&gdk::Display::default().expect("no display"));
        window::set_accels(app);
    });
    app.connect_activate(|app| window::Explorer::open(app, Vec::new()));
    app.connect_open(|app, files, _| window::Explorer::open(app, files.to_vec()));
    app.run()
}
