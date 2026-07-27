#!/usr/bin/env bash
# =============================================================================
# install.sh — Build, install, verify, and uninstall home-backup
#
# Usage:
#   ./install.sh [options] [command]
#
# Commands (default: install):
#   install     Build and install to ~/.local/bin  (default)
#   uninstall   Remove all installed files and disable the systemd timer
#   status      Show what is currently installed and timer state
#
# Options:
#   --system      Install to /usr/local/bin instead of ~/.local/bin (needs sudo)
#   --skip-build  Skip cargo build; use an already-compiled release binary
#   --yes         Skip confirmation prompt during uninstall
#   -h, --help    Show this help
#
# Examples:
#   ./install.sh                   # user install, auto-build
#   ./install.sh --skip-build      # user install, binary already built
#   ./install.sh --system          # system-wide (sudo required)
#   ./install.sh uninstall         # remove everything (prompts for confirmation)
#   ./install.sh uninstall --yes   # remove everything, no prompt
#   ./install.sh status            # show what is installed
# =============================================================================
set -euo pipefail

BINARY_NAME="home-backup"
APP_VERSION="0.1.0"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ── Colour helpers ─────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
    _G='\033[1;32m'; _Y='\033[1;33m'; _R='\033[1;31m'; _B='\033[1;34m'; _X='\033[0m'
else
    _G=''; _Y=''; _R=''; _B=''; _X=''
fi
green()  { printf "${_G}%s${_X}\n"   "$*"; }
yellow() { printf "${_Y}%s${_X}\n"   "$*"; }
blue()   { printf "${_B}%s${_X}\n"   "$*"; }
red()    { printf "${_R}%s${_X}\n"   "$*" >&2; }
die()    { red "ERROR: $*"; exit 1; }
step()   { printf "${_B}  -->  ${_X}%s\n" "$*"; }
ok()     { printf "${_G}  ✓  ${_X}%s\n"  "$*"; }
warn()   { printf "${_Y}  ⚠  ${_X}%s\n"  "$*"; }

# ── Argument parsing ──────────────────────────────────────────────────────────
COMMAND="install"
SYSTEM_INSTALL=false
SKIP_BUILD=false
YES=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        install|uninstall|status) COMMAND="$1" ;;
        --system)      SYSTEM_INSTALL=true ;;
        --skip-build)  SKIP_BUILD=true ;;
        --yes|-y)      YES=true ;;
        --help|-h)
            sed -n '3,29p' "$0" | sed 's/^# \?//'
            exit 0 ;;
        *) die "Unknown argument: '$1'.  Run $0 --help for usage." ;;
    esac
    shift
done

if $SYSTEM_INSTALL; then
    INSTALL_BIN="/usr/local/bin"
    INSTALL_ASSETS="/usr/local/share/${BINARY_NAME}"
    NEED_SUDO=true
else
    INSTALL_BIN="${HOME}/.local/bin"
    INSTALL_ASSETS="${HOME}/.local/share/${BINARY_NAME}"
    NEED_SUDO=false
fi

INSTALLED_BIN="${INSTALL_BIN}/${BINARY_NAME}"
BUILT_BIN="${SCRIPT_DIR}/target/release/${BINARY_NAME}"

DESKTOP_DEST="${HOME}/.local/share/applications/${BINARY_NAME}.desktop"
ICON_DIR_128="${HOME}/.local/share/icons/hicolor/128x128/apps"
ICON_DIR_SVG="${HOME}/.local/share/icons/hicolor/scalable/apps"
ICON_DEST_PNG="${ICON_DIR_128}/${BINARY_NAME}.png"
ICON_DEST_SVG_FILE="${ICON_DIR_SVG}/${BINARY_NAME}.svg"
SYSTEMD_DIR="${HOME}/.config/systemd/user"
SERVICE_FILE="${SYSTEMD_DIR}/${BINARY_NAME}.service"
TIMER_FILE="${SYSTEMD_DIR}/${BINARY_NAME}.timer"
CONFIG_FILE="${HOME}/.config/${BINARY_NAME}/config.toml"
LOG_FILE="${HOME}/.local/share/${BINARY_NAME}/backup.log"

