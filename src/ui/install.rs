use std::{cell::RefCell, rc::Rc};

use gtk4::{
    glib, prelude::*, Align, ApplicationWindow, Box as GBox, Button, ComboBoxText, DropDown,
    Entry, FileChooserAction, FileChooserDialog, Frame, Label, Orientation, ResponseType,
    ScrolledWindow, SpinButton, Stack, StringList, TextView, WrapMode,
};

use crate::{config::Config, drives, systemd};

// ── Entry-point ───────────────────────────────────────────────────────────────

/// Build and show the first-run install wizard.
///
/// The wizard registers itself as an `ApplicationWindow` so the GTK
/// application exits cleanly when the user closes it.  `on_done` is called
/// with the completed config; the caller is responsible for opening the main
/// management window before or instead of the wizard window closing.
pub fn show<F: Fn(Config) + 'static>(app: &gtk4::Application, on_done: F) {
    let cfg = Rc::new(RefCell::new(Config::default()));

    let win = ApplicationWindow::builder()
        .application(app)
        .title("Home Backup — Setup")
        .default_width(640)
        .default_height(520)
        .resizable(false)
        .build();

    let outer = GBox::new(Orientation::Vertical, 0);
    win.set_child(Some(&outer));

    // ── Header bar ────────────────────────────────────────────────────────
    let header = GBox::new(Orientation::Horizontal, 12);
    header.set_margin_top(16);
    header.set_margin_start(24);
    header.set_margin_end(24);
    header.set_margin_bottom(8);

    let title_lbl = Label::builder()
        .label("Home Backup Setup")
        .css_classes(vec!["title-1"])
        .halign(Align::Start)
        .hexpand(true)
        .build();
    header.append(&title_lbl);
    outer.append(&header);

    // ── Page stack ────────────────────────────────────────────────────────
    let stack = Stack::new();
    stack.set_vexpand(true);
    stack.set_margin_start(24);
    stack.set_margin_end(24);
    outer.append(&stack);

    // ── Navigation buttons ────────────────────────────────────────────────
    let nav_row = GBox::new(Orientation::Horizontal, 8);
    nav_row.set_margin_top(8);
    nav_row.set_margin_bottom(16);
    nav_row.set_margin_start(24);
    nav_row.set_margin_end(24);
    nav_row.set_halign(Align::End);

    let btn_back = Button::with_label("Back");
    let btn_next = Button::with_label("Next");
    btn_next.add_css_class("suggested-action");
    nav_row.append(&btn_back);
    nav_row.append(&btn_next);
    outer.append(&nav_row);

    // ── Build pages ───────────────────────────────────────────────────────
    let drives_state: Rc<RefCell<Vec<drives::DriveInfo>>> = Rc::new(RefCell::new(vec![]));

    let p_welcome = build_welcome();
    let (p_source, source_entry) = build_source(Rc::clone(&cfg));
    let (p_drive, drive_drop, drive_dest_entry, refresh_btn) =
        build_drive(Rc::clone(&cfg), Rc::clone(&drives_state));
    let (p_schedule, day_combo, hour_spin, min_spin, ret_spin, inc_spin) =
        build_schedule(Rc::clone(&cfg));
    let (p_excludes, excludes_tv) = build_excludes(Rc::clone(&cfg));
    let (p_review, review_lbl) = build_review();
    let (p_done, done_lbl) = build_done();

    stack.add_titled(&p_welcome, Some("welcome"), "Welcome");
    stack.add_titled(&p_source, Some("source"), "Source");
    stack.add_titled(&p_drive, Some("drive"), "Drive");
    stack.add_titled(&p_schedule, Some("schedule"), "Schedule");
    stack.add_titled(&p_excludes, Some("excludes"), "Excludes");
    stack.add_titled(&p_review, Some("review"), "Review");
    stack.add_titled(&p_done, Some("done"), "Done");

    let pages = [
        "welcome", "source", "drive", "schedule", "excludes", "review", "done",
    ];
    let current_page: Rc<RefCell<usize>> = Rc::new(RefCell::new(0));

    // ── Populate drive dropdown lazily when that page becomes visible ─────
    {
        let drives_state = Rc::clone(&drives_state);
        let drive_drop = drive_drop.clone();
        let stack = stack.clone();
        stack.connect_visible_child_notify(glib::clone!(
            #[weak]
            drive_drop,
            move |s| {
                if s.visible_child_name().as_deref() == Some("drive") {
                    refresh_drives(&drive_drop, &drives_state);
                }
            }
        ));
    }

    // Refresh button re-scans drives.
    {
        let drives_state = Rc::clone(&drives_state);
        let drive_drop = drive_drop.clone();
        refresh_btn.connect_clicked(glib::clone!(
            #[weak]
            drive_drop,
            move |_| refresh_drives(&drive_drop, &drives_state)
        ));
    }

    // ── Navigation logic ─────────────────────────────────────────────────
    let update_nav = {
        let btn_back = btn_back.clone();
        let btn_next = btn_next.clone();
        let current_page = Rc::clone(&current_page);
        let pages_len = pages.len();
        move || {
            let idx = *current_page.borrow();
            btn_back.set_sensitive(idx > 0 && idx < pages_len - 1);
            match idx {
                4 => btn_next.set_label("Review"),       // excludes → review
                5 => btn_next.set_label("Install"),      // review → install
                6 => btn_next.set_label("Open Manager"), // done
                _ => btn_next.set_label("Next"),
            }
        }
    };
    update_nav();

    // ── Back ─────────────────────────────────────────────────────────────
    btn_back.connect_clicked(glib::clone!(
        #[strong]
        stack,
        #[strong]
        current_page,
        #[strong]
        pages,
        move |_| {
            // Compute the previous page name and drop the borrow BEFORE
            // calling set_visible_child_name; that call fires
            // connect_visible_child_notify synchronously, and any handler
            // that tries to borrow current_page would panic if we still
            // held the guard here.
            let prev = {
                let mut idx = current_page.borrow_mut();
                if *idx == 0 { return; }
                *idx -= 1;
                pages[*idx]
            };
            stack.set_visible_child_name(prev);
        }
    ));

    // ── Next / Install / Done ────────────────────────────────────────────
    let on_done_rc: Rc<dyn Fn(Config)> = Rc::new(on_done);
    btn_next.connect_clicked(glib::clone!(
        #[strong]
        stack,
        #[strong]
        current_page,
        #[strong]
        pages,
        #[strong]
        cfg,
        #[strong]
        drives_state,
        #[strong]
        drive_drop,
        #[strong]
        drive_dest_entry,
        #[strong]
        source_entry,
        #[strong]
        day_combo,
        #[strong]
        hour_spin,
        #[strong]
        min_spin,
        #[strong]
        ret_spin,
        #[strong]
        inc_spin,
        #[strong]
        excludes_tv,
        #[strong]
        review_lbl,
        #[strong]
        done_lbl,
        #[strong]
        win,
        #[strong]
        on_done_rc,
        move |_| {
            let idx = *current_page.borrow();

            // Collect per-page data before advancing.
            match idx {
                1 => {
                    // source page
                    cfg.borrow_mut().source_dir = source_entry.text().to_string();
                }
                2 => {
                    // drive page: copy selection into config
                    let sel = drive_drop.selected() as usize;
                    let ds = drives_state.borrow();
                    if let Some(drv) = ds.get(sel) {
                        cfg.borrow_mut().drive_uuid = drv.uuid.clone();
                        cfg.borrow_mut().drive_label = drv.label.clone();
                    }
                    cfg.borrow_mut().dest_dir = drive_dest_entry.text().to_string();
                }
                3 => {
                    // schedule page
                    let day = day_combo
                        .active_text()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "Monday".to_string());
                    let h = hour_spin.value() as u8;
                    let m = min_spin.value() as u8;
                    let ret = ret_spin.value() as u32;
                    let inc = inc_spin.value() as u32;
                    let mut c = cfg.borrow_mut();
                    c.full_backup_day = day;
                    c.backup_time = format!("{h:02}:{m:02}");
                    c.retention_days = ret;
                    c.incremental_every_n_days = inc;
                }
                4 => {
                    // excludes page
                    let buf = excludes_tv.buffer();
                    let text = buf.text(&buf.start_iter(), &buf.end_iter(), false);
                    let excl: Vec<String> = text
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(|l| l.to_string())
                        .collect();
                    cfg.borrow_mut().excludes = excl;
                }
                5 => {
                    // review page → install
                    do_install(
                        &cfg,
                        &done_lbl,
                        &win,
                        &on_done_rc,
                        &stack,
                        &current_page,
                        pages,
                    );
                    return;
                }
                6 => {
                    // done page → open main window.
                    // Call on_done FIRST so the main ApplicationWindow is
                    // registered before this wizard window closes; without
                    // that ordering the GTK app would exit in the gap.
                    let c = cfg.borrow().clone();
                    on_done_rc(c);
                    win.close();
                    return;
                }
                _ => {}
            }

            // Advance page.  Drop the borrow before set_visible_child_name
            // to avoid a re-entrant RefCell panic in the notify signal handler.
            let next_name = {
                let mut i = current_page.borrow_mut();
                *i += 1;
                pages[*i]
            };
            stack.set_visible_child_name(next_name);

            if next_name == "review" {
                review_lbl.set_text(&build_review_text(&cfg.borrow()));
            }
        }
    ));

    // Update nav buttons when the visible page changes.
    // NOTE: current_page is already updated by the click handlers before
    // set_visible_child_name is called, so we can read it safely here.
    stack.connect_visible_child_notify(glib::clone!(
        #[strong]
        btn_back,
        #[strong]
        btn_next,
        #[strong]
        current_page,
        move |_| {
            let idx = *current_page.borrow();
            btn_back.set_sensitive(idx > 0 && idx < pages.len() - 1);
            match idx {
                4 => btn_next.set_label("Review"),
                5 => btn_next.set_label("Install"),
                6 => btn_next.set_label("Open Manager"),
                _ => btn_next.set_label("Next"),
            }
        }
    ));

    win.present();
}

