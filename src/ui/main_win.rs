use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
};

use gtk4::{
    glib, prelude::*, Align, Box as GBox, Button, ComboBoxText, FileChooserAction,
    FileChooserDialog, Frame, Label, Notebook, Orientation, ResponseType, ScrolledWindow,
    SpinButton, Switch, TextView, WrapMode,
};
use crate::{backup, config::Config, systemd};

// ── Entry-point ───────────────────────────────────────────────────────────────

/// Build and show the main backup-manager window.
pub fn show(app: &gtk4::Application, config: Config) {
    let cfg = Rc::new(RefCell::new(config));

    let win = gtk4::ApplicationWindow::builder()
        .application(app)
        .title("Home Backup Manager")
        .default_width(720)
        .default_height(540)
        .build();

    let nb = Notebook::new();
    win.set_child(Some(&nb));

    // ── Tab 1 — Dashboard ─────────────────────────────────────────────────
    let (dash_page, status_lbl) = build_dashboard(Rc::clone(&cfg));
    nb.append_page(&dash_page, Some(&Label::new(Some("Dashboard"))));

    // ── Tab 2 — Schedule ──────────────────────────────────────────────────
    let sched_page = build_schedule(Rc::clone(&cfg));
    nb.append_page(&sched_page, Some(&Label::new(Some("Schedule"))));

    // ── Tab 3 — Excludes ──────────────────────────────────────────────────
    let excl_page = build_excludes(Rc::clone(&cfg));
    nb.append_page(&excl_page, Some(&Label::new(Some("Excludes"))));

    // ── Tab 4 — Settings (source / destination) ──────────────────────────────
    let settings_page = build_settings(Rc::clone(&cfg));
    nb.append_page(&settings_page, Some(&Label::new(Some("Source/Destination"))));

    // ── Tab 5 — Log ───────────────────────────────────────────────────────────
    let log_page = build_log();
    nb.append_page(&log_page, Some(&Label::new(Some("Log"))));

    // Refresh the dashboard status label whenever the dashboard tab is shown.
    nb.connect_switch_page(glib::clone!(
        #[weak]
        status_lbl,
        #[strong]
        cfg,
        move |_, _, page| {
            if page == 0 {
                status_lbl.set_text(&status_text(&cfg.borrow()));
            }
        }
    ));

    // Initial status.
    status_lbl.set_text(&status_text(&cfg.borrow()));

    win.present();
}

// ── Dashboard ─────────────────────────────────────────────────────────────────

fn build_dashboard(cfg: Rc<RefCell<Config>>) -> (GBox, Label) {
    let b = tab_box();

    b.append(
        &Label::builder()
            .label("Backup Status")
            .css_classes(vec!["title-2"])
            .halign(Align::Start)
            .build(),
    );

    let status_lbl = Label::builder()
        .wrap(true)
        .halign(Align::Start)
        .selectable(true)
        .build();
    status_lbl.add_css_class("monospace");
    b.append(&status_lbl);

    // ── Action buttons ────────────────────────────────────────────────────
    let sep = gtk4::Separator::new(Orientation::Horizontal);
    sep.set_margin_top(8);
    sep.set_margin_bottom(8);
    b.append(&sep);

    b.append(
        &Label::builder()
            .label("Run a backup now:")
            .halign(Align::Start)
            .css_classes(vec!["heading"])
            .build(),
    );

    let btn_row = GBox::new(Orientation::Horizontal, 8);

    let btn_auto = Button::builder()
        .label("Auto (smart)")
        .tooltip_text("Full on the configured day, incremental otherwise")
        .build();
    let btn_full = Button::builder()
        .label("Force Full")
        .css_classes(vec!["destructive-action"])
        .tooltip_text("Always create a full snapshot regardless of day")
        .build();
    let btn_inc = Button::builder()
        .label("Force Incremental")
        .tooltip_text("Always create a hardlinked incremental snapshot")
        .build();

    btn_row.append(&btn_auto);
    btn_row.append(&btn_full);
    btn_row.append(&btn_inc);
    b.append(&btn_row);

    let result_lbl = Label::builder()
        .wrap(true)
        .halign(Align::Start)
        .selectable(true)
        .build();
    b.append(&result_lbl);

    // Wire backup buttons.
    wire_backup_btn(
        &btn_auto,
        backup::BackupKind::Auto,
        Rc::clone(&cfg),
        result_lbl.clone(),
    );
    wire_backup_btn(
        &btn_full,
        backup::BackupKind::Full,
        Rc::clone(&cfg),
        result_lbl.clone(),
    );
    wire_backup_btn(
        &btn_inc,
        backup::BackupKind::Incremental,
        Rc::clone(&cfg),
        result_lbl.clone(),
    );

    // ── Timer toggle ──────────────────────────────────────────────────────
    let sep2 = gtk4::Separator::new(Orientation::Horizontal);
    sep2.set_margin_top(8);
    sep2.set_margin_bottom(8);
    b.append(&sep2);

    let timer_row = GBox::new(Orientation::Horizontal, 12);
    timer_row.append(
        &Label::builder()
            .label("Scheduled backups enabled")
            .halign(Align::Start)
            .hexpand(true)
            .build(),
    );
    let timer_sw = Switch::new();
    timer_sw.set_active(systemd::timer_is_active());
    timer_sw.set_valign(Align::Center);
    timer_row.append(&timer_sw);
    b.append(&timer_row);

    {
        let cfg = Rc::clone(&cfg);
        timer_sw.connect_state_set(move |_, state| {
            if state {
                let _ = systemd::update_timer(&cfg.borrow());
            } else {
                let _ = systemd::disable();
            }
            glib::Propagation::Proceed
        });
    }

    (b, status_lbl)
}

