//! Native GPU MUX switching: the firmware transaction, written against a hardware trait so the
//! sequence can be tested with a mock. The real implementation is `LinuxHw`.
//!
//! Sequence (characterized by hayatboj/msi-gpu-mux-switch, reproduced here):
//!   1. preflight: board allowlisted, AC online, variable is 20 bytes / attrs 0x7, mode advertised,
//!      not already current, no unresolved earlier transition, WMI apply-ready clear
//!   2. stage: set the target bits (byte 5, low 2 bits) in `MsiDCVarData`, verify read-back
//!   3. trigger: WMI set_data(0xD1, (ap_byte & 0xFC) | 1), wait up to 5 s for apply-ready
//!   4. acknowledge: WMI set_data(0xBE, 2); the new mode takes effect after a FULL power-off
//! Any failure after the staging write restores the previous target bits.

use crate::mux::{self, MuxMode};
use anyhow::{Context, Result, bail};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

const ATTRS: u32 = 0x07;
const LEN: usize = 20;
const TRIGGER_ADDR: u8 = 0xd1;
const ACK_ADDR: u8 = 0xbe;
const TRIGGER_MARKER: &str = "/run/msi-control/mux-trigger-attempted";

#[derive(Debug, Clone, Copy)]
pub struct Ap {
    pub data_byte: u8,
    pub apply_ready: bool,
}

pub trait Hw {
    fn read_var(&self) -> Result<(u32, Vec<u8>)>;
    fn write_var(&self, attrs: u32, payload: &[u8]) -> Result<()>;
    fn get_ap(&self) -> Result<Ap>;
    fn set_data(&self, addr: u8, val: u8) -> Result<()>;
    fn ac_online(&self) -> Result<bool>;
    fn board(&self) -> Result<String>;
    fn bios(&self) -> Result<String>;
    fn sleep_ms(&self, ms: u64);
    /// Marker that survives daemon restarts but not a reboot (tmpfs): set before any trigger is sent.
    fn trigger_marked(&self) -> bool;
    fn mark_trigger(&self) -> Result<()>;
}

fn stage(payload: &[u8], mode: MuxMode) -> Vec<u8> {
    let mut v = payload.to_vec();
    v[5] = (v[5] & 0xfc) | mode.bits();
    v
}

fn read_checked<H: Hw>(hw: &H) -> Result<(u32, Vec<u8>)> {
    let (attrs, payload) = hw.read_var()?;
    if payload.len() != LEN {
        bail!("refusing: MsiDCVarData is {} bytes, expected {LEN}", payload.len());
    }
    if attrs != ATTRS {
        bail!("refusing: MsiDCVarData attributes are 0x{attrs:08X}, expected 0x{ATTRS:08X}");
    }
    Ok((attrs, payload))
}

pub struct Preflight {
    pub state: mux::MuxState,
    pub ap: Ap,
    payload: Vec<u8>,
}

/// Everything that can be checked without writing. Safe for `--dry-run`.
pub fn preflight<H: Hw>(hw: &H, target: MuxMode, boards: &[String], bios: &[String]) -> Result<Preflight> {
    if hw.trigger_marked() {
        bail!("a trigger was already sent this boot; fully power off before another transition");
    }
    let board = hw.board()?;
    if !boards.iter().any(|b| *b == board) {
        bail!("board {board} is not in mux_boards; refusing firmware access");
    }
    let v = hw.bios()?;
    if !bios.iter().any(|b| *b == v) {
        bail!("BIOS {v} is not in mux_bios; the firmware layout is only characterized per BIOS version");
    }
    if !hw.ac_online()? {
        bail!("AC power is required for a GPU mode transition");
    }
    let (attrs, payload) = read_checked(hw)?;
    let mut raw = attrs.to_le_bytes().to_vec();
    raw.extend(&payload);
    let state = mux::decode(&raw)?;
    if state.reboot_required {
        bail!("an earlier transition is pending ({:?} -> {:?}); fully power off before another", state.current, state.pending);
    }
    if !state.available.contains(&target) {
        bail!("{target:?} is not advertised by this firmware");
    }
    if state.current == Some(target) {
        bail!("{target:?} is already active; nothing to do");
    }
    let ap = hw.get_ap()?;
    if ap.apply_ready {
        bail!("apply-ready is already asserted; refusing an ambiguous transition");
    }
    Ok(Preflight { state, ap, payload })
}