// ── Page builders ─────────────────────────────────────────────────────────────

fn page_box() -> GBox {
    let b = GBox::new(Orientation::Vertical, 12);
    b.set_margin_top(16);
    b.set_margin_bottom(8);
    b
}

fn section_label(text: &str) -> Label {
    Label::builder()
        .label(text)
        .halign(Align::Start)
        .css_classes(vec!["heading"])
        .build()
}

fn sub_label(text: &str) -> Label {
    Label::builder()
        .label(text)
        .halign(Align::Start)
        .wrap(true)
        .build()
}

// Page 0 – Welcome
fn build_welcome() -> GBox {
    let b = page_box();
    b.append(
        &Label::builder()
            .label("Welcome to Home Backup")
            .css_classes(vec!["title-2"])
            .halign(Align::Start)
            .build(),
    );
    b.append(&sub_label(
        "This wizard will guide you through setting up automatic backups \
         of your home directory to an external drive using rsync.\n\n\
         Backups are atomic point-in-time snapshots stored as:\n\
         • full-YYYY-MM-DD_HHmmss  — weekly full copy\n\
         • inc-YYYY-MM-DD_HHmmss   — daily incremental (hardlinked)\n\n\
         A systemd user timer runs backups automatically at the time you choose.\n\
         You can always open this app to change settings or trigger a backup manually.",
    ));
    b
}

