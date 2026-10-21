{ lib, rustPlatform, pkg-config, wrapGAppsHook4, gtk4, libadwaita, glib }:
rustPlatform.buildRustPackage {
  pname = "msi-control";
  version = "0.1.0";
  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;
  nativeBuildInputs = [ pkg-config wrapGAppsHook4 ];
  buildInputs = [ gtk4 libadwaita glib ];
  postInstall = ''
    install -Dm644 data/org.msicontrol.Daemon1.conf $out/share/dbus-1/system.d/org.msicontrol.Daemon1.conf
    install -Dm644 data/org.msicontrol.policy $out/share/polkit-1/actions/org.msicontrol.policy
    install -Dm644 data/org.msicontrol.Gui.desktop $out/share/applications/org.msicontrol.Gui.desktop
    mkdir -p $out/lib/systemd/system
    substitute data/msi-controld.service $out/lib/systemd/system/msi-controld.service --replace @bindir@ $out/bin
  '';
  meta = { description = "MSI laptop EC, GPU MUX and power-profile control"; license = lib.licenses.gpl3Plus; mainProgram = "msi-control"; };
}