fn wire_backup_btn(
    btn: &Button,
    kind: backup::BackupKind,
    cfg: Rc<RefCell<Config>>,
    result_lbl: Label,
) {
    btn.connect_clicked(glib::clone!(
        #[strong]
        result_lbl,
        #[strong]
        cfg,
        move |b| {
            b.set_sensitive(false);
            result_lbl.set_text("⏳  Running backup — please wait…");
            let c = cfg.borrow().clone();

            // Spawn the blocking backup on a background thread.
            // Communicate the result back to the main thread via Arc<Mutex>.
            let result: Arc<Mutex<Option<anyhow::Result<String>>>> = Arc::new(Mutex::new(None));
            let result_thread = Arc::clone(&result);

            std::thread::spawn(move || {
                let res = backup::run(&c, kind);
                *result_thread.lock().unwrap() = Some(res);
            });

            // Poll every 200 ms on the main thread to pick up the result.
            glib::timeout_add_local(
                std::time::Duration::from_millis(200),
                glib::clone!(
                    #[strong]
                    result_lbl,
                    #[strong]
                    b,
                    move || {
                        let mut guard = result.lock().unwrap();
                        if let Some(res) = guard.take() {
                            match res {
                                Ok(s) => result_lbl.set_text(&s),
                                Err(e) => {
                                    result_lbl.set_text(&format!("❌  Backup failed:\n{e}"));
                                }
                            }
                            b.set_sensitive(true);
                            return glib::ControlFlow::Break;
                        }
                        glib::ControlFlow::Continue
                    }
                ),
            );
        }
    ));
}

fn status_text(cfg: &Config) -> String {
    let timer_active = if systemd::timer_is_active() {
        "active ✅"
    } else {
        "inactive ⚠"
    };
    let timer_line = systemd::timer_status_line();
    format!(
        "Source          : {}\n\
         Destination     : {}\n\
         Drive label     : {}\n\
         Full backup day : {}\n\
         Daily time      : {}\n\
         Retention       : {} days\n\
         Timer           : {}\n\
         {}",
        cfg.source_dir,
        cfg.dest_dir,
        cfg.drive_label.as_deref().unwrap_or("(not set)"),
        cfg.full_backup_day,
        cfg.backup_time,
        cfg.retention_days,
        timer_active,
        timer_line,
    )
}

// ── Schedule tab ──────────────────────────────────────────────────────────────