// Page 1 – Source directory
fn build_source(cfg: Rc<RefCell<Config>>) -> (GBox, Entry) {
    let b = page_box();
    b.append(&section_label("Source Directory"));
    b.append(&sub_label(
        "Choose the directory to back up.  Defaults to your home folder.",
    ));

    let row = GBox::new(Orientation::Horizontal, 8);
    let src = cfg.borrow().source_dir.clone();
    let entry = Entry::builder().text(&src).hexpand(true).build();

    let browse = Button::with_label("Browse…");
    {
        let entry = entry.clone();
        browse.connect_clicked(move |btn| {
            let chooser = FileChooserDialog::builder()
                .title("Choose source directory")
                .action(FileChooserAction::SelectFolder)
                .build();
            chooser.add_button("Cancel", ResponseType::Cancel);
            chooser.add_button("Select", ResponseType::Accept);
            let entry = entry.clone();
            chooser.connect_response(move |dlg, resp| {
                if resp == ResponseType::Accept {
                    if let Some(f) = dlg.file().and_then(|f| f.path()) {
                        entry.set_text(&f.to_string_lossy());
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

    row.append(&entry);
    row.append(&browse);
    b.append(&row);
    (b, entry)
}

// Page 2 – Drive selection
fn build_drive(
    cfg: Rc<RefCell<Config>>,
    drives_state: Rc<RefCell<Vec<drives::DriveInfo>>>,
) -> (GBox, DropDown, Entry, Button) {
    let b = page_box();
    b.append(&section_label("Backup Drive"));
    b.append(&sub_label(
        "Select the partition that will store your backups.  \
         If the drive is not listed, plug it in and press Refresh.",
    ));

    // Drive dropdown (populated lazily when the page becomes visible)
    let model = StringList::new(&[]);
    let drop = DropDown::builder().model(&model).hexpand(true).build();

    let refresh_btn = Button::with_label("⟳ Refresh");

    let drop_row = GBox::new(Orientation::Horizontal, 8);
    drop_row.append(&drop);
    drop_row.append(&refresh_btn);
    b.append(&drop_row);

    // Destination path
    b.append(&section_label("Backup path on drive"));
    b.append(&sub_label(
        "Path where snapshots will be stored on the chosen partition.\n\
         It will be created if it does not exist.",
    ));

    let dest_default = {
        let src = cfg.borrow().dest_dir.clone();
        if src.is_empty() {
            format!("/mnt/home_backups/{}", glib::host_name())
        } else {
            src
        }
    };
    let dest_entry = Entry::builder().text(&dest_default).hexpand(true).build();
    b.append(&dest_entry);

    // When the selected drive changes, update the dest_entry prefix.
    {
        let drives_state = Rc::clone(&drives_state);
        let dest_entry = dest_entry.clone();
        drop.connect_selected_notify(glib::clone!(
            #[weak]
            dest_entry,
            move |dd| {
                let ds = drives_state.borrow();
                if let Some(drv) = ds.get(dd.selected() as usize) {
                    if let Some(mp) = &drv.mountpoint {
                        let host = glib::host_name();
                        dest_entry.set_text(&format!("{mp}/home_backups/{host}"));
                    }
                }
            }
        ));
    }

    (b, drop, dest_entry, refresh_btn)
}

// Page 3 – Schedule
fn build_schedule(
    cfg: Rc<RefCell<Config>>,
) -> (GBox, ComboBoxText, SpinButton, SpinButton, SpinButton, SpinButton) {
    let b = page_box();
    b.append(&section_label("Backup Schedule"));

    // Full backup day
    b.append(&sub_label("Full backup day of week:"));
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
    b.append(&day_combo);

    // Time row
    b.append(&sub_label("Daily backup time (24-hour):"));
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
    b.append(&sub_label("Keep incremental snapshots for (days):"));
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
    b.append(&sub_label(
        "Run incremental backup every N days (1 = daily, 2 = every other day, 7 = weekly):",
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

    (b, day_combo, hour_spin, min_spin, ret_spin, inc_spin)
}

// Page 4 – Excludes
fn build_excludes(cfg: Rc<RefCell<Config>>) -> (GBox, TextView) {
    let b = page_box();
    b.append(&section_label("Exclude Patterns"));
    b.append(&sub_label(
        "One rsync exclude pattern per line.  \
         Leading '/' anchors to the source root.  \
         Trailing '/' means directory only.",
    ));

    let tv = TextView::builder()
        .monospace(true)
        .wrap_mode(WrapMode::None)
        .build();

    let text = cfg.borrow().excludes.join("\n");
    tv.buffer().set_text(&text);

    let sw = ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(180)
        .build();
    sw.set_child(Some(&tv));

    let frame = Frame::new(None);
    frame.set_child(Some(&sw));
    b.append(&frame);

    (b, tv)
}

// Page 5 – Review
fn build_review() -> (GBox, Label) {
    let b = page_box();
    b.append(&section_label("Review & Install"));
    b.append(&sub_label(
        "Please review the settings below.  Click Install to proceed.",
    ));

    let lbl = Label::builder()
        .wrap(true)
        .halign(Align::Start)
        .selectable(true)
        .build();
    lbl.add_css_class("monospace");
    b.append(&lbl);
    (b, lbl)
}

// Page 6 – Done
fn build_done() -> (GBox, Label) {
    let b = page_box();
    b.append(
        &Label::builder()
            .label("✅  Installation complete")
            .css_classes(vec!["title-2"])
            .halign(Align::Start)
            .build(),
    );
    let lbl = Label::builder()
        .wrap(true)
        .halign(Align::Start)
        .selectable(true)
        .build();
    lbl.add_css_class("monospace");
    b.append(&lbl);
    (b, lbl)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn refresh_drives(drop: &DropDown, drives_state: &Rc<RefCell<Vec<drives::DriveInfo>>>) {
    match drives::list_drives() {
        Ok(list) => {
            // Collect owned Strings first, then borrow as &str for StringList.
            let labels_owned: Vec<String> = list.iter().map(|d| d.display_label()).collect();
            let labels: Vec<&str> = labels_owned.iter().map(String::as_str).collect();
            let model = StringList::new(&labels);
            drop.set_model(Some(&model));
            *drives_state.borrow_mut() = list;
        }
        Err(e) => {
            let msg = format!("Error listing drives: {e}");
            let model = StringList::new(&[msg.as_str()]);
            drop.set_model(Some(&model));
        }
    }
}

fn build_review_text(cfg: &Config) -> String {
    let inc_desc = match cfg.incremental_every_n_days {
        1 => "daily".to_string(),
        7 => "weekly".to_string(),
        n => format!("every {n} days"),
    };
    format!(
        "Source directory  : {}\n\
         Backup destination: {}\n\
         Drive UUID        : {}\n\
         Drive label       : {}\n\
         Full backup day   : {}\n\
         Daily time        : {}\n\
         Incrementals      : {inc_desc}\n\
         Retention         : {} days\n\
         Excludes          : {} patterns\n\n\
         What will be installed:\n\
         • ~/.config/systemd/user/home-backup.service\n\
         • ~/.config/systemd/user/home-backup.timer  (OnCalendar=*-*-* {})\n\
         • ~/.local/share/applications/home-backup.desktop\n\
         • ~/.local/share/icons/hicolor/128x128/apps/home-backup.png\n\
         • ~/.config/home-backup/config.toml",
        cfg.source_dir,
        cfg.dest_dir,
        cfg.drive_uuid.as_deref().unwrap_or("(none)"),
        cfg.drive_label.as_deref().unwrap_or("(none)"),
        cfg.full_backup_day,
        cfg.backup_time,
        cfg.retention_days,
        cfg.excludes.len(),
        cfg.backup_time,
    )
}

fn do_install(
    cfg: &Rc<RefCell<Config>>,
    done_lbl: &Label,
    win: &impl IsA<gtk4::Window>,
    _on_done_rc: &Rc<dyn Fn(Config)>,
    stack: &Stack,
    current_page: &Rc<RefCell<usize>>,
    pages: [&str; 7],
) {
    // Validate minimum requirements.
    {
        let c = cfg.borrow();
        if c.dest_dir.is_empty() {
            show_error(
                win,
                "Backup destination path is empty.  Please choose a drive.",
            );
            return;
        }

        // Block backing up to the same filesystem as the source.
        // This catches both the obvious case (dest inside ~/) and subtler
        // cases where a bind-mount or second partition on the same disk is
        // selected.
        if drives::is_same_device(
            std::path::Path::new(&c.source_dir),
            std::path::Path::new(&c.dest_dir),
        ) {
            show_error(
                win,
                "The backup destination is on the same filesystem as the source 
directory.\n\n\
                 Backing up to the same drive defeats the purpose of a backup — 
if the\n\
                 drive fails you lose both your data and its backup.\n\n\
                 Please choose a different physical drive.",
            );
            return;
        }
    }

    // Save config first.
    {
        let mut c = cfg.borrow_mut();
        c.installed = true;
        if let Err(e) = c.save() {
            show_error(win, &format!("Could not save config: {e}"));
            return;
        }
    }

    // Run systemd install.
    let recap = match systemd::install(&cfg.borrow()) {
        Ok(log) => format!(
            "Configuration saved to:\n  {}\n\nInstalled:\n{}\n\nSchedule:\n\
             • Full backup every {full_day}\n\
             • Incremental every other day\n\
             • Daily at {time}\n\
             • Incrementals kept for {ret} days\n\n\
             You can now close this wizard.  The backup timer is active.",
            Config::config_path().display(),
            log,
            full_day = cfg.borrow().full_backup_day,
            time = cfg.borrow().backup_time,
            ret = cfg.borrow().retention_days,
        ),
        Err(e) => format!(
            "⚠  Systemd setup encountered an error:\n{e}\n\n\
             Config was saved.  You can re-run the install from Settings.",
        ),
    };

    done_lbl.set_text(&recap);

    // Drop borrow before set_visible_child_name fires the notify signal.
    {
        let mut i = current_page.borrow_mut();
        *i = 6;
    }
    stack.set_visible_child_name(pages[6]);
}

fn show_error(win: &impl IsA<gtk4::Window>, msg: &str) {
    let dlg = gtk4::MessageDialog::builder()
        .transient_for(win)
        .modal(true)
        .message_type(gtk4::MessageType::Error)
        .buttons(gtk4::ButtonsType::Ok)
        .text(msg)
        .build();
    dlg.connect_response(|d, _| d.close());
    dlg.present();
}
