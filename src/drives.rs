use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::process::Command;

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
    "/", "/home", "/boot", "/boot/efi", "/boot/grub", "/boot/grub2",
    "/usr", "/var", "/tmp", "/opt", "/srv",
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

/// Attempt to mount a partition by UUID using `udisksctl` (no root required).
/// Returns the mountpoint on success.
pub fn mount_by_uuid(uuid: &str) -> Result<String> {
    // Resolve UUID → device path
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

    // Mount via udisksctl
    let mount_out = Command::new("udisksctl")
        .args(["mount", "--block-device", &device, "--no-user-interaction"])
        .output()
        .context("running udisksctl")?;

    if !mount_out.status.success() {
        bail!(
            "udisksctl mount failed for {device}: {}",
            String::from_utf8_lossy(&mount_out.stderr).trim()
        );
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
            if p.exists() { break p.to_path_buf(); }
            p = p.parent()?;
        }
    };

    // Find the source device for the resolved path.
    let dev_out = Command::new("findmnt")
        .args(["--noheadings", "-o", "SOURCE", "--target",
               &resolved.to_string_lossy().into_owned()])
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
