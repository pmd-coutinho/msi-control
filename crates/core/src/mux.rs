//! Read-only decoding of the MSI GPU MUX firmware state (`MsiDCVarData` EFI variable).
//!
//! Layout and bit meanings follow the characterization in hayatboj/msi-gpu-mux-switch.
//! Switching lives in `mux_apply`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const EFIVAR: &str =
    "/sys/firmware/efi/efivars/MsiDCVarData-dd96baaf-145e-4f56-b1cf-193256298e99";
const PAYLOAD_LEN: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MuxMode {
    Hybrid,
    Discrete,
    Integrated,
}

impl MuxMode {
    pub fn bits(self) -> u8 {
        match self { Self::Hybrid => 0, Self::Discrete => 1, Self::Integrated => 2 }
    }

    fn from_bits(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Hybrid),
            1 => Some(Self::Discrete),
            2 => Some(Self::Integrated),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuxState {
    pub current: Option<MuxMode>,
    /// Mode firmware will use after the next full power cycle.
    pub pending: Option<MuxMode>,
    pub available: Vec<MuxMode>,
    pub reboot_required: bool,
}

pub fn decode(raw: &[u8]) -> Result<MuxState> {
    // efivarfs prefixes the payload with 4 attribute bytes.
    let payload = raw.get(4..).context("efivar shorter than attribute header")?;
    if payload.len() != PAYLOAD_LEN {
        bail!("unexpected MsiDCVarData length {}", payload.len());
    }
    let b = payload[5];
    let pending = MuxMode::from_bits(b & 0x03);
    let current = MuxMode::from_bits((b & 0x0c) >> 2);
    let mut available = Vec::new();
    if b & 0x10 != 0 {
        available.push(MuxMode::Hybrid);
        if b & 0x40 == 0 {
            available.push(MuxMode::Discrete);
        }
        if b & 0x20 != 0 {
            available.push(MuxMode::Integrated);
        }
    }
    Ok(MuxState { current, pending, available, reboot_required: current != pending })
}

pub fn read() -> Result<MuxState> {
    read_from(EFIVAR)
}

pub fn read_from(path: impl AsRef<Path>) -> Result<MuxState> {
    let raw = std::fs::read(path.as_ref()).with_context(|| format!("read {}", path.as_ref().display()))?;
    decode(&raw)
}

pub fn mode_arg(m: &str) -> Option<MuxMode> {
    match m {
        "hybrid" | "mshybrid" => Some(MuxMode::Hybrid),
        "discrete" => Some(MuxMode::Discrete),
        "integrated" => Some(MuxMode::Integrated),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(b5: u8) -> Vec<u8> {
        let mut v = vec![7, 0, 0, 0];
        v.extend([0u8; PAYLOAD_LEN]);
        v[4 + 5] = b5;
        v
    }

    #[test]
    fn decodes_discrete_baseline() {
        let s = decode(&raw(0x35)).unwrap();
        assert_eq!(s.current, Some(MuxMode::Discrete));
        assert_eq!(s.pending, Some(MuxMode::Discrete));
        assert!(!s.reboot_required);
        assert_eq!(s.available.len(), 3);
    }

    #[test]
    fn pending_differs_from_current() {
        let s = decode(&raw(0x32)).unwrap();
        assert_eq!(s.pending, Some(MuxMode::Integrated));
        assert_eq!(s.current, Some(MuxMode::Hybrid));
        assert!(s.reboot_required);
    }

    #[test]
    fn rejects_bad_length() {
        assert!(decode(&[7, 0, 0, 0, 1]).is_err());
    }
}
