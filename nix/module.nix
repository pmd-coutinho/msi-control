self: { config, lib, pkgs, ... }:
let cfg = config.services.msi-control; pkg = self.packages.${pkgs.stdenv.hostPlatform.system}.default; in {
  options.services.msi-control.enable = lib.mkEnableOption "msi-control daemon";
  config = lib.mkIf cfg.enable {
    boot.kernelModules = [ "msi-ec" "msi_wmi_platform" ];
    environment.systemPackages = [ pkg ];
    services.dbus.packages = [ pkg ];
    services.udev.packages = [ ];
    security.polkit.enable = true;
    environment.pathsToLink = [ "/share/polkit-1" ];
    systemd.packages = [ pkg ];
    systemd.services.msi-controld.wantedBy = [ "multi-user.target" ];
    # Either backend provides net.hadess.PowerProfiles: enable one of
    #   services.power-profiles-daemon.enable = true;   or   services.tuned.ppdSupport = true;
  };
}
