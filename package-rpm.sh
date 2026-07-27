#!/usr/bin/env bash
# =============================================================================
# package-rpm.sh — Build an RPM package for home-backup
#
# Usage:
#   ./package-rpm.sh            # full build (vendors deps + compiles)
#   ./package-rpm.sh --no-vendor  # skip cargo vendor (vendor dir must exist)
#
# Output: ~/rpmbuild/RPMS/x86_64/home-backup-*.rpm
#         ~/rpmbuild/SRPMS/home-backup-*.src.rpm
# =============================================================================
set -euo pipefail

NAME="home-backup"
VERSION="0.1.3"
RELEASE="1"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ── Colour helpers ─────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
    G='\033[1;32m'; Y='\033[1;33m'; B='\033[1;34m'; R='\033[1;31m'; X='\033[0m'
else
    G=''; Y=''; B=''; R=''; X=''
fi
step()  { printf "${B}  -->  ${X}%s\n" "$*"; }
ok()    { printf "${G}  ✓  ${X}%s\n"  "$*"; }
warn()  { printf "${Y}  ⚠  ${X}%s\n"  "$*"; }
die()   { printf "${R}ERROR: ${X}%s\n" "$*" >&2; exit 1; }

NO_VENDOR=false
for arg in "$@"; do
    case "$arg" in
        --no-vendor) NO_VENDOR=true ;;
        *) die "Unknown argument: $arg" ;;
    esac
done

# ── Dependency checks ──────────────────────────────────────────────────────────
step "Checking build dependencies…"
for cmd in cargo rpmbuild rpmdev-setuptree; do
    command -v "$cmd" &>/dev/null \
        || die "'$cmd' not found.  Install with: sudo dnf install rpm-build rpmdevtools cargo"
done
ok "Build tools present"

# ── rpmbuild directory tree ───────────────────────────────────────────────────
step "Setting up ~/rpmbuild tree…"
rpmdev-setuptree
ok "~/rpmbuild ready"

# ── Cargo vendor ─────────────────────────────────────────────────────────────
cd "${SCRIPT_DIR}"

if $NO_VENDOR; then
    warn "--no-vendor: skipping cargo vendor (vendor/ must already exist)"
else
    step "Vendoring Rust dependencies (this downloads ~50 MB on first run)…"
    # Remove stale vendor dir before re-vendoring.
    rm -rf vendor
    cargo vendor vendor
    ok "Rust dependencies vendored into vendor/"
fi

[[ -d vendor ]] || die "vendor/ directory missing.  Run without --no-vendor first."

# ── Source tarballs ───────────────────────────────────────────────────────────
step "Creating source tarballs…"

SRC_TAR="${NAME}-${VERSION}.tar.gz"
VENDOR_TAR="${NAME}-${VERSION}-vendor.tar.gz"
SOURCES=~/rpmbuild/SOURCES

# Source0: project source (exclude build artifacts and vendor dir)
tar czf "${SOURCES}/${SRC_TAR}" \
    --transform "s|^\.|${NAME}-${VERSION}|" \
    --exclude='./.git'          \
    --exclude='./target'        \
    --exclude='./vendor'        \
    --exclude='./.cargo'        \
    --exclude='./*.rpm'         \
    .
ok "Source tarball: ${SOURCES}/${SRC_TAR}"

# Source1: vendored Rust crates
tar czf "${SOURCES}/${VENDOR_TAR}" vendor
ok "Vendor tarball: ${SOURCES}/${VENDOR_TAR}"

# ── Spec file ─────────────────────────────────────────────────────────────────
step "Copying spec file…"
cp "${SCRIPT_DIR}/${NAME}.spec" ~/rpmbuild/SPECS/
ok "Spec: ~/rpmbuild/SPECS/${NAME}.spec"

# ── Build ─────────────────────────────────────────────────────────────────────
step "Building RPM (this compiles the Rust binary — ~40 s)…"
rpmbuild -ba \
    --define "dist .fc$(rpm -E '%{fedora}')" \
    ~/rpmbuild/SPECS/${NAME}.spec 2>&1

# ── Results ───────────────────────────────────────────────────────────────────
echo
printf "${G}═══════════════════════════════════════════════${X}\n"
printf "${G} RPM build complete!${X}\n"
printf "${G}═══════════════════════════════════════════════${X}\n"
echo
echo "  Binary RPM:"
find ~/rpmbuild/RPMS -name "${NAME}-${VERSION}*.rpm" | sort | sed 's/^/    /'
echo
echo "  Source RPM:"
find ~/rpmbuild/SRPMS -name "${NAME}-${VERSION}*.rpm" | sort | sed 's/^/    /'
echo
echo "  Install:"
echo "    sudo dnf install $(find ~/rpmbuild/RPMS -name "${NAME}-${VERSION}*.rpm" | head -1)"
echo
echo "  Or to test without installing:"
echo "    rpm -qpl $(find ~/rpmbuild/RPMS -name "${NAME}-${VERSION}*.rpm" | head -1)"
