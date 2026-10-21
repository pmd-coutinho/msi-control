//! msi-ec sysfs interface (`/sys/devices/platform/msi-ec`).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_ROOT: &str = "/sys/devices/platform/msi-ec";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EcState {
    pub shift_mode: Option<String>,
    pub available_shift_modes: Vec<String>,
    pub fan_mode: Option<String>,
    pub available_fan_modes: Vec<String>,
    pub cooler_boost: Option<bool>,
    pub super_battery: Option<bool>,
    pub webcam: Option<bool>,
    pub webcam_block: Option<bool>,
    pub fn_key: Option<String>,
    pub win_key: Option<String>,
    pub charge_start: Option<u32>,
    pub charge_end: Option<u32>,
    pub fw_version: Option<String>,
    pub cpu_temp: Option<u32>,
    pub cpu_fan_pct: Option<u32>,
    pub gpu_temp: Option<u32>,
    pub gpu_fan_pct: Option<u32>,
}

fn battery_dir() -> Option<PathBuf> {
    let mut bats: Vec<PathBuf> = std::fs::read_dir("/sys/class/power_supply").ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("charge_control_end_threshold").exists())
        .collect();
    bats.sort();
    bats.into_iter().next()
}

fn battery_threshold(kind: &str) -> Option<u32> {
    std::fs::read_to_string(battery_dir()?.join(format!("charge_control_{kind}_threshold"))).ok()?.trim().parse().ok()
}

/// True when a mains supply is online, or when none can be found (desktop-like fallback).
pub fn ac_online() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") else { return true };
    let mut any = false;
    for e in dir.flatten() {
        let p = e.path();
        let rd = |f: &str| std::fs::read_to_string(p.join(f)).map(|s| s.trim().to_owned()).unwrap_or_default();
        if rd("type") == "Mains" {
            any = true;
            if rd("online") == "1" {
                return true;
            }
        }
    }
    !any
}

pub struct Ec {
    root: PathBuf,
}

impl Ec {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn system() -> Self {
        Self::new(DEFAULT_ROOT)
    }

    pub fn available(&self) -> bool {
        self.root.join("shift_mode").exists()
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(rel)).ok().map(|s| s.trim().to_owned())
    }

    fn read_list(&self, rel: &str) -> Vec<String> {
        self.read(rel)
            .map(|s| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn read_bool(&self, rel: &str) -> Option<bool> {
        self.read(rel).map(|s| s == "on")
    }

    fn read_num(&self, rel: &str) -> Option<u32> {
        self.read(rel)?.parse().ok()
    }

    fn write(&self, rel: &str, value: &str) -> Result<()> {
        let path: &Path = &self.root.join(rel);
        std::fs::write(path, value).with_context(|| format!("write {value:?} to {}", path.display()))
    }

    pub fn state(&self) -> EcState {
        EcState {
            shift_mode: self.read("shift_mode"),
            available_shift_modes: self.read_list("available_shift_modes"),
            fan_mode: self.read("fan_mode"),
            available_fan_modes: self.read_list("available_fan_modes"),
            cooler_boost: self.read_bool("cooler_boost"),
            super_battery: self.read_bool("super_battery"),
            webcam: self.read_bool("webcam"),
            webcam_block: self.read_bool("webcam_block"),
            fn_key: self.read("fn_key"),
            win_key: self.read("win_key"),
            charge_start: battery_threshold("start"),
            charge_end: battery_threshold("end"),
            fw_version: self.read("fw_version"),
            cpu_temp: self.read_num("cpu/realtime_temperature"),
            cpu_fan_pct: self.read_num("cpu/realtime_fan_speed"),
            gpu_temp: self.read_num("gpu/realtime_temperature"),
            gpu_fan_pct: self.read_num("gpu/realtime_fan_speed"),
        }
    }

    fn set_choice(&self, attr: &str, avail: &str, value: &str) -> Result<()> {
        if !self.read_list(avail).iter().any(|m| m == value) {
            bail!("{value:?} is not in {avail}");
        }
        self.write(attr, value)
    }

    pub fn set_shift_mode(&self, mode: &str) -> Result<()> {
        self.set_choice("shift_mode", "available_shift_modes", mode)
    }

    pub fn set_fan_mode(&self, mode: &str) -> Result<()> {
        self.set_choice("fan_mode", "available_fan_modes", mode)
    }

    pub fn set_cooler_boost(&self, on: bool) -> Result<()> {
        self.write("cooler_boost", if on { "on" } else { "off" })
    }

    pub fn set_super_battery(&self, on: bool) -> Result<()> {
        self.write("super_battery", if on { "on" } else { "off" })
    }

    /// Set any user-facing attribute by name, with validation. The single entry point used by the
    /// daemon, so the set of writable attributes is exactly this whitelist.
    pub fn set_attr(&self, name: &str, value: &str) -> Result<()> {
        let on_off = |v: &str| -> Result<bool> {
            match v { "on" => Ok(true), "off" => Ok(false), _ => bail!("expected on|off, got {v:?}") }
        };
        match name {
            "shift_mode" => self.set_shift_mode(value),
            "fan_mode" => self.set_fan_mode(value),
            "cooler_boost" => self.set_cooler_boost(on_off(value)?),
            "super_battery" => self.set_super_battery(on_off(value)?),
            "webcam" | "webcam_block" => { on_off(value)?; self.write(name, value) }
            "fn_key" | "win_key" => {
                if !matches!(value, "left" | "right") { bail!("expected left|right, got {value:?}"); }
                self.write(name, value)
            }
            // msi-ec derives the start threshold from the end threshold, so only `charge_end` is
            // settable; the driver validates the range.
            "charge_end" => {
                let v: u32 = value.parse().map_err(|_| anyhow::anyhow!("expected 0-100, got {value:?}"))?;
                let dir = battery_dir().context("no battery with charge thresholds")?;
                let path = dir.join("charge_control_end_threshold");
                std::fs::write(&path, v.to_string()).with_context(|| format!("write {}", path.display()))
            }
            _ => bail!("unknown attribute {name:?}"),
        }
    }
}
