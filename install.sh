#!/bin/sh
set -e

REPO="zamkara/alga"
PREFIX="/usr/local"

# ── Install runtime dependencies ──────────────────────────────────────────────
install_deps() {
  if command -v pacman >/dev/null 2>&1; then
    sudo pacman -Sy --noconfirm --needed \
      gtk4 libadwaita vte3 polkit \
      bootc ostree skopeo \
      btrfs-progs dosfstools efibootmgr util-linux

  elif command -v apt-get >/dev/null 2>&1; then
    sudo apt-get update -qq
    sudo apt-get install -y \
      libgtk-4-1 libadwaita-1-0 libvte-2.91-gtk4-0 policykit-1 \
      ostree skopeo \
      btrfs-progs dosfstools efibootmgr util-linux
    # bootc is not in Debian/Ubuntu repos — install via cargo if available
    if ! command -v bootc >/dev/null 2>&1; then
      if command -v cargo >/dev/null 2>&1; then
        echo "Installing bootc via cargo..."
        cargo install bootc 2>/dev/null || \
          echo "WARNING: Could not install bootc. Install it manually: https://github.com/containers/bootc"
      else
        echo "WARNING: bootc not found. Install it manually: https://github.com/containers/bootc"
      fi
    fi

  elif command -v dnf >/dev/null 2>&1; then
    sudo dnf install -y \
      gtk4 libadwaita vte291-gtk4 polkit \
      bootc ostree skopeo \
      btrfs-progs dosfstools efibootmgr util-linux

  elif command -v zypper >/dev/null 2>&1; then
    sudo zypper install -y \
      gtk4 libadwaita polkit \
      ostree skopeo \
      btrfs-progs dosfstools efibootmgr util-linux
    if ! command -v bootc >/dev/null 2>&1; then
      echo "WARNING: bootc not found. Install it manually: https://github.com/containers/bootc"
    fi

  else
    echo "WARNING: Could not detect package manager."
    echo "Install manually: gtk4, libadwaita, polkit, bootc, ostree, skopeo, btrfs-progs, dosfstools, efibootmgr"
  fi
}

echo "Installing dependencies..."
install_deps

# ── Download latest release ───────────────────────────────────────────────────
latest=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
  | grep '"tag_name"' | cut -d'"' -f4)

asset="alga-1.0.0-${latest#v}-x86_64.pkg.tar.zst"
url="https://github.com/$REPO/releases/download/$latest/$asset"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading alga $latest..."
curl -fsSL "$url" -o "$tmp/alga.pkg.tar.zst"
tar -I zstd -xf "$tmp/alga.pkg.tar.zst" -C "$tmp"

# ── Install files ─────────────────────────────────────────────────────────────
do_install() {
  install -Dm755 "$tmp/usr/bin/alga" "$PREFIX/bin/alga"

  install -Dm644 "$tmp/usr/share/icons/hicolor/scalable/apps/com.zamkara.alga.svg" \
    "$PREFIX/share/icons/hicolor/scalable/apps/com.zamkara.alga.svg"

  install -Dm644 /dev/stdin "$PREFIX/share/applications/com.zamkara.alga.desktop" << 'EOF'
[Desktop Entry]
Name=Ark Wizard
GenericName=System Installer
Comment=Install Ark Linux to your system
Exec=alga
Icon=com.zamkara.alga
Terminal=false
Type=Application
DBusActivatable=true
Categories=System;
StartupNotify=true
EOF

  install -Dm644 /dev/stdin "$PREFIX/share/dbus-1/services/com.zamkara.alga.service" << 'EOF'
[D-BUS Service]
Name=com.zamkara.alga
Exec=/usr/local/bin/alga --gapplication-service
EOF

  for f in ready-to-go.svg check-for-update.svg update-available.svg; do
    [ -f "$tmp/usr/share/alga/$f" ] && \
      install -Dm644 "$tmp/usr/share/alga/$f" "$PREFIX/share/alga/$f"
  done

  gtk-update-icon-cache -qtf "$PREFIX/share/icons/hicolor" 2>/dev/null || true
  update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
}

if [ -w "$PREFIX" ]; then
  do_install
else
  sudo sh -c "$(declare -f do_install); tmp='$tmp' PREFIX='$PREFIX' do_install"
fi

echo "Done. Run: alga"
