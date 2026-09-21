# Backup-Tool — Kanban

Project board for **backup logic (BLC)**, **packaging & release**, and related work. Card IDs are stable for commits and PR descriptions.

**Terminology:** rsync `full-*` / `inc-*` = **backups**. Btrfs / VM = **snapshots** (out of BLC scope unless extended later).

---

## Backup Logic Component (BLC)

**Purpose:** Pre-backup intelligence that compares **schedule + config** to **what is on the backup drive**, detects deviation *before* rsync runs, applies **rule-gated fixes**, and notifies with the right severity.

**Origin:** Aug 30 2026 — Sunday full deferred (disk space), inc ran instead. Runtime logic was correct; **expectation vs reality** was invisible until log dig. Desktop notify said “inc complete”, not “scheduled full deviated”.

### Architecture sketch

```
┌─────────────┐     ┌──────────────────────────────────────┐     ┌─────────────┐
│ config.toml │────▶│ BLC: expect() from schedule + clock  │────▶│ Expectation │
│ + pending   │     │      scan() backup dir + df          │     │ vs Reality  │
└─────────────┘     │      reconcile() rule-gated fixes    │     └──────┬──────┘
                    └──────────────────────────────────────┘            │
                              ▲                                         ▼
                              │                              notify / log / block
                    ┌─────────┴─────────┐                    backup.rs run()
                    │ Rules: allowed /  │
                    │ forbidden actions │
                    └───────────────────┘
```

### Expectation model (examples)

| Config | Today | Expected on disk before 02:00 run |
|--------|-------|-----------------------------------|
| `full_backup_day = Sunday` | Sunday AM, no full dated today | **Full** should run; **no incs** after successful full |
| Same, full deferred prior night | Monday | **pending full**; retry full or inc if still no space |
| `keep_full_snapshots = 2` | After successful full | At most **2** `full-*`; older fulls removed |
| `incremental_every_n_days = 1` | Weekday | At most **1 inc per calendar day** |

### Reconcile rules (draft — Norm to approve)

**BLC may do automatically (no user prompt):**

- Empty backup-volume trash (already in 0.1.10)
- Apply `keep_full_snapshots` on **old** fulls when a new full is about to start **and** space check passes after prune
- Delete **orphan** `.inprogress-*` staging dirs older than N hours
- Log + **critical** notify when deviation detected

**BLC may do when explicit policy enabled (`blc_auto_reclaim_space = true`):**

- Remove oldest full beyond retention to make room for scheduled full
- Prune incs **only when** replacing with imminent successful full (never before space confirmed)

**BLC must not do without user:**

- Delete the only full backup
- Delete backups newer than retention policy
- Shrink source tree or excludes

### Notification tiers

| Tier | When | Example |
|------|------|---------|
| **normal** | Expected outcome | “Full backup complete” |
| **warning** | Deviation, backup still ran | “Scheduled full → inc (insufficient space)” |
| **critical** | Deviation + user action needed | “Sunday full blocked; 87G free, 500G needed — delete full-2026-07-27 or trim excludes” |

Fix **today:** `notify-send -u critical` when `deferred_full`; body must lead with deviation, not “inc succeeded”.

### BLC kanban

#### Backlog

| ID | Card | Notes |
|----|------|-------|
| BLC-1 | **Expectation model** — `blc::expect(config, now)` → struct: intended kind, max fulls, inc policy | Pure function, unit-testable |
| BLC-2 | **Reality scan** — `blc::scan(dest_root)` → fulls, incs, latest, df avail, pending_full | Wrap existing `resolve_latest`, retention helpers |
| BLC-3 | **Gap report** — `blc::analyze(expect, scan)` → deviations vec | e.g. `IncPresentOnScheduledFullDay`, `TooManyFulls`, `InsufficientSpaceForFull` |
| BLC-4 | **Rules engine** — `blc::reconcile(gap, config, rules)` → actions + remaining gaps | Returns what was fixed vs needs human |
| BLC-5 | **Pre-run hook** — call BLC before `auto_kind` / rsync in `backup::run` | `backup auto` only; manual `full`/`inc` unchanged |
| BLC-7 | **Config: `blc_auto_reclaim_space`** — opt-in delete old full for space | Default false |
| BLC-8 | **GUI: BLC status strip** — Dashboard tab shows expect vs actual | Yellow/red banner |
| BLC-9 | **Integration tests** — Sunday defer, Sunday success prune, keep_full_snapshots | Fixture dirs under `tests/fixtures/` |
| BLC-10 | **Post-success validate** — after backup, assert disk matches expect | Catch prune failures |

