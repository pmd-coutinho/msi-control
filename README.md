# msi-control

> [!WARNING]
> **Use at your own risk. This writes to laptop firmware.**
> GPU MUX switching changes a UEFI variable and calls MSI WMI methods. A bad write can leave a machine
> with a black screen or in a state that needs a BIOS reset, and a firmware write can't be undone from
> software. The switching code follows the sequence documented by
> [hayatboj/msi-gpu-mux-switch](https://github.com/hayatboj/msi-gpu-mux-switch) and is unit-tested only
> against a mock. It has been run for read-only checks (status and `--dry-run`) on exactly one machine,
> an **MSI Vector 16 HX A14VHG (MS-15M1), BIOS E15M1IMS.915**. It has **not** completed a real switch yet.
> The default allowlist (`mux_boards`, `mux_bios`) also permits MS-15M3 / E15M3IMS.116, the machine the
> upstream project characterized; that board has never been tested with this code. Every other board and
> BIOS is refused. Do not add yours without reading `crates/core/src/mux_apply.rs`. The EC controls (shift and fan mode, charge limit and the
> rest) go through the `msi-ec` kernel driver and are far lower risk. Keep disk-encryption recovery keys
> available before any MUX change. No warranty; see LICENSE.


One app for MSI laptops: **msi-ec** (shift/fan/cooler boost/super battery), **GPU MUX** status and switching
(native EFI+WMI), and **power-profiles-daemon / tuned-ppd** sync.

- `msi-controld` – root daemon, system D-Bus `org.msicontrol.Daemon1`, polkit-gated writes
- `msi-control` – CLI (`status`, `profile`, `shift`, `fan`, `cooler-boost`, `super-battery`, `mux`)
- `msi-control-gui` – GTK4/libadwaita front-end (works on Hyprland and KDE)
- `msi-control-tray` – status-notifier tray icon: shows the profile and switches it (NixOS runs it as a user service; other distros get an XDG autostart entry)

Profile integration uses only the `net.hadess.PowerProfiles` D-Bus API, so it works with
power-profiles-daemon (CachyOS) and `tuned-ppd` (tuned on NixOS) alike.

## Config (`/etc/msi-control/config.toml`, optional)

```toml
sync = true          # PPD profile -> EC settings
ec_to_ppd = false    # EC shift change (Fn hotkey) -> PPD profile; see warning
# mux_boards = ["MS-15M1", "MS-15M3"]
# mux_bios = ["E15M1IMS.915", "E15M3IMS.116"]  # update after a BIOS update
[profiles.power-saver]
shift_mode = "eco"
fan_mode = "silent"

[profiles.balanced]
shift_mode = "comfort"
fan_mode = "auto"

[profiles.performance]
shift_mode = "turbo"
fan_mode = "advanced"
```

### CPU and GPU limits (optional, per profile)
Profiles can also set CPU and GPU limits. Anything you leave out is left alone, so by default a profile
changes nothing here. Limits reset on reboot; the daemon re-applies them when the profile changes.

```toml
[profiles.power-saver.cpu]
pl1_w = 45            # sustained power, 15-150 W
pl2_w = 65            # burst power, 15-220 W
turbo = false
max_perf_pct = 80     # share of the top frequency, 20-100

[profiles.power-saver.gpu]
power_w = 35          # NVIDIA power limit, never above the maker's default
```

- CPU limits use `intel_pstate` and RAPL. The CPU exposes RAPL twice (MSR and MMIO) and obeys the lower
  value, so both are written. `thermald --adaptive` may change them again on its own; check the value after
  about 30 seconds.
- GPU limits use NVML directly (no `nvidia-smi`). A sleeping GPU is never queried or woken, so profile
  changes in Hybrid mode skip the GPU limits. Many laptop GPUs refuse power-limit or clock-lock changes;
  you get the driver's error message. On NixOS the daemon needs `LD_LIBRARY_PATH=/run/opengl-driver/lib`,
  which the module sets.
- From the CLI: `msi-control set cpu_pl1 90`, `cpu_turbo off`, `gpu_power 40`, `gpu_clock_max 1800`.

**Single writer rule:** tuned profiles that write `shift_mode` through the `[sysfs]` plugin
(e.g. custom `msi-*` profiles) race with this daemon. Remove those lines and let the daemon own the EC,
or keep `ec_to_ppd = false`. The daemon re-asserts its settings 3 s after each profile change.

## GPU MUX (native, no external MUX tool)
msi-ec has no MUX interface, so the MUX is driven directly: the `MsiDCVarData` EFI variable plus the
`msi-wmi-platform` debugfs WMI methods (`get_ap`, `set_data`). The transaction sequence is reproduced from the
characterization in hayatboj/msi-gpu-mux-switch and unit-tested against a mock (`crates/core/src/mux_apply.rs`):
preflight -> stage target bits -> verify -> trigger (0xD1) -> wait apply-ready -> acknowledge (0xBE); any failure
after staging restores the previous target.

- `msi-control mux <hybrid|discrete|integrated> --dry-run` runs preflight only (writes nothing). **Run this first.**
- A trigger sent this boot (even if it failed) blocks further transitions until a full power-off (marker in `/run/msi-control`).
- Needs a polkit agent for the admin prompt: built in on KDE; `hyprpolkitagent` or `polkit-gnome` on Hyprland; `pkttyagent` for the CLI in a bare TTY. Without one, MUX calls fail with "not authorized".
- Requires AC power, `msi_wmi_platform` loaded, debugfs mounted, root (daemon), and polkit `auth_admin` every time.
- Takes effect only after a **full power-off**, not a reboot. Keep disk-encryption recovery keys available.
- Allowed only on boards listed in `mux_boards` (default `MS-15M1`, `MS-15M3`). Your MS-15M1 works with the
  upstream tool, but this native path has **never been run against real hardware**: do a dry-run, then a first
  real switch with AC connected and work saved.
- Native status/dry-run decode is verified on an MS-15M1; the real trigger/ack WMI calls are not.

## Install
- Nix: `nixosModules.default` from this flake, `services.msi-control.enable = true;`, plus either
  `services.power-profiles-daemon.enable` or `services.tuned.ppdSupport`.
- Arch/CachyOS, prebuilt: download `msi-control-<version>-x86_64-linux.tar.gz` from the
  [releases](https://github.com/pmd-coutinho/msi-control/releases), extract it and run `./install.sh`.
  Built on Arch against current GTK4 and libadwaita (1.7+), so it targets Arch, CachyOS and similar.
- Arch/CachyOS, from source: `./scripts/install.sh` or `cd packaging/arch && makepkg -si`.
- Either way you need `msi_ec` in the kernel or `msi-ec-dkms`. The Arch paths are untested.

## First run (nothing here has been exercised as root yet)
1. Install the D-Bus conf and polkit policy, start the daemon.
2. `msi-control status`
3. `msi-control set charge_end 80`, then re-run `status` and read it back.
4. `msi-control mux hybrid --dry-run`
5. Only then one real switch: AC connected, work saved, **full power-off** afterwards.

### Smoke test commands
```
sudo cp data/org.msicontrol.Daemon1.conf /etc/dbus-1/system.d/ && sudo cp data/org.msicontrol.policy /usr/share/polkit-1/actions/
sudo ./target/debug/msi-controld &
msi-control status
msi-control profile performance; sleep 10; cat /sys/devices/platform/msi-ec/shift_mode   # must stay "turbo"
```

## Not done yet
Keyboard backlight, custom fan curves (need raw EC register writes, which msi-ec does not expose), tray/autostart, in-GUI MUX apply button.
