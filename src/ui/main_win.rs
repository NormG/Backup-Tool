use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
};

use crate::{backup, config::Config, systemd};
use gtk4::{
    glib, prelude::*, Align, Box as GBox, Button, ComboBoxText, FileChooserAction,
    FileChooserDialog, Frame, Label, ListBox, ListBoxRow, Notebook, Orientation, ResponseType,
    ScrolledWindow, SelectionMode, SpinButton, Switch, TextView, WrapMode,
};

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
    // Scrollable so all tabs are reachable even when the window is narrow.
    nb.set_scrollable(true);
    win.set_child(Some(&nb));

    let (dash_page, status_lbl) = build_dashboard(Rc::clone(&cfg));
    nb.append_page(&dash_page, Some(&Label::new(Some("Dashboard"))));

    let sched_page = build_schedule(Rc::clone(&cfg));
    nb.append_page(&sched_page, Some(&Label::new(Some("Schedule"))));

    let excl_page = build_excludes(Rc::clone(&cfg));
    nb.append_page(&excl_page, Some(&Label::new(Some("Excludes"))));

    let settings_page = build_settings(Rc::clone(&cfg));
    nb.append_page(&settings_page, Some(&Label::new(Some("Paths"))));

    let log_page = build_log();
    nb.append_page(&log_page, Some(&Label::new(Some("Log"))));

    let about_page = build_about();
    nb.append_page(&about_page, Some(&Label::new(Some("About"))));

    // ── Tab 7 — BTRFS Snapshots ───────────────────────────────────────────────
    let btrfs_page = build_btrfs_tab(Rc::clone(&cfg));
    nb.append_page(&btrfs_page, Some(&Label::new(Some("BTRFS"))));
    nb.set_tab_reorderable(&btrfs_page, false);

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

    // Use the saved label; if not set, detect it live from the destination path.
    let drive_label = cfg
        .drive_label
        .clone()
        .or_else(|| crate::drives::detect_label_for_path(&cfg.dest_dir))
        .unwrap_or_else(|| "(unlabelled)".to_string());

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
        drive_label,
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

    // Time — 12h / 24h toggle
    b.append(&field_label("Daily backup time:"));
    let (stored_h, stored_m) = cfg.borrow().backup_hm();
    let is_24h_time: Rc<RefCell<bool>> = Rc::new(RefCell::new(true));

    let time_row = GBox::new(Orientation::Horizontal, 8);

    let adj_h = gtk4::Adjustment::new(f64::from(stored_h), 0.0, 23.0, 1.0, 1.0, 0.0);
    let hour_spin = SpinButton::new(Some(&adj_h), 1.0, 0);
    hour_spin.set_width_chars(2);
    hour_spin.set_alignment(1.0); // right-justify digits
    hour_spin.connect_output(|s| {
        s.set_text(&format!("{:02}", s.value() as u32));
        glib::Propagation::Stop
    });
    time_row.append(&hour_spin);
    time_row.append(&Label::new(Some(":")));
    let adj_m = gtk4::Adjustment::new(f64::from(stored_m), 0.0, 59.0, 1.0, 5.0, 0.0);
    let min_spin = SpinButton::new(Some(&adj_m), 1.0, 0);
    min_spin.set_width_chars(2);
    min_spin.set_alignment(1.0);
    min_spin.connect_output(|s| {
        s.set_text(&format!("{:02}", s.value() as u32));
        glib::Propagation::Stop
    });
    time_row.append(&min_spin);

    // 24h suffix / AM-PM picker (mutually exclusive)
    let suffix_lbl = Label::new(Some("Hrs"));
    let ampm_combo_time = ComboBoxText::new();
    ampm_combo_time.append_text("AM");
    ampm_combo_time.append_text("PM");
    ampm_combo_time.set_active(Some(if stored_h >= 12 { 1 } else { 0 }));
    ampm_combo_time.set_visible(false);

    let fmt_btn = Button::with_label("Use 12 hr");
    fmt_btn.set_halign(gtk4::Align::End);
    time_row.append(&suffix_lbl);
    time_row.append(&ampm_combo_time);
    // Expanding spacer pushes the toggle button to the right edge of the tab.
    let tspc = gtk4::Label::builder().hexpand(true).build();
    time_row.append(&tspc);
    time_row.append(&fmt_btn);
    b.append(&time_row);

    // Toggle between 24h and 12h display.
    {
        let is_24h_time = Rc::clone(&is_24h_time);
        let hour_spin = hour_spin.clone();
        let suffix_lbl = suffix_lbl.clone();
        let ampm_combo_time = ampm_combo_time.clone();
        fmt_btn.connect_clicked(move |btn| {
            if *is_24h_time.borrow() {
                // Switch to 12h
                let h24 = hour_spin.value() as u8;
                let (h12, pm) = h24_to_12h(h24);
                ampm_combo_time.set_active(Some(if pm { 1 } else { 0 }));
                hour_spin.set_range(1.0, 12.0);
                hour_spin.set_value(f64::from(h12));
                suffix_lbl.set_visible(false);
                ampm_combo_time.set_visible(true);
                btn.set_label("Use 24 hr");
                *is_24h_time.borrow_mut() = false;
            } else {
                // Switch to 24h
                let h12 = hour_spin.value() as u8;
                let pm = ampm_combo_time.active() == Some(1);
                let h24 = h12_to_24h(h12, pm);
                hour_spin.set_range(0.0, 23.0);
                hour_spin.set_value(f64::from(h24));
                suffix_lbl.set_visible(true);
                ampm_combo_time.set_visible(false);
                btn.set_label("Use 12 hr");
                *is_24h_time.borrow_mut() = true;
            }
        });
    }

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
    let save_lbl = Label::builder().halign(Align::Start).build();

    {
        let cfg = Rc::clone(&cfg);
        let day_combo = day_combo.clone();
        let save_lbl = save_lbl.clone();
        save_btn.connect_clicked(move |_| {
            let day = day_combo
                .active_text()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "Monday".to_string());
            let h_raw = hour_spin.value() as u8;
            let m = min_spin.value() as u8;
            let h = if *is_24h_time.borrow() {
                h_raw
            } else {
                let pm = ampm_combo_time.active() == Some(1);
                h12_to_24h(h_raw, pm)
            };
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
                Err(e) => {
                    save_lbl.set_text(&format!("⚠  Saved config but timer reload failed: {e}"))
                }
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

    // Drive info (read-only) — fall back to live detection if not saved in config
    let detected_label = cfg
        .borrow()
        .drive_label
        .clone()
        .or_else(|| crate::drives::detect_label_for_path(&cfg.borrow().dest_dir))
        .unwrap_or_else(|| "(unlabelled)".to_string());
    let drive_info = format!(
        "Drive label: {}   UUID: {}",
        detected_label,
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
            let new_src = src_entry.text().to_string();
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
                c.dest_dir = new_dest;
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
        .label(format!("Log file: {}", Config::log_path().display()))
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

// ── BTRFS Snapshot tab ───────────────────────────────────────────────────────────

fn build_btrfs_tab(cfg: Rc<RefCell<Config>>) -> GBox {
    let b = tab_box();

    b.append(
        &Label::builder()
            .label("BTRFS Snapshots")
            .css_classes(vec!["title-2"])
            .halign(Align::Start)
            .build(),
    );

    // Detect filesystem type of source directory.
    let source = cfg.borrow().source_dir.clone();
    let fstype = crate::drives::detect_fstype(&source);
    let is_btrfs = fstype.as_deref() == Some("btrfs");

    b.append(
        &Label::builder()
            .label(format!(
                "Source filesystem: {}",
                fstype.as_deref().unwrap_or("unknown")
            ))
            .halign(Align::Start)
            .css_classes(vec!["dim-label"])
            .build(),
    );

    if !is_btrfs {
        b.append(
            &Label::builder()
                .label(
                    "BTRFS snapshots are only available when the source \
                     directory is on a BTRFS filesystem.",
                )
                .halign(Align::Start)
                .wrap(true)
                .css_classes(vec!["dim-label"])
                .margin_top(12)
                .build(),
        );
        return b;
    }

    b.append(&gtk4::Separator::new(Orientation::Horizontal));

    // ── Requirement notes ───────────────────────────────────────
    b.append(
        &Label::builder()
            .label(
                "ℹ  Snapshots must be outside the source subvolume and on the \
                 same BTRFS volume.  The default path /home/.snapshots is \
                 root-owned — run this once to allow your user to write there:",
            )
            .halign(Align::Start)
            .wrap(true)
            .css_classes(vec!["dim-label"])
            .build(),
    );

    // Copyable one-time setup command
    let user = std::env::var("USER").unwrap_or_else(|_| "$USER".to_string());
    let setup_cmd = format!(
        "sudo mkdir -p /home/.snapshots && sudo chown {user}:{user} /home/.snapshots"
    );
    b.append(
        &gtk4::Entry::builder()
            .text(&setup_cmd)
            .editable(false)
            .margin_bottom(4)
            .build(),
    );

    b.append(
        &Label::builder()
            .label(
                "ℹ  To export snapshots to an ext4 drive, use \
                 'btrfs send / btrfs receive' (Phase 2 — not yet in this GUI).",
            )
            .halign(Align::Start)
            .wrap(true)
            .css_classes(vec!["dim-label"])
            .margin_bottom(4)
            .build(),
    );

    // ── Snapshot storage path ───────────────────────────────────────
    let source_base = std::path::Path::new(&source)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    // Default to a sibling directory on the same BTRFS volume as the source.
    // The user must run the one-time setup command above if /home/.snapshots
    // does not yet exist.
    let source_parent = std::path::Path::new(&source)
        .parent()
        .unwrap_or(std::path::Path::new("/"))
        .to_string_lossy()
        .into_owned();
    let default_snap_dir = format!("{}/.snapshots", source_parent);

    b.append(&field_label("Snapshot storage path:"));
    let snap_row = GBox::new(Orientation::Horizontal, 8);
    let snap_entry = gtk4::Entry::builder()
        .text(&default_snap_dir)
        .hexpand(true)
        .build();
    snap_row.append(&snap_entry);
    b.append(&snap_row);

    // ── Create button ─────────────────────────────────────────────────
    let create_row = GBox::new(Orientation::Horizontal, 8);
    let create_btn = Button::builder()
        .label("Create Snapshot")
        .css_classes(vec!["suggested-action"])
        .build();
    let create_lbl = Label::builder()
        .halign(Align::Start)
        .hexpand(true)
        .wrap(true)
        .build();
    create_row.append(&create_btn);
    create_row.append(&create_lbl);
    b.append(&create_row);

    // ── Snapshot list ───────────────────────────────────────────────────
    let list_header = GBox::new(Orientation::Horizontal, 8);
    list_header.append(&field_label("Existing snapshots:"));
    let spacer = Label::builder().hexpand(true).build();
    list_header.append(&spacer);
    let refresh_btn = Button::with_label("↺ Refresh");
    list_header.append(&refresh_btn);
    b.append(&list_header);

    let list_box = ListBox::builder()
        .selection_mode(SelectionMode::Single)
        .build();
    let list_sw = ScrolledWindow::builder().min_content_height(130).build();
    let list_frame = Frame::new(None);
    list_sw.set_child(Some(&list_box));
    list_frame.set_child(Some(&list_sw));
    b.append(&list_frame);

    // ── Recovery instructions ──────────────────────────────────────────
    b.append(&field_label(
        "Recovery instructions (select a snapshot above):",
    ));
    let instr_tv = TextView::builder()
        .monospace(true)
        .editable(false)
        .wrap_mode(WrapMode::Word)
        .build();
    instr_tv
        .buffer()
        .set_text("Select a snapshot from the list above to see recovery instructions.");
    let instr_sw = ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(160)
        .build();
    instr_sw.set_child(Some(&instr_tv));
    let instr_frame = Frame::new(None);
    instr_frame.set_child(Some(&instr_sw));
    b.append(&instr_frame);

    // ── Delete button ─────────────────────────────────────────────────
    let del_btn = Button::builder()
        .label("Delete Selected Snapshot")
        .css_classes(vec!["destructive-action"])
        .halign(Align::End)
        .sensitive(false)
        .build();
    b.append(&del_btn);

    // ── Populate and wire ────────────────────────────────────────────────
    btrfs_populate_list(&list_box, &snap_entry.text(), &source_base);

    // List selection → update instructions + enable delete
    {
        let instr_tv = instr_tv.clone();
        let del_btn = del_btn.clone();
        let snap_entry = snap_entry.clone();
        list_box.connect_row_selected(glib::clone!(
            #[weak]
            instr_tv,
            #[weak]
            del_btn,
            move |_, row| {
                if let Some(row) = row {
                    let name = row.widget_name().to_string();
                    let snap_dir = snap_entry.text().to_string();
                    instr_tv
                        .buffer()
                        .set_text(&btrfs_instructions(&name, &snap_dir));
                    del_btn.set_sensitive(true);
                } else {
                    del_btn.set_sensitive(false);
                }
            }
        ));
    }

    // Create snapshot
    {
        let list_box = list_box.clone();
        let snap_entry = snap_entry.clone();
        let create_lbl = create_lbl.clone();
        let source = source.clone();
        let source_base = source_base.clone();
        create_btn.connect_clicked(glib::clone!(
            #[weak]
            list_box,
            move |_| {
                let snap_dir = snap_entry.text().to_string();
                let stamp = chrono::Local::now().format("%Y-%m-%d_%H%M%S").to_string();
                let snap_name = format!("{}-{}", source_base, stamp);
                let snap_path = format!("{}/{}", snap_dir, snap_name);

                // Validate that the chosen path is on BTRFS.
                if crate::drives::detect_fstype(&snap_dir).as_deref() != Some("btrfs") {
                    create_lbl.set_text(
                        "❌  Snapshot path is not on a BTRFS filesystem.  \
                         Choose a location on the same BTRFS volume as the source \
                         (e.g. /home/.snapshots).",
                    );
                    return;
                }
                if let Err(e) = std::fs::create_dir_all(&snap_dir) {
                    create_lbl.set_text(&format!("❌  Could not create snapshot dir: {e}"));
                    return;
                }
                match std::process::Command::new("btrfs")
                    .args(["subvolume", "snapshot", "-r", &source, &snap_path])
                    .output()
                {
                    Ok(o) if o.status.success() => {
                        create_lbl.set_text(&format!("✅  Snapshot created: {}", snap_name));
                        btrfs_populate_list(&list_box, &snap_dir, &source_base);
                    }
                    Ok(o) => {
                        let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                        create_lbl.set_text(&format!("❌  btrfs error: {err}"));
                    }
                    Err(_) => {
                        create_lbl.set_text(
                            "❌  'btrfs' not found.  \
                             Install with: sudo dnf install btrfs-progs",
                        );
                    }
                }
            }
        ));
    }

    // Refresh list
    {
        let list_box = list_box.clone();
        let snap_entry = snap_entry.clone();
        refresh_btn.connect_clicked(glib::clone!(
            #[weak]
            list_box,
            move |_| btrfs_populate_list(&list_box, &snap_entry.text(), &source_base)
        ));
    }

    // Delete snapshot
    {
        let list_box = list_box.clone();
        let snap_entry = snap_entry.clone();
        del_btn.connect_clicked(glib::clone!(
            #[weak]
            list_box,
            move |btn| {
                if let Some(row) = list_box.selected_row() {
                    let name = row.widget_name().to_string();
                    let snap_path = format!("{}/{}", snap_entry.text(), name);
                    match std::process::Command::new("btrfs")
                        .args(["subvolume", "delete", &snap_path])
                        .output()
                    {
                        Ok(o) if o.status.success() => {
                            btrfs_populate_list(&list_box, &snap_entry.text(), "");
                            btn.set_sensitive(false);
                        }
                        Ok(o) => {
                            eprintln!(
                                "btrfs delete failed: {}",
                                String::from_utf8_lossy(&o.stderr)
                            );
                        }
                        Err(e) => eprintln!("btrfs not found: {e}"),
                    }
                }
            }
        ));
    }

    b
}

/// Populate a `ListBox` with snapshot subdirectories found in `snap_dir`
/// whose names start with `prefix` (may be empty to show all).
fn btrfs_populate_list(list_box: &ListBox, snap_dir: &str, prefix: &str) {
    // Remove all existing rows.
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }

    let dir = std::path::Path::new(snap_dir);
    if !dir.exists() {
        let row = ListBoxRow::new();
        row.set_child(Some(
            &Label::builder()
                .label("(no snapshots yet)")
                .halign(Align::Start)
                .css_classes(vec!["dim-label"])
                .margin_start(8)
                .margin_top(4)
                .margin_bottom(4)
                .build(),
        ));
        list_box.append(&row);
        return;
    }

    let mut entries: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            e.path().is_dir() && (prefix.is_empty() || name.starts_with(prefix))
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    entries.sort_by(|a, b| b.cmp(a)); // newest first

    if entries.is_empty() {
        let row = ListBoxRow::new();
        row.set_child(Some(
            &Label::builder()
                .label("(no snapshots yet)")
                .halign(Align::Start)
                .css_classes(vec!["dim-label"])
                .margin_start(8)
                .margin_top(4)
                .margin_bottom(4)
                .build(),
        ));
        list_box.append(&row);
        return;
    }

    for name in entries {
        let row = ListBoxRow::new();
        row.set_widget_name(&name);
        row.set_child(Some(
            &Label::builder()
                .label(&name)
                .halign(Align::Start)
                .margin_start(8)
                .margin_top(4)
                .margin_bottom(4)
                .build(),
        ));
        list_box.append(&row);
    }
}

/// Generate the recovery instructions string for a snapshot.
fn btrfs_instructions(snap_name: &str, snap_dir: &str) -> String {
    // Try to resolve the device backing the snapshot directory.
    let device = std::process::Command::new("findmnt")
        .args(["--noheadings", "-o", "SOURCE", "--target", snap_dir])
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        })
        .unwrap_or_else(|| "/dev/<device>".to_string());

    format!(
        "Snapshot : {name}\n\
         Location : {dir}/{name}\n\
         Device   : {device}\n\
         \n\
         ── Access individual files ───────────────────────────\n\
         1. Mount the BTRFS volume:\n\
            sudo mount -o subvol=.btrfs-snapshots/{name} {device} /mnt/recovery\n\
         \n\
         2. Browse and copy files:\n\
            ls /mnt/recovery/\n\
            cp /mnt/recovery/Documents/file.txt ~/Documents/\n\
         \n\
         3. Unmount when done:\n\
            sudo umount /mnt/recovery\n\
         \n\
         ── Full home directory restore ────────────────────────\n\
         ⚠  WARNING: this replaces your entire home directory.\n\
         \n\
         1. Boot from a live USB or log in as a different user.\n\
         2. Mount the BTRFS root volume:\n\
            sudo mount {device} /mnt/btrfs\n\
         3. Delete the current home subvolume:\n\
            sudo btrfs subvolume delete /home/$USER\n\
         4. Create a writable snapshot from the recovery point:\n\
            sudo btrfs subvolume snapshot \\\n\
              /mnt/btrfs/.btrfs-snapshots/{name} /home/$USER\n\
         5. Reboot.\n",
        name = snap_name,
        dir = snap_dir,
        device = device,
    )
}