pub fn apply<H: Hw>(hw: &H, target: MuxMode, boards: &[String], bios: &[String], log: &dyn Fn(&str)) -> Result<()> {
    let pf = preflight(hw, target, boards, bios)?;
    let before = pf.payload.clone();
    let previous = pf.state.pending.context("unknown pending mode")?;
    let staged = stage(&before, target);

    let restore = |why: &str| -> String {
        let r = (|| -> Result<()> {
            let (a, live) = read_checked(hw)?;
            if stage(&live, previous) != before {
                bail!("non-target firmware state changed; not restoring");
            }
            if live == before {
                return Ok(());
            }
            hw.write_var(a, &stage(&live, previous))?;
            let (_, after) = read_checked(hw)?;
            if after != before { bail!("restoration did not verify"); }
            Ok(())
        })();
        match r {
            Ok(()) => format!("{why}; previous target restored. A trigger may have been accepted: fully power off and verify before retrying"),
            Err(e) => format!("{why}; RESTORATION FAILED: {e:#}. Fully power off and check with `msi-control status`"),
        }
    };

    // Re-check immediately before the first mutation.
    let (attrs, now) = read_checked(hw)?;
    if now != before { bail!("firmware state changed during preflight; nothing was written"); }
    if !hw.ac_online()? { bail!("AC power disappeared before staging; nothing was written"); }

    log("staging target");
    hw.write_var(attrs, &staged).map_err(|e| anyhow::anyhow!(restore(&format!("stage failed: {e:#}"))))?;
    let (_, rb) = read_checked(hw)?;
    if rb != staged { bail!(restore("staged value did not verify")); }

    let step = || -> Result<()> {
        let ap = hw.get_ap()?;
        if ap.apply_ready { bail!("apply-ready asserted before trigger"); }
        if !hw.ac_online()? { bail!("AC power disappeared before trigger"); }
        let trigger = (ap.data_byte & 0xfc) | 0x01;
        log("triggering");
        hw.mark_trigger().context("cannot record trigger attempt; not triggering")?;
        hw.set_data(TRIGGER_ADDR, trigger).context("trigger call failed")?;
        let mut ready = false;
        for _ in 0..20 {
            hw.sleep_ms(250);
            if hw.get_ap()?.apply_ready { ready = true; break; }
        }
        if !ready { bail!("firmware did not assert apply-ready within five seconds"); }
        log("acknowledging");
        hw.set_data(ACK_ADDR, 2).context("acknowledge failed")?;
        Ok(())
    };
    if let Err(e) = step() {
        bail!("{}", restore(&format!("{e:#}")));
    }
    log("done: fully power off (not reboot) to apply");
    Ok(())
}

// ---------------------------------------------------------------- real hardware

nix::ioctl_read!(fs_getflags, b'f', 1, nix::libc::c_long);
nix::ioctl_write_ptr!(fs_setflags, b'f', 2, nix::libc::c_long);
const FS_IMMUTABLE_FL: nix::libc::c_long = 0x10;

pub struct LinuxHw {
    wmi: PathBuf,
}

const WMI_DEV: &str = "ABBC0F6E-8EA1-11D1-00A0-C90629100000-0";

impl LinuxHw {
    /// Validates the WMI device is the characterized MSI interface bound to msi-wmi-platform,
    /// and that the debugfs method files are root-owned 0600.
    pub fn connect() -> Result<Self> {
        if unsafe { nix::libc::geteuid() } != 0 { bail!("root privileges are required"); }
        let dev = PathBuf::from("/sys/bus/wmi/devices").join(WMI_DEV);
        let drv = std::fs::canonicalize(dev.join("driver")).context("MSI WMI device has no bound driver")?;
        if drv.file_name().and_then(|n| n.to_str()) != Some("msi-wmi-platform") {
            bail!("MSI WMI device is not bound to msi-wmi-platform (is the module loaded?)");
        }
        let rd = |f: &str| std::fs::read_to_string(dev.join(f)).map(|s| s.trim().to_owned());
        if rd("guid")? != "ABBC0F6E-8EA1-11D1-00A0-C90629100000" || rd("object_id")? != "AM" || rd("instance_count")? != "1" {
            bail!("MSI WMI device metadata does not match the characterized interface");
        }
        let wmi = PathBuf::from("/sys/kernel/debug").join(format!("msi-wmi-platform-{WMI_DEV}"));
        for m in ["get_ap", "set_data"] {
            let md = std::fs::metadata(wmi.join(m)).with_context(|| format!("{}/{m}: is debugfs mounted?", wmi.display()))?;
            if !md.is_file() || md.uid() != 0 || md.mode() & 0o777 != 0o600 {
                bail!("{m} is not a root-owned 0600 debugfs file");
            }
        }
        Ok(Self { wmi })
    }

    fn call(&self, method: &str, input: [u8; 32]) -> Result<[u8; 32]> {
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(self.wmi.join(method))?;
        if f.write(&input)? != 32 { bail!("partial WMI input"); }
        f.seek(SeekFrom::Start(0))?;
        let mut out = Vec::new();
        f.read_to_end(&mut out)?;
        out.try_into().map_err(|o: Vec<u8>| anyhow::anyhow!("WMI {method} returned {} bytes, expected 32", o.len()))
    }
}