fn build_schedule(cfg: Rc<RefCell<Config>>) -> GBox {
    let b = tab_box();

    b.append(
        &Label::builder()
            .label("Backup Schedule")
            .css_classes(vec!["title-2"])
            .halign(Align::Start)
            .build(),
    );

    // Full backup day
    b.append(&field_label("Full backup day of week:"));
    let day_combo = ComboBoxText::new();
    for day in &[
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ] {
        day_combo.append_text(day);
    }
    {
        let stored_day = cfg.borrow().full_backup_day.clone();
        let idx = [
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
            "Sunday",
        ]
        .iter()
        .position(|&d| d.eq_ignore_ascii_case(&stored_day))
        .unwrap_or(0) as u32;
        day_combo.set_active(Some(idx));
    }
    b.append(&day_combo);

    // Time
    b.append(&field_label("Daily backup time (24-hour HH:MM):"));
    let (stored_h, stored_m) = cfg.borrow().backup_hm();
    let time_row = GBox::new(Orientation::Horizontal, 8);

    let adj_h = gtk4::Adjustment::new(f64::from(stored_h), 0.0, 23.0, 1.0, 1.0, 0.0);
    let hour_spin = SpinButton::new(Some(&adj_h), 1.0, 0);
    hour_spin.set_width_chars(3);
    time_row.append(&hour_spin);
    time_row.append(&Label::new(Some(":")));
    let adj_m = gtk4::Adjustment::new(f64::from(stored_m), 0.0, 59.0, 1.0, 5.0, 0.0);
    let min_spin = SpinButton::new(Some(&adj_m), 1.0, 0);
    min_spin.set_width_chars(3);
    time_row.append(&min_spin);
    b.append(&time_row);

    // Retention
    b.append(&field_label("Keep incremental snapshots for (days):"));
    let adj_ret = gtk4::Adjustment::new(
        f64::from(cfg.borrow().retention_days),
        1.0,
        365.0,
        1.0,
        7.0,
        0.0,
    );
    let ret_spin = SpinButton::new(Some(&adj_ret), 1.0, 0);
    b.append(&ret_spin);

    // Incremental period
    b.append(&field_label(
        "Run incremental backup every N days (1 = daily, 7 = weekly):",
    ));
    let adj_inc = gtk4::Adjustment::new(
        f64::from(cfg.borrow().incremental_every_n_days.max(1)),
        1.0,
        7.0,
        1.0,
        1.0,
        0.0,
    );
    let inc_spin = SpinButton::new(Some(&adj_inc), 1.0, 0);
    b.append(&inc_spin);

    // Save button
    let save_btn = Button::builder()
        .label("Save & Reload Timer")
        .css_classes(vec!["suggested-action"])
        .halign(Align::End)
        .margin_top(16)
        .build();
    let save_lbl = Label::builder()
        .halign(Align::Start)
        .build();

    {
        let cfg = Rc::clone(&cfg);
        let day_combo = day_combo.clone();
        let save_lbl = save_lbl.clone();
        save_btn.connect_clicked(move |_| {
            let day = day_combo
                .active_text()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "Monday".to_string());
            let h = hour_spin.value() as u8;
            let m = min_spin.value() as u8;
            let ret = ret_spin.value() as u32;
            let inc = inc_spin.value() as u32;
            {
                let mut c = cfg.borrow_mut();
                c.full_backup_day = day;
                c.backup_time = format!("{h:02}:{m:02}");
                c.retention_days = ret;
                c.incremental_every_n_days = inc;
            }
            match cfg.borrow().save() {
                Ok(()) => {}
                Err(e) => {
                    save_lbl.set_text(&format!("❌ Save failed: {e}"));
                    return;
                }
            }
            match systemd::update_timer(&cfg.borrow()) {
                Ok(()) => save_lbl.set_text("✅  Schedule saved and timer reloaded."),
                Err(e) => save_lbl
                    .set_text(&format!("⚠  Saved config but timer reload failed: {e}")),
            }
        });
    }
    b.append(&save_btn);
    b.append(&save_lbl);
    b
}

// ── Excludes tab ──────────────────────────────────────────────────────────────

