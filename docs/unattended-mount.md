# Unattended drive mounting (polkit)

The nightly backup runs from a systemd **user** timer. If the backup drive is
not mounted when it fires, Backup-Tool mounts it by UUID with `udisksctl`. This
page explains why that used to fail, what the RPM ships to fix it, and how to
verify it.

## Symptom

`journalctl --user -u backup-tool.service` shows:

```
auto-mounting UUID <uuid>
udisksctl mount failed for /dev/sdX1: Error creating textual authentication agent:
  Error opening current controlling terminal for the process (`/dev/tty'): No such device or address
Error mounting /dev/sdX1: GDBus.Error:org.freedesktop.UDisks2.Error.NotAuthorizedCanObtain:
  Not authorized to perform operation
```

The backup exits with status 1 and nothing is written that run.

## Cause

udisks2 asks polkit for authorization before mounting, and the action it asks
for depends on **who is calling**:

| udisks2 action | Requested when |
|---|---|
| `filesystem-mount` | the caller has an active session on the drive's seat (you are logged in at the console) |
| `filesystem-mount-other-seat` | the caller has **no** active session on the drive's seat |
| `filesystem-mount-system` | udisks2 classifies the device as a system device (`HintSystem=true`: internal disks, anything in fstab) |

A timer that fires at boot, after a resume, or before anyone has logged in has
no session on the seat, so udisks2 asks for `filesystem-mount-other-seat`. The
polkit default for that action is "administrator authentication", and there is
no polkit agent to answer the prompt, hence "Not authorized".

> Why it was missed at first: every early test ran while a user was logged in
> on `seat0`, where udisks2 asks for plain `filesystem-mount`. The gap only
> shows with nobody at the console.

## What the RPM ships

- `/usr/share/polkit-1/rules.d/50-backup-tool.rules` grants
  `filesystem-mount` and `filesystem-mount-other-seat` to members of the
  `backup-tool` group.
- `/usr/lib/sysusers.d/backup-tool.conf` creates the `backup-tool` group
  (`%sysusers_create_package`).
- `%post` adds the installing user (`$SUDO_USER`) to the group. Other users:
  `sudo usermod -aG backup-tool <user>`.

### Scope

Deliberately narrow:

- `filesystem-mount-system` is **not** granted, so internal/system disks still
  need administrator authentication. A group member cannot silently mount them.
- Unmount, eject, format, fstab mounts and every other udisks2 action are not
  granted.

If your backup drive is an internal disk that udisks2 treats as a system device
(`udisksctl info -b /dev/sdX1` shows `HintSystem: true`), add a local rule for
that one drive rather than widening the packaged one. The drive serial is stable
across `/dev/sdX` renames (`udisksctl info -b /dev/sdX1`, or
`udevadm info -q property -n /dev/sdX | grep ID_SERIAL`):

```javascript
// /etc/polkit-1/rules.d/60-backup-tool-internal-disk.rules
polkit.addRule(function(action, subject) {
    if (action.id == "org.freedesktop.udisks2.filesystem-mount-system" &&
        subject.isInGroup("backup-tool") &&
        action.lookup("drive.serial") == "YOUR_DRIVE_SERIAL") {
        return polkit.Result.YES;
    }
});
```

## Check an installation

```bash
id -nG | tr ' ' '\n' | grep -x backup-tool       # you are in the group
grep -c other-seat /usr/share/polkit-1/rules.d/50-backup-tool.rules   # 1 or more
```

`pkcheck` from a session-less process (what the timer is) should authorize the
two mount actions and refuse the rest:

```bash
cat > /tmp/pkcheck-test.sh <<'EOF'
#!/bin/bash
for a in "$@"; do
    pkcheck --action-id "$a" --process $$ >/dev/null 2>&1
    echo "$a rc=$?"        # 0 = authorized, 2 = would need authentication
done
EOF
systemd-run --user --quiet --wait --pipe --collect bash /tmp/pkcheck-test.sh \
    org.freedesktop.udisks2.filesystem-mount \
    org.freedesktop.udisks2.filesystem-mount-other-seat \
    org.freedesktop.udisks2.filesystem-mount-system \
    org.freedesktop.udisks2.filesystem-fstab
rm -f /tmp/pkcheck-test.sh
```

Expected: `filesystem-mount` and `filesystem-mount-other-seat` rc=0; the other two
rc=2.

## Reproduce the unattended case without rebooting

`pkcheck` does not prove udisks2 picks the action you expect, and udisks2 reports
"already mounted" *before* it consults polkit, so the drive must be unmounted.
The drive's seat comes from the udev property `ID_SEAT` of the **disk**, so a
temporary udev rule can move it to a seat nobody is logged in on. This needs
root and a drive nothing is using (close file-manager windows on it first):

```bash
udisksctl unmount -b /dev/sdX1
# Move the drive to seat1, where you have no session:
echo 'SUBSYSTEM=="block", KERNEL=="sdX", ENV{ID_SEAT}="seat1"' |
    sudo tee /etc/udev/rules.d/99-backup-tool-seat-sim.rules
sudo udevadm control --reload && sudo udevadm trigger --action=change /dev/sdX /dev/sdX1
sudo udevadm settle
busctl --system get-property org.freedesktop.UDisks2 \
    "$(udisksctl info -b /dev/sdX1 | sed -n "s/^ *Drive: *'\(.*\)'/\1/p")" \
    org.freedesktop.UDisks2.Drive Seat          # must print: s "seat1"

# Mount from a session-less process, exactly like the timer:
systemd-run --user --quiet --wait --pipe --collect \
    /usr/bin/udisksctl mount -b /dev/sdX1 --no-user-interaction

# Undo:
sudo rm /etc/udev/rules.d/99-backup-tool-seat-sim.rules
sudo udevadm control --reload && sudo udevadm trigger --action=change /dev/sdX /dev/sdX1
```

Result on the development machine (Fedora 44, USB disk, `HintSystem=false`):

| Seat | Rule installed | Result |
|---|---|---|
| `seat0` | current | mounted |
| `seat1` | none | **Not authorized** (the original failure) |
| `seat1` | mount + mount-system only (the earlier rule) | **Not authorized** |
| `seat1` | current (mount + mount-other-seat) | mounted |

## After a reboot

Confirm an unattended run works on a real boot:

```bash
journalctl --user -u backup-tool.service --since today
```

A mount error mentioning polkit means the rule is missing, the user is not in
`backup-tool`, or the drive is a system device (see *Scope* above).
