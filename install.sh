#!/usr/bin/env bash
#
# install.sh - Build Loupe from source and install it into /usr/local.
#
# The system package (/usr/bin/loupe from RPM) is left untouched. Because
# /usr/local/bin and /usr/local/share come before /usr/bin and /usr/share in
# PATH / XDG_DATA_DIRS, the locally built copy shadows the system one.
#
# Bỏa đảm an toàn:
#   - Build chạy bằng user thường (cargo không chạy bằng root).
#   - Chỉ bước "meson install" mới dùng sudo, với --no-rebuild.
#   - Có thể gỡ sạch bằng ./install.sh --uninstall.
#
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BUILD_DIR="${SCRIPT_DIR}/build"
STATE_DIR="${SCRIPT_DIR}/.install-state"
PREFIX="/usr/local"
PROFILE="release"
JOBS="$(nproc 2>/dev/null || echo 4)"

INSTALL_DEPS=1
CLEAN=0
UNINSTALL=0
BUILD_ONLY=0
DEPS_ONLY=0
RUN_AFTER=0

if [ "$(id -u)" -eq 0 ]; then
    SUDO=()
else
    SUDO=(sudo)
fi

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[!]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[x]\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<EOF
Cách dùng: ./install.sh [tuỳ chọn]

Không có tuỳ chọn  Build + cài (hoặc reinstall) vào $PREFIX.
                 Chạy lại lệnh này sau mỗi lần sửa source là đủ.

  --prefix PATH  Tiền tố cài đặt (mặc định: /usr/local).
  --build-dir P  Thư mục build (mặc định: <repo>/build).
  --debug        Dùng Cargo profile "dev" cho build nhanh khi đang sửa code.
  -j N           Số job song song cho ninja (mặc định: nproc).
  --clean        Xoá sạch build dir trước khi build (build lại từ đầu).
  --no-deps      Không tự cài dependency còn thiếu.
  --deps-only    Chỉ kiểm tra/cài dependency rồi thoát.
  --build-only   Build nhưng không cài vào hệ thống.
  --uninstall    Gỡ bản đã cài trong $PREFIX (bản RPM hệ thống vẫn còn).
  --run          Mở Loupe sau khi cài xong.
  -h, --help     Hiện trợ giúp này.

Ví dụ:
  ./install.sh                 # build + cài / reinstall
  ./install.sh --debug --run   # build nhanh bản debug rồi chạy thử
  ./install.sh --clean         # build lại hoàn toàn từ đầu
  ./install.sh --uninstall     # quay về Loupe của hệ thống
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        -h|--help)     usage; exit 0 ;;
        --prefix)      [ "$#" -ge 2 ] || die "Thiếu giá trị cho --prefix"
                       PREFIX="$2"; shift 2 ;;
        --prefix=*)    PREFIX="${1#*=}"; shift ;;
        --build-dir)   [ "$#" -ge 2 ] || die "Thiếu giá trị cho --build-dir"
                       BUILD_DIR="$2"; shift 2 ;;
        --build-dir=*) BUILD_DIR="${1#*=}"; shift ;;
        --debug)       PROFILE="dev"; shift ;;
        -j|--jobs)     [ "$#" -ge 2 ] || die "Thiếu giá trị cho -j"
                       JOBS="$2"; shift 2 ;;
        -j*)           JOBS="${1#-j}"; shift ;;
        --clean)       CLEAN=1; shift ;;
        --no-deps)     INSTALL_DEPS=0; shift ;;
        --deps-only)   DEPS_ONLY=1; shift ;;
        --build-only)  BUILD_ONLY=1; shift ;;
        --uninstall)   UNINSTALL=1; shift ;;
        --run)         RUN_AFTER=1; shift ;;
        *)             die "Tuỳ chọn không hợp lệ: $1 (xem --help)" ;;
    esac
done

