//! Agent Viewer: a GNOME viewer for Microsoft Agent character files.

mod audio;
mod player;
mod speech;
mod stage;
mod window;

use std::path::PathBuf;

use adw::prelude::*;
use gtk::gio;
use gtk::glib;

const APP_ID: &str = "org.gnome.AgentViewer";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    app.connect_activate(|app| {
        window::build(app, None).present();
    });

    // Launched with file arguments, e.g. from a file manager or the shell.
    app.connect_open(|app, files, _hint| {
        let path: Option<PathBuf> = files.first().and_then(|f| f.path());
        window::build(app, path).present();
    });

    app.run()
}