#### Ready

| ID | Card | Notes |
|----|------|-------|
| — | — | — |

#### In Progress

| ID | Card | Owner | Notes |
|----|------|-------|-------|
| — | — | — | — |

#### Review

| ID | Card | Notes |
|----|------|-------|
| — | — | — |

#### Done

| ID | Card | Notes |
|----|------|-------|
| BLC-6 | Deviation notifications | 0.1.11 — critical notify + GUI caution-tape banner |

### Reference: Aug 30 2026 incident

**Schedule:** Sunday full (`full_backup_day = Sunday`), `keep_full_snapshots = 0`.

**Expected after successful 02:00 run:** new `full-2026-08-30_*`, prior week `full-2026-08-23_*`, **no** `inc-*` from Aug 24–30.

**Actual:** `⚠️ Scheduled full deferred (insufficient space)` → `inc-2026-08-30_*`; 7 incs retained; 87G free / ~500G needed.

**BLC would have reported before rsync:**

1. Today is scheduled full day; latest is inc dated today → pending expectation unmet  
2. Space: 87G avail < 502G required → recommend delete `full-2026-07-27` (133G) or enable auto-reclaim  
3. Notify: **critical** — deviation from schedule, inc is fallback not success  

---

## Packaging & release

**Purpose:** Ship on Fedora (RPM, done) and Debian/Ubuntu (`.deb`, next), with old-school docs and signed releases.

**VMs (Norm has):** Debian 13 — primary build; Ubuntu Desktop 25.10 — install/smoke test.

| ID | Card | Notes |
|----|------|-------|
| PKG-1 | **Man pages** — `backup-tool(1)` + optional config/systemd sections | Old-school `man backup-tool`; derive from clap + README; install via RPM `%files` and `debian/*.install` |
| PKG-2 | **Debian VM setup** — finish build host on Debian 13 | `apt install devscripts debhelper cargo libgtk-4-dev …`; clone repo; confirm `dpkg-buildpackage` toolchain |
| PKG-3 | **`debian/` + `package-deb.sh`** — mirror `backup-tool.spec` | cargo vendor offline build; postinst: gtk cache + systemd `ExecStart` migration; smoke on Ubuntu 25.10 VM |
| PKG-4 | **GPG signing + publish keys** | Generate release key; sign RPM (`rpm --addsign`) + `.deb` (`debsign`); publish public key + SHA256 checksums on GitHub releases; spec TODO at line 1 |
| PKG-5 | **GitHub release artifacts** | Attach signed RPM + `.deb` + `SHA256SUMS` + `backup-tool.gpg` (or `.asc` key export) per tag |

### Packaging kanban

#### Backlog

| ID | Card | Notes |
|----|------|-------|
| PKG-1 | Man pages | |
| PKG-2 | Debian VM setup | Debian 13 primary |
| PKG-3 | debian/ + package-deb.sh | After PKG-2 |
| PKG-4 | GPG signing + publish keys | RPM + deb; checksums on release |
| PKG-5 | GitHub release artifacts | Depends PKG-3, PKG-4 |

#### Ready

| ID | Card | Notes |
|----|------|-------|
| PKG-2 | Debian VM setup | VMs exist; toolchain not verified yet |

#### In Progress / Review / Done

*(empty — pick up PKG-2 when BLC or Monday backup is settled)*

---

## Links

- Runtime defer logic: `src/backup.rs` — `prepare_full_backup`, `FullPrep::DeferredToIncremental`
- Pending full state: `src/pending_full.rs`
- Retention: `prune_all_incrementals`, `apply_full_retention`
- RPM build: `package-rpm.sh`, `backup-tool.spec` (GPG TODO line 1)
- `.deb` plan: MemPalace `backup-tool/debian-packaging-todo`; Understory Backup-Tool concept

*Move cards across sections as work progresses.*