impl Hw for LinuxHw {
    fn read_var(&self) -> Result<(u32, Vec<u8>)> {
        let raw = std::fs::read(mux::EFIVAR)?;
        let a: [u8; 4] = raw.get(..4).context("short efivar")?.try_into()?;
        Ok((u32::from_le_bytes(a), raw[4..].to_vec()))
    }

    fn write_var(&self, attrs: u32, payload: &[u8]) -> Result<()> {
        if payload.len() != LEN || attrs != ATTRS { bail!("refusing to write unexpected variable shape"); }
        let mut enc = attrs.to_le_bytes().to_vec();
        enc.extend(payload);
        let guard = std::fs::File::open(mux::EFIVAR)?;
        let mut flags = 0;
        unsafe { fs_getflags(guard.as_raw_fd(), &mut flags)? };
        if flags & FS_IMMUTABLE_FL != 0 {
            let w = flags & !FS_IMMUTABLE_FL;
            unsafe { fs_setflags(guard.as_raw_fd(), &w)? };
        }
        let res = (|| -> Result<()> {
            let mut f = std::fs::OpenOptions::new().write(true).open(mux::EFIVAR)?;
            if f.write(&enc)? != enc.len() { bail!("partial efivar write"); }
            Ok(())
        })();
        let restored = unsafe { fs_setflags(guard.as_raw_fd(), &flags) };
        res?;
        restored.context("restore efivarfs immutable flag")?;
        Ok(())
    }

    fn get_ap(&self) -> Result<Ap> {
        let raw = self.call("get_ap", [0; 32])?;
        if raw[0] == 0 { bail!("get_ap reported failure"); }
        Ok(Ap { data_byte: raw[2], apply_ready: raw[2] & 0x02 != 0 })
    }

    fn set_data(&self, addr: u8, val: u8) -> Result<()> {
        let mut i = [0u8; 32];
        i[0] = addr;
        i[1] = val;
        if self.call("set_data", i)?[0] == 0 { bail!("set_data(0x{addr:02X}) reported failure"); }
        Ok(())
    }