# ─────────────────────────────────────────────────────────────────────────────
# STATUS
# ─────────────────────────────────────────────────────────────────────────────
cmd_status() {
    blue "Home Backup — Installation Status"
    echo
    _cf() { [[ -e "$2" ]] && ok "$1: $2" || warn "$1: not found ($2)"; }
    _cf "Binary       " "${INSTALLED_BIN}"
    _cf "Config       " "${CONFIG_FILE}"
    _cf "Icon (PNG)   " "${ICON_DEST_PNG}"
    _cf "Launcher     " "${DESKTOP_DEST}"
    _cf "Service unit " "${SERVICE_FILE}"
    _cf "Timer unit   " "${TIMER_FILE}"
    echo
    if systemctl --user is-active --quiet "${BINARY_NAME}.timer" 2>/dev/null; then
        ok "Systemd timer: active"
        local nxt
        nxt=$(systemctl --user list-timers "${BINARY_NAME}.timer" \
              --no-pager --no-legend 2>/dev/null | awk '{print $1,$2,$3}' | head -1)
        [[ -n "${nxt}" ]] && printf "     Next run : %s\n" "${nxt}"
    elif systemctl --user is-enabled --quiet "${BINARY_NAME}.timer" 2>/dev/null; then
        warn "Systemd timer: enabled but not active"
    else
        warn "Systemd timer: not installed"
    fi
    echo
    if [[ -f "${LOG_FILE}" ]]; then
        ok "Log: ${LOG_FILE}"
        printf "     Last entry: %s\n" "$(tail -1 "${LOG_FILE}" 2>/dev/null)"
    else
        warn "Log: no backups have run yet"
    fi
    echo
    [[ ":${PATH}:" == *":${INSTALL_BIN}:"* ]] \
        && ok "PATH includes ${INSTALL_BIN}" \
        || warn "PATH does not include ${INSTALL_BIN}"
}

# ─────────────────────────────────────────────────────────────────────────────
# UNINSTALL
# ─────────────────────────────────────────────────────────────────────────────
cmd_uninstall() {
    blue "Home Backup — Uninstall"
    echo
    if ! $YES; then
        echo "  The following will be removed:"
        echo "    • ${INSTALLED_BIN}"
        echo "    • ${DESKTOP_DEST}"
        echo "    • ${ICON_DEST_PNG}  ${ICON_DEST_SVG_FILE}"
        echo "    • ${INSTALL_ASSETS}"
        echo "    • ${SERVICE_FILE}  ${TIMER_FILE}"
        echo
        echo "  Your config and backup snapshots will NOT be touched."
        echo
        read -r -p "  Continue? [y/N] " confirm
        [[ "${confirm,,}" == "y" ]] || { echo "Aborted."; exit 0; }
    fi
    step "Disabling systemd timer…"
    systemctl --user disable --now "${BINARY_NAME}.timer" 2>/dev/null && ok "Timer stopped" || true
    step "Removing systemd units…"
    rm -f "${SERVICE_FILE}" "${TIMER_FILE}"
    systemctl --user daemon-reload 2>/dev/null || true
    ok "Systemd units removed"
    step "Removing binary and assets…"
    rm -f  "${INSTALLED_BIN}"
    rm -rf "${INSTALL_ASSETS}"
    rm -f  "${ICON_DEST_PNG}" "${ICON_DEST_SVG_FILE}"
    ok "Binary and assets removed"
    step "Removing launcher…"
    rm -f "${DESKTOP_DEST}"
    update-desktop-database "${HOME}/.local/share/applications" 2>/dev/null || true
    ok "Launcher removed"
    echo
    green "Uninstall complete."
    echo "  Config preserved at : ${CONFIG_FILE}"
    echo "  Backup data untouched."
}

