#!/bin/sh
# One-shot install for non-Nix systems (CachyOS/Arch etc.). Works two ways:
#   - from a release tarball (has bin/): installs the prebuilt binaries, no Rust needed
#   - from a source checkout: builds release binaries first
# Installs the D-Bus policy, polkit action, systemd unit and tray autostart, then enables the daemon.
# On NixOS use the flake's module instead.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
PREFIX=${PREFIX:-/usr/local}
if [ -x "$here/bin/msi-controld" ]; then
    ROOT=$here
    BIN=$here/bin
else
    ROOT=$here/..
    cd "$ROOT"
    cargo build --release --workspace --locked
    BIN=$ROOT/target/release
fi
sudo install -Dm755 "$BIN/msi-controld" "$BIN/msi-control" "$BIN/msi-control-gui" "$BIN/msi-control-tray" -t "$PREFIX/bin"
sudo install -Dm644 "$ROOT/data/org.msicontrol.Daemon1.conf" /usr/share/dbus-1/system.d/org.msicontrol.Daemon1.conf
sudo install -Dm644 "$ROOT/data/org.msicontrol.policy" /usr/share/polkit-1/actions/org.msicontrol.policy
sudo install -Dm644 "$ROOT/data/org.msicontrol.Gui.desktop" /usr/share/applications/org.msicontrol.Gui.desktop
sudo install -Dm644 "$ROOT/data/org.msicontrol.Tray.desktop" /etc/xdg/autostart/org.msicontrol.Tray.desktop
sed "s|@bindir@|$PREFIX/bin|" "$ROOT/data/msi-controld.service" | sudo install -Dm644 /dev/stdin /usr/lib/systemd/system/msi-controld.service
printf 'msi_ec\nmsi_wmi_platform\n' | sudo tee /etc/modules-load.d/msi-control.conf >/dev/null
sudo modprobe msi_ec msi_wmi_platform 2>/dev/null || true
sudo systemctl reload dbus-broker 2>/dev/null || sudo systemctl reload dbus 2>/dev/null || true
sudo systemctl daemon-reload
sudo systemctl enable --now msi-controld.service
echo "Installed. Try: msi-control status"