// ── About tab ──────────────────────────────────────────────────────────────────

fn build_about() -> GBox {
    let b = tab_box();

    // App name + icon row
    let header_row = GBox::new(Orientation::Horizontal, 16);
    header_row.set_margin_bottom(8);

    // Icon
    let icon = gtk4::Image::from_icon_name("home-backup");
    icon.set_pixel_size(64);
    icon.set_valign(Align::Start);
    header_row.append(&icon);

    // Title + version stacked vertically
    let title_col = GBox::new(Orientation::Vertical, 4);
    title_col.append(
        &Label::builder()
            .label("Home Backup")
            .css_classes(vec!["title-1"])
            .halign(Align::Start)
            .build(),
    );
    title_col.append(
        &Label::builder()
            .label(concat!("Version ", env!("CARGO_PKG_VERSION")))
            .halign(Align::Start)
            .css_classes(vec!["dim-label"])
            .build(),
    );
    title_col.append(
        &Label::builder()
            .label("GTK4 home-directory backup manager for Fedora Linux")
            .halign(Align::Start)
            .wrap(true)
            .build(),
    );
    header_row.append(&title_col);
    b.append(&header_row);

    b.append(&gtk4::Separator::new(Orientation::Horizontal));

    // Details grid
    let details = [
        ("License", "GPL-3.0-or-later"),
        ("Source", "github.com/NormG/Backup-Tool"),
        ("Backup tool", "rsync (atomic hardlinked snapshots)"),
        ("Scheduler", "systemd user timer — no root required"),
        ("Config", "~/.config/home-backup/config.toml"),
        ("Log", "~/.local/share/home-backup/backup.log"),
    ];

    for (key, val) in &details {
        let row = GBox::new(Orientation::Horizontal, 12);
        row.set_margin_top(4);
        row.append(
            &Label::builder()
                .label(*key)
                .halign(Align::Start)
                .width_chars(14)
                .css_classes(vec!["heading"])
                .build(),
        );
        row.append(
            &Label::builder()
                .label(*val)
                .halign(Align::Start)
                .selectable(true)
                .wrap(true)
                .hexpand(true)
                .build(),
        );
        b.append(&row);
    }

    b.append(&gtk4::Separator::new(Orientation::Horizontal));

    // Copyright
    b.append(
        &Label::builder()
            .label("Copyright \u{00a9} 2026 veronalinux.ca.  Distributed under the GNU General Public License v3.")
            .halign(Align::Start)
            .wrap(true)
            .css_classes(vec!["dim-label"])
            .build(),
    );

    b
}

// ── Clock conversion helpers ─────────────────────────────────────────────────

/// 24-hour hour → (12-hour display value, is_pm)
fn h24_to_12h(h24: u8) -> (u8, bool) {
    match h24 {
        0 => (12, false),
        1..=11 => (h24, false),
        12 => (12, true),
        h => (h - 12, true),
    }
}

/// 12-hour display value + AM/PM flag → 24-hour hour
fn h12_to_24h(h12: u8, pm: bool) -> u8 {
    match (pm, h12) {
        (false, 12) => 0,
        (false, h) => h,
        (true, 12) => 12,
        (true, h) => h + 12,
    }
}

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