# ─────────────────────────────────────────────────────────────────────────────
# INSTALL
# ─────────────────────────────────────────────────────────────────────────────
cmd_install() {
    blue "Home Backup v${APP_VERSION} — Install"
    echo

    # ── Dependency checks ────────────────────────────────────────────────
    step "Checking dependencies…"
    _need() { command -v "$1" &>/dev/null || die "Required command not found: '$1'.  ${2:-}"; }
    _need rsync     "Install: sudo dnf install rsync"
    _need lsblk     "Part of util-linux; should already be present."
    _need systemctl "Part of systemd; should already be present."
    if ! $SKIP_BUILD; then
        _need cargo     "Install Rust: https://rustup.rs"
        _need pkg-config "Install: sudo dnf install pkgconf-pkg-config"
        pkg-config --exists gtk4 \
            || die "GTK4 dev headers missing.  Fix: sudo dnf install gtk4-devel"
    fi
    ok "All required tools present"

    # ── Build ────────────────────────────────────────────────────────────
    if $SKIP_BUILD; then
        step "Skipping build (--skip-build)"
        [[ -x "${BUILT_BIN}" ]] \
            || die "No pre-built binary at ${BUILT_BIN}.  Remove --skip-build to compile."
        ok "Using existing binary: ${BUILT_BIN}"
    else
        step "Compiling release binary (first run: ~30–60 s)…"
        cd "${SCRIPT_DIR}"
        if ! cargo build --release --quiet 2>/tmp/hb-cargo-err-$$; then
            red "Build failed:"
            cat /tmp/hb-cargo-err-$$
            rm -f /tmp/hb-cargo-err-$$
            exit 1
        fi
        rm -f /tmp/hb-cargo-err-$$
        ok "Build complete: $(du -h "${BUILT_BIN}" | cut -f1) binary"
    fi

    [[ -x "${INSTALLED_BIN}" ]] && warn "Upgrading existing installation."

    # ── Binary ───────────────────────────────────────────────────────────
    step "Installing binary to ${INSTALL_BIN}…"
    mkdir -p "${INSTALL_BIN}"
    if $NEED_SUDO; then
        sudo install -m 0755 "${BUILT_BIN}" "${INSTALLED_BIN}"
    else
        install -m 0755 "${BUILT_BIN}" "${INSTALLED_BIN}"
    fi
    ok "Binary installed"

    # ── Assets ───────────────────────────────────────────────────────────
    step "Installing assets…"
    mkdir -p "${INSTALL_ASSETS}/assets"
    cp "${SCRIPT_DIR}/assets/"* "${INSTALL_ASSETS}/assets/" 2>/dev/null || true
    ok "Assets copied to ${INSTALL_ASSETS}/assets/"

    # ── Icon ─────────────────────────────────────────────────────────────
    step "Installing icons…"
    mkdir -p "${ICON_DIR_128}" "${ICON_DIR_SVG}"
    local icon_png="${SCRIPT_DIR}/assets/${BINARY_NAME}.png"
    local icon_svg="${SCRIPT_DIR}/assets/${BINARY_NAME}.svg"
    if [[ -f "${icon_png}" ]]; then
        cp "${icon_png}" "${ICON_DEST_PNG}"
        ok "PNG icon installed (128x128)"
    else
        warn "PNG not found — launcher may show a generic icon"
    fi
    [[ -f "${icon_svg}" ]] && cp "${icon_svg}" "${ICON_DEST_SVG_FILE}" && ok "SVG icon installed"
    gtk-update-icon-cache -f -t "${HOME}/.local/share/icons/hicolor" 2>/dev/null || true

    # ── .desktop launcher ────────────────────────────────────────────────
    step "Installing application launcher…"
    mkdir -p "$(dirname "${DESKTOP_DEST}")"
    sed "s|@EXEC@|${INSTALLED_BIN}|g" \
        "${SCRIPT_DIR}/assets/${BINARY_NAME}.desktop" \
        > "${DESKTOP_DEST}"
    update-desktop-database "${HOME}/.local/share/applications" 2>/dev/null || true
    ok "Launcher installed: ${DESKTOP_DEST}"

    # ── Summary ──────────────────────────────────────────────────────────
    echo
    green "═══════════════════════════════════════════"
    green " Installation complete!"
    green "═══════════════════════════════════════════"
    echo
    printf "  %-18s %s\n" "Binary:"   "${INSTALLED_BIN}"
    printf "  %-18s %s\n" "Launcher:" "${DESKTOP_DEST}"
    printf "  %-18s %s\n" "Config:"   "${CONFIG_FILE}  (created on first run)"
    printf "  %-18s %s\n" "Log:"      "${LOG_FILE}"
    echo
    if [[ ":${PATH}:" != *":${INSTALL_BIN}:"* ]]; then
        warn "${INSTALL_BIN} is not in your current PATH."
        echo
        echo "  Add to ~/.bashrc:"
        echo "      export PATH=\"\${HOME}/.local/bin:\${PATH}\""
        echo "  Then: source ~/.bashrc"
        echo
        echo "  Or run directly: ${INSTALLED_BIN}"
    else
        echo "  Run the app : ${BINARY_NAME}"
        echo "  Or find it in your desktop application launcher."
    fi
    echo
    echo "  The first-run wizard lets you choose a backup drive and schedule."
    echo "  Once configured, the systemd timer runs backups automatically."
}

# ── Dispatch ──────────────────────────────────────────────────────────────────
case "${COMMAND}" in
    install)   cmd_install   ;;
    uninstall) cmd_uninstall ;;
    status)    cmd_status    ;;
esac
