//! Profile mapping between power-profiles-daemon names and MSI EC settings.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const CONFIG_PATH: &str = "/etc/msi-control/config.toml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EcProfile {
    pub shift_mode: String,
    pub fan_mode: String,
    #[serde(default)]
    pub cooler_boost: bool,
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
        let p = |s: &str, f: &str| EcProfile { shift_mode: s.into(), fan_mode: f.into(), cooler_boost: false };
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
