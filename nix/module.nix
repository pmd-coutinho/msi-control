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
    # Tray icon as a user service: Hyprland sessions do not process XDG autostart files.
    systemd.user.services.msi-control-tray = {
      description = "MSI Control tray icon";
      wantedBy = [ "graphical-session.target" ];
      partOf = [ "graphical-session.target" ];
      after = [ "graphical-session.target" ];
      path = [ pkg ]; # the tray starts msi-control-gui from PATH
      serviceConfig = {
        ExecStart = "${pkg}/bin/msi-control-tray";
        Restart = "on-failure";
      };
    };
    # Either backend provides net.hadess.PowerProfiles: enable one of
    #   services.power-profiles-daemon.enable = true;   or   services.tuned.ppdSupport = true;
  };
}
