//! NVIDIA GPU limits through NVML (the library `nvidia-smi` itself uses), so nothing depends on
//! the `nvidia-smi` binary or a shell.
//!
//! Polling must never wake a sleeping GPU: in Hybrid mode the dGPU sits in runtime D3 and any NVML
//! call powers it up, which costs battery. `state()` therefore reads the PCI runtime status first
//! and only talks to NVML while the GPU is already awake.

use crate::config::GpuProfile;
use anyhow::{Context, Result, bail};
use nvml_wrapper::{Nvml, enum_wrappers::device::Clock, enums::device::GpuLockedClocksSetting};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Lowest graphics clock accepted for a lock. NVML rejects values the hardware cannot do.
pub const MIN_CLOCK_MHZ: u32 = 210;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GpuState {
    pub present: bool,
    /// False while the GPU is in runtime suspend; no other field is filled then.
    pub awake: bool,
    pub name: Option<String>,
    pub power_draw_w: Option<f32>,
    /// Limit in force, and the range we allow (never above the vendor default).
    pub power_limit_w: Option<u32>,
    pub power_min_w: Option<u32>,
    pub power_default_w: Option<u32>,
    pub clock_mhz: Option<u32>,
    pub clock_max_mhz: Option<u32>,
    pub error: Option<String>,
}

fn nvidia_pci() -> Option<PathBuf> {
    std::fs::read_dir("/sys/bus/pci/devices").ok()?.flatten().map(|e| e.path()).find(|p| {
        let rd = |f: &str| std::fs::read_to_string(p.join(f)).map(|s| s.trim().to_owned()).unwrap_or_default();
        rd("vendor") == "0x10de" && rd("class").starts_with("0x03")
    })
}

fn awake(pci: &std::path::Path) -> bool {
    std::fs::read_to_string(pci.join("power/runtime_status")).map(|s| s.trim() == "active").unwrap_or(true)
}

pub fn state() -> GpuState {
    let Some(pci) = nvidia_pci() else { return GpuState::default() };
    if !awake(&pci) {
        return GpuState { present: true, ..Default::default() };
    }
    let mut st = GpuState { present: true, awake: true, ..Default::default() };
    let nvml = match Nvml::init() {
        Ok(n) => n,
        Err(e) => {
            st.error = Some(format!("NVML unavailable: {e}"));
            return st;
        }
    };
    let Ok(dev) = nvml.device_by_index(0) else {
        st.error = Some("no NVIDIA device found through NVML".into());
        return st;
    };
    let w = |mw: u32| mw / 1000;
    st.name = dev.name().ok();
    st.power_draw_w = dev.power_usage().ok().map(|mw| mw as f32 / 1000.0);
    st.power_limit_w = dev.enforced_power_limit().ok().map(w);
    st.power_default_w = dev.power_management_limit_default().ok().map(w);
    st.power_min_w = dev.power_management_limit_constraints().ok().map(|c| w(c.min_limit).max(1));
    st.clock_mhz = dev.clock_info(Clock::Graphics).ok();
    st.clock_max_mhz = dev.max_clock_info(Clock::Graphics).ok();
    st
}

/// Pure range check for a power limit: between the driver minimum and the vendor default.
pub fn check_power(w: u32, min: u32, default: u32) -> Result<()> {
    if w < min || w > default {
        bail!("GPU power limit must be {min}-{default} W (the vendor default is the ceiling), got {w}");
    }
    Ok(())
}

pub fn check_clock(mhz: u32, max: u32) -> Result<()> {
    if mhz < MIN_CLOCK_MHZ || mhz > max {
        bail!("GPU clock cap must be {MIN_CLOCK_MHZ}-{max} MHz, got {mhz}");
    }
    Ok(())
}

pub fn set_power_limit(w: u32) -> Result<()> {
    let nvml = Nvml::init().context("NVML unavailable")?;
    let mut dev = nvml.device_by_index(0).context("no NVIDIA device")?;
    let default = dev.power_management_limit_default().context("read default power limit")? / 1000;
    let min = (dev.power_management_limit_constraints().context("read power limit range")?.min_limit / 1000).max(1);
    check_power(w, min, default)?;
    dev.set_power_management_limit(w * 1000)
        .map_err(|e| anyhow::anyhow!("the driver refused the power limit ({e}); many laptop GPUs lock this"))
}

/// Cap the graphics clock. A value at or above the GPU's maximum removes the cap.
pub fn set_clock_cap(mhz: u32) -> Result<()> {
    let nvml = Nvml::init().context("NVML unavailable")?;
    let mut dev = nvml.device_by_index(0).context("no NVIDIA device")?;
    let max = dev.max_clock_info(Clock::Graphics).context("read maximum clock")?;
    if mhz >= max {
        return dev.reset_gpu_locked_clocks().map_err(|e| anyhow::anyhow!("could not remove the clock cap ({e})"));
    }
    check_clock(mhz, max)?;
    dev.set_gpu_locked_clocks(GpuLockedClocksSetting::Numeric { min_clock_mhz: MIN_CLOCK_MHZ, max_clock_mhz: mhz })
        .map_err(|e| anyhow::anyhow!("the driver refused the clock cap ({e}); laptop GPUs often do not allow clock locking"))
}

/// Entry point for `SetAttr` names starting with `gpu_`.
pub fn set_attr(name: &str, value: &str) -> Result<()> {
    let n: u32 = value.parse().map_err(|_| anyhow::anyhow!("expected a number, got {value:?}"))?;
    match name {
        "gpu_power" => set_power_limit(n),
        "gpu_clock_max" => set_clock_cap(n),
        _ => bail!("unknown GPU attribute {name:?}"),
    }
}

/// Apply the GPU part of a profile. A sleeping GPU is skipped rather than woken.
pub fn apply(p: &GpuProfile) -> Vec<Result<()>> {
    let mut out = Vec::new();
    if p.power_w.is_none() && p.max_clock_mhz.is_none() {
        return out;
    }
    if !nvidia_pci().is_some_and(|d| awake(&d)) {
        return out;
    }
    if let Some(w) = p.power_w {
        out.push(set_power_limit(w));
    }
    if let Some(m) = p.max_clock_mhz {
        out.push(set_clock_cap(m));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_is_bounded_by_the_vendor_default() {
        // RTX 4080 Laptop on the tested machine: driver minimum 5 W, vendor default 60 W.
        assert!(check_power(45, 5, 60).is_ok());
        assert!(check_power(60, 5, 60).is_ok());
        assert!(check_power(61, 5, 60).is_err());
        assert!(check_power(4, 5, 60).is_err());
    }

    #[test]
    fn clock_cap_range() {
        assert!(check_clock(1800, 3105).is_ok());
        assert!(check_clock(100, 3105).is_err());
        assert!(check_clock(3200, 3105).is_err());
    }

    #[test]
    fn unknown_attribute_rejected() {
        assert!(set_attr("gpu_voltage", "1").is_err());
        assert!(set_attr("gpu_power", "lots").is_err());
    }
}