[[ "$PREFIX" = /* ]] || die "--prefix phải là đường dẫn tuyệt đối: $PREFIX"
[[ "$JOBS" =~ ^[0-9]+$ ]] && [ "$JOBS" -gt 0 ] || die "-j phải là số nguyên dương"

BUILD_DIR="$(realpath -m -- "$BUILD_DIR")"
[ "$BUILD_DIR" != "/" ] || die "Từ chối dùng / làm build dir"
[ "$BUILD_DIR" != "$SCRIPT_DIR" ] || die "Build dir không được trùng thư mục source"

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------
refresh_caches() {
    local schema_dir="$PREFIX/share/glib-2.0/schemas"
    local icon_dir="$PREFIX/share/icons/hicolor"
    local app_dir="$PREFIX/share/applications"

    if [ -d "$icon_dir" ] && command -v gtk-update-icon-cache >/dev/null 2>&1; then
        "${SUDO[@]}" gtk-update-icon-cache -q -t -f "$icon_dir" 2>/dev/null || true
    fi
    if [ -d "$app_dir" ] && command -v update-desktop-database >/dev/null 2>&1; then
        "${SUDO[@]}" update-desktop-database -q "$app_dir" 2>/dev/null || true
    fi
    if [ -d "$schema_dir" ]; then
        if compgen -G "$schema_dir/*.gschema.xml" >/dev/null; then
            "${SUDO[@]}" glib-compile-schemas "$schema_dir" 2>/dev/null || true
        else
            "${SUDO[@]}" rm -f "$schema_dir/gschemas.compiled"
        fi
    fi
}

if [ "$UNINSTALL" -eq 1 ]; then
    log "Gỡ bản Loupe đã cài trong $PREFIX..."
    SAVED_LOG="$STATE_DIR/install-files.txt"

    if [ -f "$BUILD_DIR/build.ninja" ]; then
        "${SUDO[@]}" ninja -C "$BUILD_DIR" uninstall
    elif [ -f "$SAVED_LOG" ]; then
        warn "Không có build dir; dùng danh sách file đã lưu để gỡ."
        while IFS= read -r path; do
            [ -n "$path" ] || continue
            case "$(realpath -m -- "$path")" in
                "$PREFIX"/*) "${SUDO[@]}" rm -f -- "$path" || warn "Không xoá được: $path" ;;
            esac
        done < "$SAVED_LOG"
    else
        die "Không tìm thấy thông tin cài đặt (build dir hoặc $SAVED_LOG)."
    fi

    rm -f "$SAVED_LOG"
    rmdir "$STATE_DIR" 2>/dev/null || true
    pkill -x loupe 2>/dev/null || true
    refresh_caches

    log "Đã gỡ. Từ giờ 'loupe' sẽ dùng lại bản hệ thống (/usr/bin/loupe)."
    exit 0
fi

# ---------------------------------------------------------------------------
# Dependencies
# ---------------------------------------------------------------------------
pkg_exists() { pkg-config --exists "$1" 2>/dev/null; }

MISSING=()
command -v cargo        >/dev/null 2>&1 || MISSING+=(cargo)
command -v meson        >/dev/null 2>&1 || MISSING+=(meson)
command -v ninja        >/dev/null 2>&1 || MISSING+=(ninja-build)
command -v msgfmt       >/dev/null 2>&1 || MISSING+=(gettext)
command -v itstool      >/dev/null 2>&1 || MISSING+=(itstool)
pkg_exists gtk4                   || MISSING+=(gtk4-devel)
pkg_exists libadwaita-1           || MISSING+=(libadwaita-devel)
pkg_exists gweather4              || MISSING+=(libgweather-devel)
pkg_exists libseccomp             || MISSING+=(libseccomp-devel)

if [ "${#MISSING[@]}" -gt 0 ]; then
    mapfile -t MISSING < <(printf '%s\n' "${MISSING[@]}" | sort -u)
    warn "Thiếu dependency: ${MISSING[*]}"

    if [ "$INSTALL_DEPS" -eq 1 ] && command -v dnf >/dev/null 2>&1; then
        log "Cài dependency bằng dnf..."
        "${SUDO[@]}" dnf install -y "${MISSING[@]}"
    else
        die "Hãy cài thủ công: sudo dnf install -y ${MISSING[*]}"
    fi
else
    log "Dependency đã đủ."
fi

if [ "$DEPS_ONLY" -eq 1 ]; then
    log "Xong (--deps-only)."
    exit 0
fi

# ---------------------------------------------------------------------------
# Configure + build
# ---------------------------------------------------------------------------
if [ "$CLEAN" -eq 1 ] && [ -d "$BUILD_DIR" ]; then
    log "Xoá build dir: $BUILD_DIR"
    rm -rf -- "$BUILD_DIR"
fi

MESON_ARGS=(--prefix="$PREFIX" -Dprofile="$PROFILE")

if [ -f "$BUILD_DIR/build.ninja" ]; then
    log "Cấu hình lại build dir (profile=$PROFILE, prefix=$PREFIX)..."
    meson setup --reconfigure "$BUILD_DIR" "${MESON_ARGS[@]}"
else
    log "Cấu hình build dir (profile=$PROFILE, prefix=$PREFIX)..."
    meson setup "$BUILD_DIR" "${MESON_ARGS[@]}"
fi

log "Build Loupe (jobs=$JOBS). Lần đầu sẽ tải crate từ crates.io..."
ninja -C "$BUILD_DIR" -j "$JOBS"

BINARY="$BUILD_DIR/src/loupe"
[ -x "$BINARY" ] || die "Build xong nhưng không thấy binary: $BINARY"

if [ "$BUILD_ONLY" -eq 1 ]; then
    log "Build xong (--build-only): $BINARY"
    exit 0
fi

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------
if pgrep -x loupe >/dev/null 2>&1; then
    log "Dừng Loupe đang chạy để bản mới được dùng..."
    pkill -x loupe 2>/dev/null || true
    sleep 1
fi

log "Cài vào $PREFIX..."
"${SUDO[@]}" meson install -C "$BUILD_DIR" --no-rebuild

# Lưu install log để --uninstall vẫn dùng được sau khi --clean.
if [ -f "$BUILD_DIR/meson-logs/install-log.txt" ]; then
    mkdir -p "$STATE_DIR"
    cp "$BUILD_DIR/meson-logs/install-log.txt" "$STATE_DIR/install-files.txt"
fi

refresh_caches
hash -r 2>/dev/null || true

INSTALLED="$PREFIX/bin/loupe"
[ -x "$INSTALLED" ] || die "Cài xong nhưng không thấy binary: $INSTALLED"

FOUND="$(command -v loupe 2>/dev/null || true)"
log "Đã cài: $INSTALLED"
log "Binary trong PATH: ${FOUND:-không tìm thấy}"

if [ -n "$FOUND" ] && [ "$(realpath -m -- "$FOUND")" != "$(realpath -m -- "$INSTALLED")" ]; then
    warn "PATH đang trỏ tới $FOUND. Hãy đặt $PREFIX/bin trước /usr/bin trong PATH."
fi

if [ -f "$PREFIX/share/dbus-1/services/org.gnome.Loupe.service" ]; then
    log "D-Bus service: $PREFIX/share/dbus-1/services/org.gnome.Loupe.service"
fi

if [ "$RUN_AFTER" -eq 1 ]; then
    log "Chạy Loupe..."
    nohup "$INSTALLED" >/dev/null 2>&1 &
    disown 2>/dev/null || true
fi

log "Hoàn tất. Nếu Loupe vừa được cài lần đầu, hãy thoát hẳn instance cũ rồi mở lại."