fn build_excludes(cfg: Rc<RefCell<Config>>) -> GBox {
    let b = tab_box();

    b.append(
        &Label::builder()
            .label("Exclude Patterns")
            .css_classes(vec!["title-2"])
            .halign(Align::Start)
            .build(),
    );
    b.append(&field_label(
        "One rsync exclude pattern per line.  Changes take effect on the next backup.",
    ));

    let tv = TextView::builder()
        .monospace(true)
        .wrap_mode(WrapMode::None)
        .build();

    let text = cfg.borrow().excludes.join("\n");
    tv.buffer().set_text(&text);

    let sw = ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(240)
        .build();
    sw.set_child(Some(&tv));
    let frame = Frame::new(None);
    frame.set_child(Some(&sw));
    b.append(&frame);

    let btn_row = GBox::new(Orientation::Horizontal, 8);
    btn_row.set_halign(Align::End);
    btn_row.set_margin_top(8);

    let reset_btn = Button::with_label("Reset to defaults");
    let save_btn = Button::builder()
        .label("Save")
        .css_classes(vec!["suggested-action"])
        .build();
    btn_row.append(&reset_btn);
    btn_row.append(&save_btn);
    b.append(&btn_row);

    let save_lbl = Label::builder().halign(Align::Start).build();
    b.append(&save_lbl);

    // Reset to defaults.
    {
        let tv = tv.clone();
        reset_btn.connect_clicked(move |_| {
            let defaults = Config::default().excludes.join("\n");
            tv.buffer().set_text(&defaults);
        });
    }

    // Save.
    {
        let cfg = Rc::clone(&cfg);
        let save_lbl = save_lbl.clone();
        save_btn.connect_clicked(move |_| {
            let buf = tv.buffer();
            let text = buf.text(&buf.start_iter(), &buf.end_iter(), false);
            let excl: Vec<String> = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.to_string())
                .collect();
            cfg.borrow_mut().excludes = excl;
            match cfg.borrow().save() {
                Ok(()) => save_lbl.set_text("✅  Excludes saved."),
                Err(e) => save_lbl.set_text(&format!("❌  Save failed: {e}")),
            }
        });
    }

    b
}

// ── Settings tab (source & destination) ───────────────────────────────────────

fn build_settings(cfg: Rc<RefCell<Config>>) -> GBox {
    let b = tab_box();

    b.append(
        &Label::builder()
            .label("Source & Destination")
            .css_classes(vec!["title-2"])
            .halign(Align::Start)
            .build(),
    );
    b.append(&field_label(
        "Changes take effect on the next backup.  The drive UUID is preserved; \
         if you change the destination path make sure it is reachable.",
    ));

    // ── Source directory ────────────────────────────────────────────────
    b.append(&field_label("Back up from (source directory):"));
    let src_row = GBox::new(Orientation::Horizontal, 8);
    let src_entry = gtk4::Entry::builder()
        .text(&cfg.borrow().source_dir)
        .hexpand(true)
        .build();
    let src_browse = Button::with_label("Browse…");
    {
        let src_entry = src_entry.clone();
        src_browse.connect_clicked(move |btn| {
            let chooser = FileChooserDialog::builder()
                .title("Choose source directory")
                .action(FileChooserAction::SelectFolder)
                .build();
            chooser.add_button("Cancel", ResponseType::Cancel);
            chooser.add_button("Select", ResponseType::Accept);
            let src_entry = src_entry.clone();
            chooser.connect_response(move |dlg, resp| {
                if resp == ResponseType::Accept {
                    if let Some(f) = dlg.file().and_then(|f| f.path()) {
                        src_entry.set_text(&f.to_string_lossy());
                    }
                }
                dlg.close();
            });
            if let Some(w) = btn.root().and_then(|r| r.downcast::<gtk4::Window>().ok()) {
                chooser.set_transient_for(Some(&w));
            }
            chooser.present();
        });
    }
    src_row.append(&src_entry);
    src_row.append(&src_browse);
    b.append(&src_row);

    // ── Destination directory ─────────────────────────────────────────────
    b.append(&field_label("Backup destination path (on backup drive):"));
    let dest_row = GBox::new(Orientation::Horizontal, 8);
    let dest_entry = gtk4::Entry::builder()
        .text(&cfg.borrow().dest_dir)
        .hexpand(true)
        .build();
    let dest_browse = Button::with_label("Browse…");
    {
        let dest_entry = dest_entry.clone();
        dest_browse.connect_clicked(move |btn| {
            let chooser = FileChooserDialog::builder()
                .title("Choose backup destination")
                .action(FileChooserAction::SelectFolder)
                .build();
            chooser.add_button("Cancel", ResponseType::Cancel);
            chooser.add_button("Select", ResponseType::Accept);
            let dest_entry = dest_entry.clone();
            chooser.connect_response(move |dlg, resp| {
                if resp == ResponseType::Accept {
                    if let Some(f) = dlg.file().and_then(|f| f.path()) {
                        dest_entry.set_text(&f.to_string_lossy());
                    }
                }
                dlg.close();
            });
            if let Some(w) = btn.root().and_then(|r| r.downcast::<gtk4::Window>().ok()) {
                chooser.set_transient_for(Some(&w));
            }
            chooser.present();
        });
    }
    dest_row.append(&dest_entry);
    dest_row.append(&dest_browse);
    b.append(&dest_row);

    // Drive info (read-only)
    let drive_info = format!(
        "Drive label: {}   UUID: {}",
        cfg.borrow().drive_label.as_deref().unwrap_or("(not set)"),
        cfg.borrow().drive_uuid.as_deref().unwrap_or("(not set)"),
    );
    b.append(
        &Label::builder()
            .label(&drive_info)
            .halign(Align::Start)
            .wrap(true)
            .css_classes(vec!["dim-label"])
            .build(),
    );

    // ── Save ─────────────────────────────────────────────────────────────
    let save_btn = Button::builder()
        .label("Save")
        .css_classes(vec!["suggested-action"])
        .halign(Align::End)
        .margin_top(16)
        .build();
    let save_lbl = Label::builder().halign(Align::Start).build();
    {
        let cfg = Rc::clone(&cfg);
        let save_lbl = save_lbl.clone();
        save_btn.connect_clicked(move |_| {
            let new_src  = src_entry.text().to_string();
            let new_dest = dest_entry.text().to_string();

            // Refuse same-device configurations.
            if crate::drives::is_same_device(
                std::path::Path::new(&new_src),
                std::path::Path::new(&new_dest),
            ) {
                save_lbl.set_text(
                    "❌  Source and destination are on the same filesystem.  \
                     Choose a different drive.",
                );
                return;
            }

            {
                let mut c = cfg.borrow_mut();
                c.source_dir = new_src;
                c.dest_dir   = new_dest;
            }
            match cfg.borrow().save() {
                Ok(()) => save_lbl.set_text("✅  Paths saved."),
                Err(e) => save_lbl.set_text(&format!("❌  {e}")),
            }
        });
    }
    b.append(&save_btn);
    b.append(&save_lbl);
    b
}

