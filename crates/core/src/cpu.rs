//! CPU limits through standard Linux interfaces: intel_pstate and RAPL (powercap).
//! These are reversible (a reboot restores firmware defaults) and need no EC access.

use crate::config::CpuProfile;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Power limit bounds in watts.
pub const MIN_PL_W: u32 = 15;
/// Sustained limit (PL1) cap: what MSI's firmware ships on the tested machine (150 W).
pub const MAX_PL1_W: u32 = 150;
/// Burst limit (PL2) cap: MSI's shipped 220 W. Limits are never raised above what the vendor set.
pub const MAX_PL2_W: u32 = 220;
pub const MIN_PERF_PCT: u32 = 20;

const PSTATE: &str = "/sys/devices/system/cpu/intel_pstate";
const RAPL_ROOT: &str = "/sys/class/powercap";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CpuState {
    /// True when turbo is disabled.
    pub no_turbo: Option<bool>,
    pub max_perf_pct: Option<u32>,
    pub pl1_w: Option<u32>,
    pub pl2_w: Option<u32>,
    pub pl_min_w: u32,
    pub pl1_max_w: u32,
    pub pl2_max_w: u32,
}

fn read_num(p: &Path) -> Option<u64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// Every package-level RAPL domain. The CPU exposes the same limits through the MSR interface
/// (`intel-rapl:0`) and through MMIO (`intel-rapl-mmio:0`); firmware may set them differently and
/// the hardware obeys the lower value, so both are read and written.
fn rapl_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(RAPL_ROOT).into_iter().flatten().flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("intel-rapl") && n.matches(':').count() == 1)
                && std::fs::read_to_string(p.join("name")).is_ok_and(|n| n.trim().starts_with("package"))
        })
        .collect();
    dirs.sort();
    dirs
}

fn limit_w(dir: &Path, constraint: u8) -> Option<u32> {
    read_num(&dir.join(format!("constraint_{constraint}_power_limit_uw"))).map(|uw| (uw / 1_000_000) as u32)
}

/// The limit actually in force: the lowest across domains.
fn effective_w(dirs: &[PathBuf], constraint: u8) -> Option<u32> {
    dirs.iter().filter_map(|d| limit_w(d, constraint)).min()
}

pub fn state() -> CpuState {
    let rapl = rapl_dirs();
    CpuState {
        no_turbo: read_num(&Path::new(PSTATE).join("no_turbo")).map(|v| v != 0),
        max_perf_pct: read_num(&Path::new(PSTATE).join("max_perf_pct")).map(|v| v as u32),
        pl1_w: effective_w(&rapl, 0),
        pl2_w: effective_w(&rapl, 1),
        pl_min_w: MIN_PL_W,
        pl1_max_w: MAX_PL1_W,
        pl2_max_w: MAX_PL2_W,
    }
}

fn write(p: &Path, v: impl ToString) -> Result<()> {
    std::fs::write(p, v.to_string()).with_context(|| format!("write {}", p.display()))
}

fn parse_w(value: &str, max: u32) -> Result<u32> {
    let w: u32 = value.parse().map_err(|_| anyhow::anyhow!("expected watts, got {value:?}"))?;
    if !(MIN_PL_W..=max).contains(&w) {
        bail!("power limit must be {MIN_PL_W}-{max} W, got {w}");
    }
    Ok(w)
}

pub fn parse_pl1(value: &str) -> Result<u32> {
    parse_w(value, MAX_PL1_W)
}

pub fn parse_pl2(value: &str) -> Result<u32> {
    parse_w(value, MAX_PL2_W)
}

pub fn parse_pct(value: &str) -> Result<u32> {
    let p: u32 = value.parse().map_err(|_| anyhow::anyhow!("expected a percentage, got {value:?}"))?;
    if !(MIN_PERF_PCT..=100).contains(&p) {
        bail!("max performance must be {MIN_PERF_PCT}-100 %, got {p}");
    }
    Ok(p)
}

/// Write one constraint on every domain, skipping domains already at the target. Succeeds if at
/// least one domain accepted the value (one may be locked by firmware); reports the rest on stderr.
fn set_pl(dirs: &[PathBuf], constraint: u8, w: u32) -> Result<()> {
    let target = u64::from(w) * 1_000_000;
    let (mut ok, mut last_err) = (false, None);
    for d in dirs {
        let path = d.join(format!("constraint_{constraint}_power_limit_uw"));
        if read_num(&path) == Some(target) {
            ok = true;
            continue;
        }
        match write(&path, target) {
            Ok(()) => ok = true,
            Err(e) => {
                eprintln!("msi-controld: {e:#}");
                last_err = Some(e);
            }
        }
    }
    match (ok, last_err) {
        (false, Some(e)) => Err(e),
        _ => Ok(()),
    }
}

/// Decide the (PL1, PL2) pair to write. PL2 never sits below PL1, so raising PL1 may raise PL2.
/// Pure so it can be tested; `set_power_limits` does the I/O.
fn plan(cur: (u32, u32), pl1: Option<u32>, pl2: Option<u32>) -> Result<(u32, u32)> {
    let new1 = pl1.unwrap_or(cur.0);
    let new2 = pl2.unwrap_or(cur.1).max(new1);
    if new1 > MAX_PL1_W {
        bail!("PL1 would exceed {MAX_PL1_W} W");
    }
    if new2 > MAX_PL2_W {
        bail!("PL2 would exceed {MAX_PL2_W} W");
    }
    Ok((new1, new2))
}