    fn ac_online(&self) -> Result<bool> {
        for e in std::fs::read_dir("/sys/class/power_supply")? {
            let p = e?.path();
            let rd = |f: &str| std::fs::read_to_string(p.join(f)).map(|s| s.trim().to_owned());
            if rd("type").ok().as_deref() == Some("Mains") && rd("online").ok().as_deref() == Some("1") {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn board(&self) -> Result<String> {
        Ok(std::fs::read_to_string("/sys/class/dmi/id/board_name")?.trim().to_owned())
    }

    fn bios(&self) -> Result<String> {
        Ok(std::fs::read_to_string("/sys/class/dmi/id/bios_version")?.trim().to_owned())
    }

    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }

    fn trigger_marked(&self) -> bool {
        std::path::Path::new(TRIGGER_MARKER).exists()
    }

    fn mark_trigger(&self) -> Result<()> {
        std::fs::create_dir_all("/run/msi-control")?;
        std::fs::write(TRIGGER_MARKER, b"trigger attempted this boot\n")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Mock {
        var: RefCell<Vec<u8>>,
        ac: bool,
        board: &'static str,
        bios: &'static str,
        ready_after: RefCell<i32>, // get_ap calls after trigger before ready; <0 = never
        triggered: RefCell<bool>,
        calls: RefCell<Vec<(u8, u8)>>,
        fail_trigger: bool,
        fail_write: bool,
        marked: RefCell<bool>,
    }

    fn mock(b5: u8) -> Mock {
        let mut v = vec![0u8; LEN];
        v[5] = b5;
        Mock { var: RefCell::new(v), ac: true, board: "MS-15M1", bios: "B1", ready_after: RefCell::new(1), triggered: RefCell::new(false), calls: RefCell::new(vec![]), fail_trigger: false, fail_write: false, marked: RefCell::new(false) }
    }

    impl Hw for Mock {
        fn read_var(&self) -> Result<(u32, Vec<u8>)> { Ok((ATTRS, self.var.borrow().clone())) }
        fn write_var(&self, _: u32, p: &[u8]) -> Result<()> {
            if self.fail_write { bail!("efi write failed"); }
            *self.var.borrow_mut() = p.to_vec();
            Ok(())
        }
        fn get_ap(&self) -> Result<Ap> {
            if *self.triggered.borrow() {
                let mut n = self.ready_after.borrow_mut();
                if *n >= 0 { if *n == 0 { return Ok(Ap { data_byte: 0x82, apply_ready: true }); } *n -= 1; }
            }
            Ok(Ap { data_byte: 0x80, apply_ready: false })
        }
        fn set_data(&self, a: u8, v: u8) -> Result<()> {
            self.calls.borrow_mut().push((a, v));
            if a == TRIGGER_ADDR { if self.fail_trigger { bail!("boom"); } *self.triggered.borrow_mut() = true; }
            Ok(())
        }
        fn ac_online(&self) -> Result<bool> { Ok(self.ac) }
        fn board(&self) -> Result<String> { Ok(self.board.into()) }
        fn bios(&self) -> Result<String> { Ok(self.bios.into()) }
        fn sleep_ms(&self, _: u64) {}
        fn trigger_marked(&self) -> bool { *self.marked.borrow() }
        fn mark_trigger(&self) -> Result<()> { *self.marked.borrow_mut() = true; Ok(()) }
    }

    fn boards() -> Vec<String> { vec!["MS-15M1".into()] }
    fn bios() -> Vec<String> { vec!["B1".into()] }

    #[test]
    fn happy_path_sequence() {
        let m = mock(0x35); // current=target=discrete
        apply(&m, MuxMode::Hybrid, &boards(), &bios(), &|_| {}).unwrap();
        assert_eq!(m.var.borrow()[5], 0x34); // only low 2 bits changed
        assert_eq!(*m.calls.borrow(), vec![(0xd1, 0x81), (0xbe, 2)]);
    }

    #[test]
    fn refuses_unlisted_board_and_battery() {
        let mut m = mock(0x35);
        m.board = "MS-9999";
        assert!(apply(&m, MuxMode::Hybrid, &boards(), &bios(), &|_| {}).is_err());
        let mut m = mock(0x35);
        m.ac = false;
        assert!(apply(&m, MuxMode::Hybrid, &boards(), &bios(), &|_| {}).is_err());
        assert_eq!(m.var.borrow()[5], 0x35);
        assert!(m.calls.borrow().is_empty());
    }

    #[test]
    fn refuses_unknown_bios() {
        let mut m = mock(0x35);
        m.bios = "E15M1IMS.916";
        let e = apply(&m, MuxMode::Hybrid, &boards(), &bios(), &|_| {}).unwrap_err();
        assert!(e.to_string().contains("mux_bios"), "{e}");
        assert!(m.calls.borrow().is_empty());
    }

    #[test]
    fn refuses_noop_and_pending() {
        assert!(apply(&mock(0x35), MuxMode::Discrete, &boards(), &bios(), &|_| {}).is_err());
        assert!(apply(&mock(0x32), MuxMode::Hybrid, &boards(), &bios(), &|_| {}).is_err()); // pending != current
    }

    #[test]
    fn restores_target_when_firmware_never_ready() {
        let m = mock(0x35);
        *m.ready_after.borrow_mut() = -1;
        let e = apply(&m, MuxMode::Hybrid, &boards(), &bios(), &|_| {}).unwrap_err();
        assert!(e.to_string().contains("previous target restored"), "{e}");
        assert_eq!(m.var.borrow()[5], 0x35);
        assert_eq!(*m.calls.borrow(), vec![(0xd1, 0x81)]); // no acknowledge
    }

    #[test]
    fn restores_target_when_trigger_fails() {
        let mut m = mock(0x35);
        m.fail_trigger = true;
        assert!(apply(&m, MuxMode::Integrated, &boards(), &bios(), &|_| {}).is_err());
        assert_eq!(m.var.borrow()[5], 0x35);
    }

    #[test]
    fn no_retry_in_same_boot_after_failed_trigger() {
        let m = mock(0x35);
        *m.ready_after.borrow_mut() = -1;
        assert!(apply(&m, MuxMode::Hybrid, &boards(), &bios(), &|_| {}).is_err());
        // target bits were restored, so only the marker stands between us and a second trigger
        assert_eq!(m.var.borrow()[5], 0x35);
        m.calls.borrow_mut().clear();
        *m.ready_after.borrow_mut() = 1;
        let e = apply(&m, MuxMode::Hybrid, &boards(), &bios(), &|_| {}).unwrap_err();
        assert!(e.to_string().contains("already sent this boot"), "{e}");
        assert!(m.calls.borrow().is_empty());
    }

    #[test]
    fn marker_set_before_trigger_call() {
        let mut m = mock(0x35);
        m.fail_trigger = true;
        assert!(apply(&m, MuxMode::Integrated, &boards(), &bios(), &|_| {}).is_err());
        assert!(*m.marked.borrow());
    }

    #[test]
    fn failed_stage_write_changes_nothing() {
        let mut m = mock(0x35);
        m.fail_write = true;
        assert!(apply(&m, MuxMode::Hybrid, &boards(), &bios(), &|_| {}).is_err());
        assert!(m.calls.borrow().is_empty());
    }

    #[test]
    fn rejects_wrong_variable_length() {
        let m = mock(0x35);
        m.var.borrow_mut().push(0);
        assert!(preflight(&m, MuxMode::Hybrid, &boards(), &bios()).is_err());
    }
}
