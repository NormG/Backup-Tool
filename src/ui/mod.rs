pub mod install;
pub mod main_win;

use gtk4::{glib, prelude::*};

use crate::config::Config;

/// Launch the GTK4 application.
///
/// If `config` is `Some` (i.e. already installed), the main window opens
/// immediately.  Otherwise the first-run install wizard is shown.
pub fn run_app(config: Option<Config>) {
    let app = gtk4::Application::builder()
        .application_id("com.normsplace.backup-tool")
        .build();

    app.connect_activate(move |app| {
        match &config {
            Some(cfg) if cfg.installed => {
                // Already installed: open the manager directly.
                main_win::show(app, cfg.clone());
            }
            _ => {
                // First run: the wizard window registers itself as an
                // ApplicationWindow so the app exits cleanly when it is closed.
                let app_clone = app.clone();
                install::show(app, move |completed_cfg| {
                    main_win::show(&app_clone, completed_cfg);
                });
            }
        }
    });

    app.run_with_args::<glib::GString>(&[]);
}
