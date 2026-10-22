//! Profile mapping between power-profiles-daemon names and MSI EC settings.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const CONFIG_PATH: &str = "/etc/msi-control/config.toml";

/// CPU limits applied with a profile. Unset fields are left as they are, so by default a profile
/// changes nothing on the CPU side.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CpuProfile {
    /// Sustained power limit (PL1) in watts.
    pub pl1_w: Option<u32>,
    /// Burst power limit (PL2) in watts.
    pub pl2_w: Option<u32>,
    pub turbo: Option<bool>,
    pub max_perf_pct: Option<u32>,
}

/// NVIDIA GPU limits applied with a profile; unset fields are left alone. Skipped while the GPU is
/// asleep, so a profile change never wakes it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GpuProfile {
    pub power_w: Option<u32>,
    pub max_clock_mhz: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EcProfile {
    pub shift_mode: String,
    pub fan_mode: String,
    #[serde(default)]
    pub cooler_boost: bool,
    #[serde(default)]
    pub cpu: CpuProfile,
    #[serde(default)]
    pub gpu: GpuProfile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Keys are PPD profile names: power-saver, balanced, performance.
    pub profiles: BTreeMap<String, EcProfile>,
    /// Overrides for `profiles` while on battery (AC offline), keyed by PPD profile name.
    /// Mirrors tuned's battery detection so the two never disagree on unplugged laptops.
    #[serde(default)]
    pub battery_profiles: BTreeMap<String, EcProfile>,
    /// Apply the mapped EC settings whenever the PPD profile changes.
    #[serde(default = "yes")]
    pub sync: bool,
    /// Also switch the PPD profile when the EC shift mode changes on its own (e.g. a Fn hotkey).
    /// Off by default: tuned profiles that write shift_mode via the sysfs plugin would make the
    /// two directions fight. Enable only if nothing else writes /sys/devices/platform/msi-ec.
    #[serde(default)]
    pub ec_to_ppd: bool,
    /// DMI board names on which native MUX switching is permitted. Add yours only after
    /// confirming (e.g. with `msi-control mux <mode> --dry-run`) that the layout matches.
    #[serde(default = "default_boards")]
    pub mux_boards: Vec<String>,
    /// DMI BIOS versions the firmware layout was characterized on. Re-add after a BIOS update
    /// once `mux --dry-run` looks right.
    #[serde(default = "default_bios")]
    pub mux_bios: Vec<String>,
}

fn default_bios() -> Vec<String> {
    vec!["E15M1IMS.915".into(), "E15M3IMS.116".into()]
}

fn default_boards() -> Vec<String> {
    vec!["MS-15M1".into(), "MS-15M3".into()]
}

fn yes() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        let p = |s: &str, f: &str| EcProfile { shift_mode: s.into(), fan_mode: f.into(), cooler_boost: false, cpu: CpuProfile::default(), gpu: GpuProfile::default() };
        Self {
            profiles: BTreeMap::from([
                ("power-saver".into(), p("eco", "silent")),
                ("balanced".into(), p("comfort", "auto")),
                ("performance".into(), p("turbo", "advanced")),
            ]),
            sync: true,
            battery_profiles: BTreeMap::new(),
            ec_to_ppd: false,
            mux_boards: default_boards(),
            mux_bios: default_bios(),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str(&s).map_err(|e| eprintln!("config error in {path}: {e}")).ok())
            .unwrap_or_default()
    }

    /// PPD profile whose EC shift mode matches, only if exactly one profile does.
    pub fn profile_for_shift(&self, shift: &str) -> Option<&str> {
        let mut it = self.profiles.iter().filter(|(_, p)| p.shift_mode == shift);
        match (it.next(), it.next()) {
            (Some((k, _)), None) => Some(k.as_str()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_limits_are_optional_in_toml() {
        let c: Config = toml::from_str(
            "[profiles.balanced]\nshift_mode = \"comfort\"\nfan_mode = \"auto\"\n\
             [profiles.power-saver]\nshift_mode = \"eco\"\nfan_mode = \"auto\"\n\
             [profiles.power-saver.cpu]\npl1_w = 45\nturbo = false\n",
        )
        .unwrap();
        assert_eq!(c.profiles["balanced"].cpu, CpuProfile::default());
        assert_eq!(c.profiles["power-saver"].cpu.pl1_w, Some(45));
        assert_eq!(c.profiles["power-saver"].cpu.turbo, Some(false));
    }

    #[test]
    fn default_config_does_not_touch_the_cpu() {
        assert!(Config::default().profiles.values().all(|p| p.cpu == CpuProfile::default() && p.gpu == GpuProfile::default()));
    }
}
