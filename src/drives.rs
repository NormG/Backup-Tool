use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{fs, path::{Path, PathBuf}, process::Command};

/// All the information we care about for a single block-device partition.
#[derive(Debug, Clone)]
#[allow(dead_code)] // device_path and fstype reserved for future use
pub struct DriveInfo {
    /// Kernel device name, e.g. `sda1` or `nvme0n1p2`.
    pub name: String,
    /// Filesystem label, if any.
    pub label: Option<String>,
    /// Human-readable size reported by lsblk, e.g. `931.5G`.
    pub size: String,
    /// Current mountpoint, or `None` if not mounted.
    pub mountpoint: Option<String>,
    /// Filesystem type, e.g. `ext4`, `xfs`, `ntfs`.
    pub fstype: Option<String>,
    /// Partition UUID.
    pub uuid: Option<String>,
    /// True when the parent disk is hot-pluggable (USB, eSATA, etc.).
    pub removable: bool,
}

impl DriveInfo {
    /// `/dev/NAME`
    #[allow(dead_code)]
    pub fn device_path(&self) -> String {
        format!("/dev/{}", self.name)
    }

    /// A human-friendly one-liner for display in a dropdown.
    pub fn display_label(&self) -> String {
        let label = self.label.as_deref().unwrap_or("(no label)");
        let mp = match &self.mountpoint {
            Some(m) => format!("mounted at {m}"),
            None => "not mounted".to_string(),
        };
        let hotplug = if self.removable { " ⚡" } else { "" };
        format!(
            "{label} — /dev/{} — {} — {mp}{hotplug}",
            self.name, self.size
        )
    }

    pub fn is_mounted(&self) -> bool {
        self.mountpoint.is_some()
    }
}

// ----- lsblk JSON deserialization structs ------------------------------------

#[derive(Deserialize, Debug)]
struct LsblkRoot {
    blockdevices: Vec<LsblkDevice>,
}

#[derive(Deserialize, Debug)]
struct LsblkDevice {
    name: String,
    label: Option<String>,
    #[serde(default)]
    size: Option<String>,
    mountpoint: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    fstype: Option<String>,
    /// 0 = not removable, 1 = removable
    #[serde(rename = "rm", default)]
    removable: serde_json::Value,
    uuid: Option<String>,
    #[serde(default)]
    children: Option<Vec<LsblkDevice>>,
}

impl LsblkDevice {
    fn is_removable(&self) -> bool {
        match &self.removable {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::Number(n) => n.as_u64().unwrap_or(0) != 0,
            serde_json::Value::String(s) => s == "1" || s.eq_ignore_ascii_case("true"),
            _ => false,
        }
    }
}

// Filesystems suitable for hosting backups.
const USABLE_FS: &[&str] = &[
    "ext4", "ext3", "ext2", "xfs", "btrfs", "ntfs", "exfat", "vfat",
];

// Mountpoints we never want to offer as backup destinations.
// This list blocks the root and home partitions as well as all system paths.
const SYSTEM_MOUNTS: &[&str] = &[
    "/",
    "/home",
    "/boot",
    "/boot/efi",
    "/boot/grub",
    "/boot/grub2",
    "/usr",
    "/var",
    "/tmp",
    "/opt",
    "/srv",
    "[SWAP]",
];