/// Set PL1 and/or PL2.
pub fn set_power_limits(pl1: Option<u32>, pl2: Option<u32>) -> Result<()> {
    let dirs = rapl_dirs();
    if dirs.is_empty() {
        bail!("RAPL power limits are not available (intel-rapl)");
    }
    let cur = (effective_w(&dirs, 0).unwrap_or(MIN_PL_W), effective_w(&dirs, 1).unwrap_or(MIN_PL_W));
    let (new1, new2) = plan(cur, pl1, pl2)?;
    // Write the burst limit first when raising, last when lowering, so PL2 >= PL1 holds throughout.
    if new2 >= cur.1 {
        set_pl(&dirs, 1, new2)?;
        set_pl(&dirs, 0, new1)
    } else {
        set_pl(&dirs, 0, new1)?;
        set_pl(&dirs, 1, new2)
    }
}

pub fn set_turbo(on: bool) -> Result<()> {
    write(&Path::new(PSTATE).join("no_turbo"), if on { 0 } else { 1 })
}

pub fn set_max_perf(pct: u32) -> Result<()> {
    write(&Path::new(PSTATE).join("max_perf_pct"), pct)
}

/// Entry point for `SetAttr` names starting with `cpu_`.
pub fn set_attr(name: &str, value: &str) -> Result<()> {
    match name {
        "cpu_turbo" => match value {
            "on" => set_turbo(true),
            "off" => set_turbo(false),
            _ => bail!("expected on|off, got {value:?}"),
        },
        "cpu_max_perf" => set_max_perf(parse_pct(value)?),
        "cpu_pl1" => set_power_limits(Some(parse_pl1(value)?), None),
        "cpu_pl2" => set_power_limits(None, Some(parse_pl2(value)?)),
        _ => bail!("unknown CPU attribute {name:?}"),
    }
}

/// Apply the CPU part of a profile. Unset fields are left alone.
pub fn apply(p: &CpuProfile) -> Vec<Result<()>> {
    let mut out = Vec::new();
    if let Some(t) = p.turbo {
        out.push(set_turbo(t));
    }
    if let Some(pct) = p.max_perf_pct {
        out.push(if (MIN_PERF_PCT..=100).contains(&pct) { set_max_perf(pct) } else { Err(anyhow::anyhow!("max_perf_pct {pct} out of range")) });
    }
    if p.pl1_w.is_some() || p.pl2_w.is_some() {
        let ok = |w: Option<u32>, max: u32| w.is_none_or(|w| (MIN_PL_W..=max).contains(&w));
        out.push(if ok(p.pl1_w, MAX_PL1_W) && ok(p.pl2_w, MAX_PL2_W) { set_power_limits(p.pl1_w, p.pl2_w) } else { Err(anyhow::anyhow!("profile power limit out of range")) });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watts_bounds() {
        assert_eq!(parse_pl1("45").unwrap(), 45);
        assert!(parse_pl1("14").is_err());
        assert!(parse_pl1("151").is_err());
        assert!(parse_pl1("fast").is_err());
        assert_eq!(parse_pl2("220").unwrap(), 220);
        assert!(parse_pl2("221").is_err());
    }

    /// Regression: this laptop ships PL1 150 W / PL2 220 W. Changing PL1 must keep the shipped PL2.
    #[test]
    fn lowering_pl1_keeps_the_shipped_burst_limit() {
        assert_eq!(plan((150, 220), Some(90), None).unwrap(), (90, 220));
    }

    /// The two RAPL views disagree on this machine (150 W vs 110 W); the lower one is in force.
    #[test]
    fn effective_limit_is_the_lowest_across_domains() {
        let dir = std::env::temp_dir().join(format!("msi-rapl-{}", std::process::id()));
        let mk = |n: &str, uw: u64| {
            let d = dir.join(n);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("constraint_0_power_limit_uw"), uw.to_string()).unwrap();
            d
        };
        let dirs = vec![mk("a", 150_000_000), mk("b", 110_000_000)];
        assert_eq!(effective_w(&dirs, 0), Some(110));
        set_pl(&dirs, 0, 90).unwrap();
        assert_eq!(effective_w(&dirs, 0), Some(90));
        assert_eq!(limit_w(&dirs[0], 0), Some(90));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn burst_never_below_sustained() {
        assert_eq!(plan((60, 80), Some(100), None).unwrap(), (100, 100));
        assert_eq!(plan((90, 220), None, Some(50)).unwrap(), (90, 90));
    }

    #[test]
    fn plan_rejects_over_cap() {
        assert!(plan((150, 220), Some(151), None).is_err());
        assert!(plan((150, 220), None, Some(221)).is_err());
    }

    #[test]
    fn percent_bounds() {
        assert!(parse_pct("19").is_err());
        assert_eq!(parse_pct("100").unwrap(), 100);
        assert!(parse_pct("101").is_err());
    }

    #[test]
    fn unknown_attribute_rejected() {
        assert!(set_attr("cpu_voltage", "-50").is_err());
        assert!(set_attr("cpu_turbo", "maybe").is_err());
    }
}
