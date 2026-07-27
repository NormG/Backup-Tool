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
        .application_id("com.normsplace.home-backup")
        .build();

    app.connect_activate(move |app| {
        // Hidden root window that anchors the app lifetime.
        let root = gtk4::ApplicationWindow::builder()
            .application(app)
            .default_width(0)
            .default_height(0)
            .visible(false)
            .build();

        match &config {
            Some(cfg) if cfg.installed => {
                // Already installed: open the manager directly.
                main_win::show(app, cfg.clone());
            }
            _ => {
                // First run: show the wizard.  When it completes, open the
                // manager in the same process instance.
                let app = app.clone();
                install::show(&root, move |completed_cfg| {
                    main_win::show(&app, completed_cfg);
                });
            }
        }
    });

    app.run_with_args::<glib::GString>(&[]);
}