/// Enumerate all block-device partitions that could host backups.
///
/// Skips system-critical mountpoints, swap, and filesystems that do not
/// support the features rsync needs.
pub fn list_drives() -> Result<Vec<DriveInfo>> {
    let out = Command::new("lsblk")
        .args(["-J", "-o", "NAME,LABEL,SIZE,MOUNTPOINT,TYPE,FSTYPE,RM,UUID"])
        .output()
        .context("running lsblk — is it installed?")?;

    if !out.status.success() {
        bail!(
            "lsblk returned non-zero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let root: LsblkRoot = serde_json::from_slice(&out.stdout).context("parsing lsblk JSON")?;

    let mut result = Vec::new();

    fn collect(device: &LsblkDevice, parent_removable: bool, result: &mut Vec<DriveInfo>) {
        let removable = device.is_removable() || parent_removable;

        if device.kind.as_deref() == Some("part") {
            // Skip swap and system mounts.
            if let Some(mp) = &device.mountpoint {
                if SYSTEM_MOUNTS.contains(&mp.as_str()) {
                    return;
                }
            }
            // Only show filesystems we can actually write backups to.
            if let Some(fs) = &device.fstype {
                if USABLE_FS.contains(&fs.as_str()) {
                    result.push(DriveInfo {
                        name: device.name.clone(),
                        label: device.label.clone(),
                        size: device.size.clone().unwrap_or_else(|| "?".to_string()),
                        mountpoint: device.mountpoint.clone(),
                        fstype: device.fstype.clone(),
                        uuid: device.uuid.clone(),
                        removable,
                    });
                }
            }
        }

        if let Some(children) = &device.children {
            for child in children {
                collect(child, removable, result);
            }
        }
    }

    for dev in &root.blockdevices {
        collect(dev, false, &mut result);
    }

    // Stable sort: mounted first, then removable, then alphabetical by name.
    result.sort_by(|a, b| {
        b.is_mounted()
            .cmp(&a.is_mounted())
            .then(b.removable.cmp(&a.removable))
            .then(a.name.cmp(&b.name))
    });

    Ok(result)
}

/// Btrfs write compression for backup volumes.
///
/// `compress-force` packs every new file (the heuristic `compress=` option
/// skips data it thinks will not shrink). zstd level 3 is the usual
/// space/CPU sweet spot for overnight backups.
pub const BTRFS_BACKUP_COMPRESS: &str = "compress-force=zstd:3";

/// Extra `udisksctl mount --options` for a filesystem type, if any.
pub fn extra_mount_options_for_fstype(fstype: &str) -> Option<&'static str> {
    if fstype.eq_ignore_ascii_case("btrfs") {
        Some(BTRFS_BACKUP_COMPRESS)
    } else {
        None
    }
}

/// Resolve a filesystem UUID to a block-device path.
///
/// Prefers udev's `/dev/disk/by-uuid` symlink, which is world-readable and
/// needs no access to the device node, then falls back to `blkid -U`.
fn device_for_uuid(uuid: &str) -> Result<String> {
    // UUIDs are hex plus dashes (or FAT-style `XXXX-XXXX`); never allow path
    // components to leak into the by-uuid lookup.
    if !uuid.is_empty() && uuid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        if let Ok(real) = fs::canonicalize(Path::new("/dev/disk/by-uuid").join(uuid)) {
            return Ok(real.to_string_lossy().into_owned());
        }
    }

    let dev_out = Command::new("blkid")
        .args(["-U", uuid])
        .output()
        .context("running blkid")?;
    if !dev_out.status.success() {
        bail!("blkid could not find a device with UUID {uuid}");
    }
    let device = String::from_utf8_lossy(&dev_out.stdout).trim().to_string();
    if device.is_empty() {
        bail!("UUID {uuid} not found by blkid");
    }
    Ok(device)
}

/// Filesystem type of a block device.
///
/// `lsblk` reads udev's database, so it works for unprivileged users and from
/// systemd user services. `blkid -s TYPE` probes the device node directly and
/// returns nothing for a non-root user when the node is `root:disk 0660`, which
/// silently dropped the btrfs compression mount options.
fn device_fstype(device: &str) -> Option<String> {
    let out = Command::new("lsblk")
        .args(["-n", "-o", "FSTYPE", device])
        .output()
        .ok()?;
    if out.status.success() {
        if let Some(line) = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
        {
            return Some(line.to_string());
        }
    }
    blkid_value(device, "TYPE")
}