// ── Log tab ───────────────────────────────────────────────────────────────────

fn build_log() -> GBox {
    let b = tab_box();

    b.append(
        &Label::builder()
            .label("Backup Log")
            .css_classes(vec!["title-2"])
            .halign(Align::Start)
            .build(),
    );

    let log_lbl = Label::builder()
        .label(&format!("Log file: {}", Config::log_path().display()))
        .halign(Align::Start)
        .selectable(true)
        .build();
    b.append(&log_lbl);

    let tv = TextView::builder()
        .monospace(true)
        .wrap_mode(WrapMode::None)
        .editable(false)
        .build();

    let sw = ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(300)
        .build();
    sw.set_child(Some(&tv));
    let frame = Frame::new(None);
    frame.set_child(Some(&sw));
    b.append(&frame);

    let refresh_btn = Button::builder()
        .label("⟳ Reload")
        .halign(Align::End)
        .margin_top(8)
        .build();
    b.append(&refresh_btn);

    // Load log on demand.
    load_log(&tv);
    {
        let tv = tv.clone();
        refresh_btn.connect_clicked(move |_| load_log(&tv));
    }

    b
}

fn load_log(tv: &TextView) {
    let path = Config::log_path();
    let text = if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => format!("Could not read log: {e}"),
        }
    } else {
        "No log file yet — run a backup to create one.".to_string()
    };
    let buf = tv.buffer();
    buf.set_text(&text);
    // Scroll to end.
    let mut end = buf.end_iter();
    tv.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn tab_box() -> GBox {
    let b = GBox::new(Orientation::Vertical, 12);
    b.set_margin_top(16);
    b.set_margin_start(16);
    b.set_margin_end(16);
    b.set_margin_bottom(16);
    b
}

fn field_label(text: &str) -> Label {
    Label::builder()
        .label(text)
        .halign(Align::Start)
        .wrap(true)
        .build()
}
