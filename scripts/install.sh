#!/bin/sh
# One-shot install for non-Nix systems (CachyOS/Arch etc.): builds release binaries, installs the
# D-Bus policy, polkit action and systemd unit, then enables the daemon. On NixOS use the module instead.
set -eu
cd "$(dirname "$0")/.."
PREFIX=${PREFIX:-/usr/local}
cargo build --release --workspace --locked
sudo install -Dm755 target/release/msi-controld target/release/msi-control target/release/msi-control-gui target/release/msi-control-tray -t "$PREFIX/bin"
sudo install -Dm644 data/org.msicontrol.Daemon1.conf /usr/share/dbus-1/system.d/org.msicontrol.Daemon1.conf
sudo install -Dm644 data/org.msicontrol.policy /usr/share/polkit-1/actions/org.msicontrol.policy
sudo install -Dm644 data/org.msicontrol.Gui.desktop /usr/share/applications/org.msicontrol.Gui.desktop
sudo install -Dm644 data/org.msicontrol.Tray.desktop /etc/xdg/autostart/org.msicontrol.Tray.desktop
sed "s|@bindir@|$PREFIX/bin|" data/msi-controld.service | sudo install -Dm644 /dev/stdin /usr/lib/systemd/system/msi-controld.service
printf 'msi_ec\nmsi_wmi_platform\n' | sudo tee /etc/modules-load.d/msi-control.conf >/dev/null
sudo modprobe msi_ec msi_wmi_platform 2>/dev/null || true
sudo systemctl reload dbus-broker 2>/dev/null || sudo systemctl reload dbus 2>/dev/null || true
sudo systemctl daemon-reload
sudo systemctl enable --now msi-controld.service
echo "Installed. Try: msi-control status"