/// Arguments for `udisksctl` to mount `device`, adding filesystem-specific
/// options (e.g. btrfs zstd compression) when the type is known.
fn udisksctl_mount_args(device: &str, fstype: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "mount".to_string(),
        "--block-device".to_string(),
        device.to_string(),
        "--no-user-interaction".to_string(),
    ];
    if let Some(opts) = fstype.and_then(extra_mount_options_for_fstype) {
        args.push("--options".to_string());
        args.push(opts.to_string());
    }
    args
}

fn blkid_value(device: &str, field: &str) -> Option<String> {
    let out = Command::new("blkid")
        .args(["-o", "value", "-s", field, device])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Attempt to mount a partition by UUID using `udisksctl` (no root required).
/// Returns the mountpoint on success. Btrfs volumes are mounted with
/// [`BTRFS_BACKUP_COMPRESS`] so rsync writes land compressed.
pub fn mount_by_uuid(uuid: &str) -> Result<String> {
    // Resolve UUID → device path
    let device = device_for_uuid(uuid)?;

    let fstype = device_fstype(&device);
    let args = udisksctl_mount_args(&device, fstype.as_deref());

    // Mount via udisksctl
    let mount_out = Command::new("udisksctl")
        .args(&args)
        .output()
        .context("running udisksctl")?;

    if !mount_out.status.success() {
        let stderr = String::from_utf8_lossy(&mount_out.stderr);
        let stderr = stderr.trim();
        let hint = if stderr.contains("Not authorized") || stderr.contains("NotAuthorized") {
            "\nHint: polkit denied the mount. Ensure the user is in the 'backup-tool' \
             group (sudo usermod -aG backup-tool $USER) and that \
             /usr/share/polkit-1/rules.d/50-backup-tool.rules is installed \
             and grants filesystem-mount-other-seat."
        } else {
            ""
        };
        bail!("udisksctl mount failed for {device}: {stderr}{hint}");
    }

    // udisksctl prints "Mounted /dev/sdX at /run/media/user/LABEL.\n"
    let stdout = String::from_utf8_lossy(&mount_out.stdout);
    let mountpoint = stdout
        .split(" at ")
        .nth(1)
        .map(|s| s.trim_end_matches(".\n").trim().to_string())
        .context("parsing udisksctl mount output")?;

    Ok(mountpoint)
}

/// `true` when a mount-option string (as printed by `findmnt -o OPTIONS`)
/// forces zstd compression.
fn mount_options_force_zstd(options: &str) -> bool {
    options
        .split(',')
        .any(|o| o.trim().starts_with("compress-force=zstd"))
}

/// Parse the output of `btrfs property get <path> compression`
/// (`compression=zstd`) into the algorithm name.
fn parse_btrfs_compression_property(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|l| l.trim().strip_prefix("compression="))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn btrfs_get_compression(path: &Path) -> Option<String> {
    let out = Command::new("btrfs")
        .args(["property", "get"])
        .arg(path)
        .arg("compression")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_btrfs_compression_property(&String::from_utf8_lossy(&out.stdout))
}

fn btrfs_set_zstd_property(path: &Path) -> std::result::Result<(), String> {
    let out = Command::new("btrfs")
        .args(["property", "set"])
        .arg(path)
        .args(["compression", "zstd"])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn mount_options_for(path: &Path) -> Option<String> {
    let resolved = existing_ancestor(path).ok()?;
    let out = Command::new("findmnt")
        .args(["--noheadings", "-o", "OPTIONS", "--target"])
        .arg(resolved)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Make new backup data on a btrfs destination compress with zstd.
///
/// The mount option ([`BTRFS_BACKUP_COMPRESS`]) is the strongest setting, but
/// the volume may have been mounted by something else (a desktop automount, an
/// older version of this tool), so the per-directory `compression` property is
/// also set on the volume root and the destination. New snapshot directories
/// inherit it from their parent.
///
/// This deliberately does **not** run `chattr +c`: on btrfs that flag sets the
/// directory's compression property to **zlib**, undoing the zstd property.
///
/// Best-effort: the result is a log line, never an error, so a backup still
/// runs.
pub fn enable_btrfs_dest_compression(dest: &Path) -> Option<String> {
    let fstype = detect_fstype(&dest.to_string_lossy())?;
    if !fstype.eq_ignore_ascii_case("btrfs") {
        return None;
    }

    let mut notes = Vec::new();

    match mount_options_for(dest) {
        Some(opts) if mount_options_force_zstd(&opts) => {
            notes.push("mounted with compress-force=zstd".to_string());
        }
        _ => notes.push(
            "WARNING: volume not mounted with compress-force=zstd \
             (relying on directory property; heuristic may skip incompressible data)"
                .to_string(),
        ),
    }

    // Volume root first (best-effort; may not be owned by the user), then the
    // destination, which is what new snapshot directories inherit from.
    if let Ok(mp) = filesystem_mountpoint(dest) {
        if mp != dest {
            let _ = btrfs_set_zstd_property(&mp);
        }
    }
    match btrfs_set_zstd_property(dest) {
        Ok(()) => {}
        Err(e) => notes.push(format!("btrfs property set failed: {e}")),
    }

    match btrfs_get_compression(dest) {
        Some(algo) if algo == "zstd" => notes.push("dest property compression=zstd".to_string()),
        Some(algo) => notes.push(format!(
            "WARNING: dest property compression={algo} (expected zstd)"
        )),
        None => notes.push("WARNING: could not read dest compression property".to_string()),
    }

    Some(format!(
        "btrfs dest compression ({BTRFS_BACKUP_COMPRESS}): {}",
        notes.join("; ")
    ))
}

/// Return the filesystem type (e.g. `"btrfs"`, `"ext4"`) of the partition
/// that contains `path`, or `None` when `findmnt` fails or produces no output.
pub fn detect_fstype(path: &str) -> Option<String> {
    // Walk up to the nearest existing ancestor.
    let resolved = {
        let mut p = std::path::Path::new(path);
        loop {
            if p.exists() {
                break p.to_path_buf();
            }
            p = p.parent()?;
        }
    };
    let out = Command::new("findmnt")
        .args([
            "--noheadings",
            "-o",
            "FSTYPE",
            "--target",
            resolved.to_string_lossy().as_ref(),
        ])
        .output()
        .ok()?;
    let fstype = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if fstype.is_empty() {
        None
    } else {
        Some(fstype)
    }
}

/// Returns `true` when `a` and `b` reside on the **same filesystem** (same
/// kernel device number).  Walks up to the nearest existing ancestor if
/// either path does not yet exist, so it works for proposed backup destinations
/// that haven't been created yet.
pub fn is_same_device(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let dev_of = |p: &std::path::Path| -> Option<u64> {
        let mut cur = p;
        loop {
            match std::fs::metadata(cur) {
                Ok(m) => return Some(m.dev()),
                Err(_) => cur = cur.parent()?,
            }
        }
    };
    matches!((dev_of(a), dev_of(b)), (Some(da), Some(db)) if da == db)
}

/// Detect the filesystem label of the partition that **contains** `path`.
///
/// Useful when `Config::drive_label` is `None` (e.g. the user typed the
/// destination path manually rather than choosing from the dropdown).
/// Uses `findmnt` to resolve the block device and `lsblk` to read its label.
/// Returns `None` when either tool fails or the label is empty.
pub fn detect_label_for_path(path: &str) -> Option<String> {
    // Walk up to the nearest existing ancestor so it works on paths that
    // haven't been created yet.
    let resolved = {
        let mut p = std::path::Path::new(path);
        loop {
            if p.exists() {
                break p.to_path_buf();
            }
            p = p.parent()?;
        }
    };

    // Find the source device for the resolved path.
    let dev_out = Command::new("findmnt")
        .args([
            "--noheadings",
            "-o",
            "SOURCE",
            "--target",
            resolved.to_string_lossy().as_ref(),
        ])
        .output()
        .ok()?;
    let source = String::from_utf8_lossy(&dev_out.stdout).trim().to_string();
    if source.is_empty() {
        return None;
    }

    // Query the label for that device.
    let lbl_out = Command::new("lsblk")
        .args(["-n", "-o", "LABEL", &source])
        .output()
        .ok()?;
    let label = String::from_utf8_lossy(&lbl_out.stdout).trim().to_string();
    if label.is_empty() || label == "-" || label == "(null)" {
        None
    } else {
        Some(label)
    }
}

/// Available and total bytes on the filesystem containing `path`.
pub fn filesystem_bytes(path: &Path) -> Result<(u64, u64)> {
    let out = Command::new("df")
        .args(["--output=avail,size", "-B1", "--"])
        .arg(path)
        .output()
        .with_context(|| format!("running df on {}", path.display()))?;

    if !out.status.success() {
        bail!(
            "df failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let fields: Vec<&str> = stdout
        .lines()
        .nth(1)
        .context("parsing df output")?
        .split_whitespace()
        .collect();

    if fields.len() < 2 {
        bail!("unexpected df output for {}", path.display());
    }

    let avail = fields[0]
        .parse::<u64>()
        .with_context(|| format!("parsing df avail bytes '{}'", fields[0]))?;
    let total = fields[1]
        .parse::<u64>()
        .with_context(|| format!("parsing df size bytes '{}'", fields[1]))?;
    Ok((avail, total))
}

/// Mountpoint of the filesystem containing `path`.
pub fn filesystem_mountpoint(path: &Path) -> Result<std::path::PathBuf> {
    let resolved = existing_ancestor(path)?;
    let out = Command::new("findmnt")
        .args([
            "--noheadings",
            "-o",
            "TARGET",
            "--target",
            resolved.to_string_lossy().as_ref(),
        ])
        .output()
        .with_context(|| format!("running findmnt for {}", path.display()))?;

    if !out.status.success() {
        bail!(
            "findmnt failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let mount = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if mount.is_empty() {
        bail!("findmnt returned no mountpoint for {}", path.display());
    }
    Ok(std::path::PathBuf::from(mount))
}

/// Empty freedesktop trash directories on the backup volume before space checks.
///
/// Targets `.Trash-{uid}` / `.Trash/{uid}` at the filesystem mount root so
/// deleted snapshots in Nautilus do not inflate `df` used space.
pub fn empty_volume_trash(path: &Path) -> Result<u64> {
    let mount = filesystem_mountpoint(path)?;
    let uid = current_uid()?;
    empty_user_trash_on_volume(&mount, uid)
}

fn existing_ancestor(path: &Path) -> Result<std::path::PathBuf> {
    let mut cur = path;
    loop {
        if cur.exists() {
            return Ok(cur.to_path_buf());
        }
        cur = cur
            .parent()
            .with_context(|| format!("no existing ancestor for {}", path.display()))?;
    }
}

fn current_uid() -> Result<u32> {
    let out = Command::new("id")
        .args(["-u"])
        .output()
        .context("running id -u")?;
    if !out.status.success() {
        bail!(
            "id -u failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let uid = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u32>()
        .context("parsing uid from id -u")?;
    Ok(uid)
}

fn stable_mount_path(mount_root: &Path) -> Result<PathBuf> {
    match mount_root.canonicalize() {
        Ok(p) => Ok(p),
        Err(e) => {
            if mount_root.is_absolute() && mount_root.exists() {
                Ok(mount_root.to_path_buf())
            } else {
                Err(e)
                    .with_context(|| format!("canonicalizing mount root {}", mount_root.display()))
            }
        }
    }
}

fn empty_user_trash_on_volume(mount_root: &Path, uid: u32) -> Result<u64> {
    let canon_mount = stable_mount_path(mount_root)?;
    let volume_mount = filesystem_mountpoint(&canon_mount)?;

    let mut freed = 0u64;
    let trash_dirs = [
        mount_root.join(format!(".Trash-{uid}")),
        mount_root.join(".Trash").join(uid.to_string()),
    ];

    for trash_root in &trash_dirs {
        if !trash_root.exists() {
            continue;
        }
        let meta = fs::symlink_metadata(trash_root)
            .with_context(|| format!("reading trash metadata {}", trash_root.display()))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        let canon_trash = trash_root
            .canonicalize()
            .unwrap_or_else(|_| trash_root.to_path_buf());
        if !canon_trash.starts_with(&canon_mount) || !on_volume_mount(&canon_trash, &volume_mount)?
        {
            continue;
        }
        freed += clear_trash_root(&canon_trash, &canon_mount, &volume_mount)?;
    }

    Ok(freed)
}

fn clear_trash_root(trash_root: &Path, mount_root: &Path, volume_mount: &Path) -> Result<u64> {
    let mut freed = 0u64;
    for sub in ["files", "expunged"] {
        let dir = trash_root.join(sub);
        if !dir.exists() {
            continue;
        }
        let meta = fs::symlink_metadata(&dir)
            .with_context(|| format!("reading trash subdir metadata {}", dir.display()))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        let canon_dir = dir
            .canonicalize()
            .with_context(|| format!("canonicalizing trash subdir {}", dir.display()))?;
        if !canon_dir.starts_with(mount_root) || !on_volume_mount(&canon_dir, volume_mount)? {
            continue;
        }

        freed += dir_disk_usage_bytes(&canon_dir).unwrap_or(0);
        for entry in fs::read_dir(&canon_dir)? {
            let entry = entry?;
            let path = canon_dir.join(entry.file_name());
            remove_trash_entry(&path, volume_mount)
                .with_context(|| format!("removing trash entry {}", path.display()))?;
        }
    }
    Ok(freed)
}

fn on_volume_mount(path: &Path, volume_mount: &Path) -> Result<bool> {
    Ok(filesystem_mountpoint(path)? == volume_mount)
}

fn remove_trash_entry(path: &Path, volume_mount: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("reading trash entry metadata {}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if !on_volume_mount(path, volume_mount)? {
        return Ok(());
    }
    if meta.is_dir() {
        remove_tree_on_volume(path, volume_mount)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Delete a directory tree without crossing bind mounts or nested filesystems.
fn remove_tree_on_volume(path: &Path, volume_mount: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if !meta.is_dir() || !on_volume_mount(path, volume_mount)? {
        return Ok(());
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = path.join(entry.file_name());
        remove_trash_entry(&child, volume_mount)?;
    }

    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() && on_volume_mount(path, volume_mount)? {
        fs::remove_dir(path)?;
    }
    Ok(())
}

fn dir_disk_usage_bytes(path: &Path) -> Result<u64> {
    let out = Command::new("du")
        .args(["-sb", "--"])
        .arg(path)
        .output()
        .with_context(|| format!("running du on {}", path.display()))?;

    if !out.status.success() {
        bail!(
            "du failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let field = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .context("parsing du output")?
        .to_string();

    field
        .parse::<u64>()
        .with_context(|| format!("parsing du bytes '{field}'"))
}

/// Find the current mountpoint of a partition with the given UUID by querying
/// `/proc/mounts`.  Returns `None` if not currently mounted.
pub fn find_mountpoint_by_uuid(uuid: &str) -> Option<String> {
    let out = Command::new("findmnt")
        .args(["--noheadings", "-o", "TARGET", &format!("UUID={uuid}")])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_mount_options_force_zstd_on_btrfs_only() {
        assert_eq!(
            extra_mount_options_for_fstype("btrfs"),
            Some(BTRFS_BACKUP_COMPRESS)
        );
        assert_eq!(
            extra_mount_options_for_fstype("BTRFS"),
            Some(BTRFS_BACKUP_COMPRESS)
        );
        assert!(extra_mount_options_for_fstype("ext4").is_none());
        assert!(BTRFS_BACKUP_COMPRESS.contains("compress-force=zstd"));
    }

    #[test]
    fn udisksctl_args_add_compression_options_for_btrfs() {
        let args = udisksctl_mount_args("/dev/sda1", Some("btrfs"));
        assert_eq!(
            args,
            [
                "mount",
                "--block-device",
                "/dev/sda1",
                "--no-user-interaction",
                "--options",
                BTRFS_BACKUP_COMPRESS,
            ]
        );
    }

    #[test]
    fn udisksctl_args_have_no_options_for_other_or_unknown_fs() {
        for fs in [Some("ext4"), Some("vfat"), None] {
            let args = udisksctl_mount_args("/dev/sdb1", fs);
            assert!(!args.iter().any(|a| a == "--options"), "fs={fs:?}");
        }
    }

    #[test]
    fn device_for_uuid_rejects_path_traversal() {
        // Must not canonicalize an escaping path; falls through to blkid,
        // which will not know this UUID.
        assert!(device_for_uuid("../../etc/passwd").is_err());
    }

    #[test]
    fn mount_options_force_zstd_detection() {
        assert!(mount_options_force_zstd(
            "rw,nosuid,nodev,relatime,compress-force=zstd:3,space_cache=v2"
        ));
        assert!(mount_options_force_zstd("rw,compress-force=zstd"));
        assert!(!mount_options_force_zstd("rw,nosuid,nodev,relatime,space_cache=v2"));
        assert!(!mount_options_force_zstd("rw,compress=zstd:3"));
        assert!(!mount_options_force_zstd("rw,compress-force=zlib"));
    }

    #[test]
    fn parse_btrfs_compression_property_values() {
        assert_eq!(
            parse_btrfs_compression_property("compression=zstd\n").as_deref(),
            Some("zstd")
        );
        assert_eq!(
            parse_btrfs_compression_property("compression=zlib").as_deref(),
            Some("zlib")
        );
        assert_eq!(parse_btrfs_compression_property(""), None);
        assert_eq!(parse_btrfs_compression_property("compression=\n"), None);
    }

    #[test]
    fn empty_user_trash_on_volume_removes_files_and_expunged() {
        let mount =
            std::env::temp_dir().join(format!("backup-tool-trash-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&mount);
        let trash = mount.join(".Trash-1000");
        let files = trash.join("files").join("deleted-snapshot");
        let expunged = trash.join("expunged").join("751706442");
        fs::create_dir_all(&files).unwrap();
        fs::create_dir_all(&expunged).unwrap();
        fs::write(files.join("note.txt"), b"gone").unwrap();
        fs::write(expunged.join("big.bin"), vec![0u8; 4096]).unwrap();

        let freed = empty_user_trash_on_volume(&mount, 1000).unwrap();
        assert!(freed >= 4096);
        assert!(!files.exists());
        assert!(!expunged.exists());

        let _ = fs::remove_dir_all(mount);
    }

    #[test]
    fn empty_user_trash_skips_symlinked_trash_root() {
        let mount =
            std::env::temp_dir().join(format!("backup-tool-trash-symlink-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("backup-tool-trash-outside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&mount);
        let _ = fs::remove_dir_all(&outside);

        fs::create_dir_all(&mount).unwrap();
        fs::create_dir_all(outside.join("files")).unwrap();
        fs::write(outside.join("files").join("keep-me.txt"), b"stay").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&outside, mount.join(".Trash-1000")).unwrap();
        }

        let freed = empty_user_trash_on_volume(&mount, 1000).unwrap();
        assert_eq!(freed, 0);
        assert!(outside.join("files").join("keep-me.txt").exists());

        let _ = fs::remove_dir_all(mount);
        let _ = fs::remove_dir_all(outside);
    }
}
