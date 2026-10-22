//! msi-controld: root daemon owning msi-ec sysfs, MUX status and PPD/tuned sync.

use anyhow::Result;
use msi_control_core::{config::{CONFIG_PATH, Config}, ec::Ec, mux, mux_apply};
use std::{collections::HashMap, sync::Arc, time::{Duration, Instant}};
use tokio::sync::Mutex;
use zbus::{fdo, message::Header, object_server::SignalEmitter, proxy, zvariant::Value};

const ACTION_MANAGE: &str = "org.msicontrol.manage";
const ACTION_MUX: &str = "org.msicontrol.mux";

#[proxy(
    interface = "net.hadess.PowerProfiles",
    default_service = "net.hadess.PowerProfiles",
    default_path = "/net/hadess/PowerProfiles"
)]
trait Ppd {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_active_profile(&self, p: &str) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.PolicyKit1.Authority",
    default_service = "org.freedesktop.PolicyKit1",
    default_path = "/org/freedesktop/PolicyKit1/Authority"
)]
trait Polkit {
    #[allow(clippy::type_complexity)]
    fn check_authorization(
        &self,
        subject: (&str, HashMap<&str, Value<'_>>),
        action_id: &str,
        details: HashMap<&str, &str>,
        flags: u32,
        cancellation_id: &str,
    ) -> zbus::Result<(bool, bool, HashMap<String, String>)>;
}

struct Shared {
    ec: Ec,
    config: Config,
    /// Time of the last change we made, so EC->PPD sync ignores our own writes.
    last_write: Instant,
    ppd_last: Option<String>,
    ec_last: Option<String>,
    ac_last: Option<bool>,
    reapply_at: Option<Instant>,
    /// Fingerprint of everything the GUI shows except live sensors; a change emits StateChanged.
    fingerprint: String,
    /// MUX firmware state changes only through a write or a power cycle, so don't hit the EFI
    /// runtime service every poll.
    mux: Option<mux::MuxState>,
    mux_at: Instant,
}

fn mux_state(sh: &mut Shared) -> Option<mux::MuxState> {
    if sh.mux.is_none() || sh.mux_at.elapsed() > Duration::from_secs(30) {
        sh.mux = mux::read().ok();
        sh.mux_at = Instant::now();
    }
    sh.mux.clone()
}

fn fingerprint(sh: &mut Shared, ppd: &Option<String>) -> String {
    let e = sh.ec.state();
    let m = mux_state(sh);
    format!(
        "{ppd:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        format!("{:?}", msi_control_core::cpu::state()), e.shift_mode, e.fan_mode, e.cooler_boost, e.super_battery, e.webcam, e.webcam_block, e.fn_key, e.win_key, e.charge_end, m.map(|m| (m.current, m.pending))
    )
}

struct Daemon {
    shared: Arc<Mutex<Shared>>,
    system: zbus::Connection,
    mux_lock: Mutex<()>,
}

impl Daemon {
    async fn authorize(&self, header: &Header<'_>, action: &str) -> fdo::Result<()> {
        let sender = header.sender().ok_or_else(|| fdo::Error::AuthFailed("no sender".into()))?;
        let subject = ("system-bus-name", HashMap::from([("name", Value::from(sender.as_str()))]));
        let polkit = PolkitProxy::new(&self.system).await.map_err(fdo::Error::from)?;
        // flags=1: allow interactive authentication.
        let (ok, _, _) = polkit
            .check_authorization(subject, action, HashMap::new(), 1, "")
            .await
            .map_err(fdo::Error::from)?;
        if ok { Ok(()) } else { Err(fdo::Error::AuthFailed("not authorized".into())) }
    }

    async fn snapshot(&self) -> String {
        let mut sh = self.shared.lock().await;
        let mux = mux_state(&mut sh);
        let ppd = match PpdProxy::new(&self.system).await {
            Ok(p) => p.active_profile().await.ok(),
            Err(_) => None,
        };
        serde_json::json!({
            "ec": sh.ec.state(),
            "cpu": msi_control_core::cpu::state(),
            "gpu": tokio::task::spawn_blocking(msi_control_core::gpu::state).await.unwrap_or_default(),
            "mux": mux,
            "ppd_profile": ppd,
            "profiles": sh.config.profiles,
            "battery_profiles": sh.config.battery_profiles,
            "ac_online": msi_control_core::ec::ac_online(),
            "sync": sh.config.sync,
        })
        .to_string()
    }
}

fn fail(e: anyhow::Error) -> fdo::Error {
    fdo::Error::Failed(format!("{e:#}"))
}

#[zbus::interface(name = "org.msicontrol.Daemon1")]
impl Daemon {
    async fn get_state(&self) -> String {
        self.snapshot().await
    }

    /// Set any whitelisted msi-ec/battery attribute (see `Ec::set_attr`).
    async fn set_attr(&self, #[zbus(header)] h: Header<'_>, #[zbus(signal_emitter)] em: SignalEmitter<'_>, name: String, value: String) -> fdo::Result<()> {
        self.authorize(&h, ACTION_MANAGE).await?;
        {
            let mut sh = self.shared.lock().await;
            if name == "shift_mode" {
                sh.last_write = Instant::now();
            }
            if name.starts_with("cpu_") {
                msi_control_core::cpu::set_attr(&name, &value).map_err(fail)?;
            } else if name.starts_with("gpu_") {
                // NVML calls block; keep them off the async executor.
                let (n, v) = (name.clone(), value.clone());
                tokio::task::spawn_blocking(move || msi_control_core::gpu::set_attr(&n, &v))
                    .await
                    .map_err(|e| fdo::Error::Failed(e.to_string()))?
                    .map_err(fail)?;
            } else {
                sh.ec.set_attr(&name, &value).map_err(fail)?;
            }
        }
        let _ = Self::state_changed(&em).await;
        Ok(())
    }

    async fn set_shift_mode(&self, #[zbus(header)] h: Header<'_>, #[zbus(signal_emitter)] em: SignalEmitter<'_>, mode: String) -> fdo::Result<()> {
        self.authorize(&h, ACTION_MANAGE).await?;
        {
            let mut sh = self.shared.lock().await;
            sh.last_write = Instant::now();
            sh.ec.set_shift_mode(&mode).map_err(fail)?;
        }
        let _ = Self::state_changed(&em).await;
        Ok(())
    }

    async fn set_fan_mode(&self, #[zbus(header)] h: Header<'_>, #[zbus(signal_emitter)] em: SignalEmitter<'_>, mode: String) -> fdo::Result<()> {
        self.authorize(&h, ACTION_MANAGE).await?;
        self.shared.lock().await.ec.set_fan_mode(&mode).map_err(fail)?;
        let _ = Self::state_changed(&em).await;
        Ok(())
    }

    async fn set_cooler_boost(&self, #[zbus(header)] h: Header<'_>, #[zbus(signal_emitter)] em: SignalEmitter<'_>, on: bool) -> fdo::Result<()> {
        self.authorize(&h, ACTION_MANAGE).await?;
        self.shared.lock().await.ec.set_cooler_boost(on).map_err(fail)?;
        let _ = Self::state_changed(&em).await;
        Ok(())
    }

    async fn set_super_battery(&self, #[zbus(header)] h: Header<'_>, #[zbus(signal_emitter)] em: SignalEmitter<'_>, on: bool) -> fdo::Result<()> {
        self.authorize(&h, ACTION_MANAGE).await?;
        self.shared.lock().await.ec.set_super_battery(on).map_err(fail)?;
        let _ = Self::state_changed(&em).await;
        Ok(())
    }

    /// Select a PPD profile (power-saver/balanced/performance) and apply its EC side right away.
    async fn set_profile(&self, #[zbus(header)] h: Header<'_>, #[zbus(signal_emitter)] em: SignalEmitter<'_>, profile: String) -> fdo::Result<()> {
        self.authorize(&h, ACTION_MANAGE).await?;
        PpdProxy::new(&self.system).await?.set_active_profile(&profile).await?;
        {
            let mut sh = self.shared.lock().await;
            if sh.config.sync {
                apply_profile(&mut sh, &profile);
                sh.reapply_at = Some(Instant::now() + Duration::from_secs(3));
            }
            sh.ppd_last = Some(profile);
            sh.ec_last = sh.ec.state().shift_mode;
        }
        let _ = Self::state_changed(&em).await;
        Ok(())
    }

    /// Stage a GPU MUX mode natively (firmware write; needs a full power-off to take effect).
    /// With `dry_run`, only runs preflight (reads firmware state, one read-only WMI query).
    async fn set_mux_mode(&self, #[zbus(header)] h: Header<'_>, #[zbus(signal_emitter)] em: SignalEmitter<'_>, mode: String, dry_run: bool) -> fdo::Result<String> {
        // Dry-run writes nothing, so it only needs the regular action; real switching is auth_admin.
        self.authorize(&h, if dry_run { ACTION_MANAGE } else { ACTION_MUX }).await?;
        let target = mux::mode_arg(&mode).ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown mode {mode:?}")))?;
        let (boards, bios) = {
            let sh = self.shared.lock().await;
            (sh.config.mux_boards.clone(), sh.config.mux_bios.clone())
        };
        let _guard = self.mux_lock.lock().await;
        let out = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let hw = mux_apply::LinuxHw::connect()?;
            if dry_run {
                let pf = mux_apply::preflight(&hw, target, &boards, &bios)?;
                return Ok(format!("preflight ok: {:?} -> {:?}, apply-ready clear", pf.state.current, target));
            }
            let log = std::cell::RefCell::new(Vec::<String>::new());
            mux_apply::apply(&hw, target, &boards, &bios, &|m| log.borrow_mut().push(m.to_owned()))?;
            Ok(log.into_inner().join("\n"))
        })
        .await
        .map_err(|e| fdo::Error::Failed(e.to_string()));
        self.shared.lock().await.mux = None;
        let _ = Self::state_changed(&em).await;
        out?.map_err(fail)
    }

    #[zbus(signal)]
    async fn state_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

fn apply_profile(sh: &mut Shared, profile: &str) {
    let on_battery = !msi_control_core::ec::ac_online();
    let chosen = if on_battery { sh.config.battery_profiles.get(profile) } else { None };
    let Some(p) = chosen.or_else(|| sh.config.profiles.get(profile)).cloned() else { return };
    sh.last_write = Instant::now();
    let results = [
        sh.ec.set_shift_mode(&p.shift_mode),
        sh.ec.set_fan_mode(&p.fan_mode),
        sh.ec.set_cooler_boost(p.cooler_boost),
    ]
    .into_iter()
    .chain(msi_control_core::cpu::apply(&p.cpu))
    .chain(msi_control_core::gpu::apply(&p.gpu));
    for r in results {
        if let Err(e) = r {
            eprintln!("apply profile {profile}: {e:#}");
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let ec = Ec::system();
    if !ec.available() {
        eprintln!("msi-ec sysfs not found; is the msi_ec module loaded?");
    }
    let config = Config::load(CONFIG_PATH);
    eprintln!("msi-controld: sync={} ec_to_ppd={} mux_boards={:?} profiles={:?} battery_profiles={:?}", config.sync, config.ec_to_ppd, config.mux_boards, config.profiles, config.battery_profiles);
    let shared = Arc::new(Mutex::new(Shared {
        ec,
        config,
        last_write: Instant::now() - Duration::from_secs(60),
        ppd_last: None,
        ec_last: None,
        ac_last: None,
        reapply_at: None,
        fingerprint: String::new(),
        mux: None,
        mux_at: Instant::now(),
    }));
    let conn = zbus::connection::Builder::system()?
        .name("org.msicontrol.Daemon1")?
        .serve_at("/org/msicontrol/Daemon1", Daemon { shared: shared.clone(), system: zbus::Connection::system().await?, mux_lock: Mutex::new(()) })?
        .build()
        .await?;
    let iface = conn.object_server().interface::<_, Daemon>("/org/msicontrol/Daemon1").await?;

    // Poll loop. PPD -> EC on profile change (and on AC plug/unplug); optional EC -> PPD; and a
    // StateChanged signal whenever anything the GUI shows changed, whoever changed it.
    loop {
        let ppd = PpdProxy::new(&conn).await.ok();
        let active = match &ppd {
            Some(p) => p.active_profile().await.ok(),
            None => None,
        };
        {
            let mut sh = shared.lock().await;
            let ac = msi_control_core::ec::ac_online();
            if sh.config.sync {
                let shift = sh.ec.state().shift_mode;
                if active != sh.ppd_last {
                    if let Some(a) = &active {
                        apply_profile(&mut sh, a);
                        // tuned applies its own sysfs settings asynchronously; re-assert ours once.
                        sh.reapply_at = Some(Instant::now() + Duration::from_secs(3));
                    }
                    sh.ppd_last = active.clone();
                    sh.ec_last = sh.ec.state().shift_mode;
                } else if sh.ac_last.is_some_and(|prev| prev != ac) {
                    if let Some(a) = &active {
                        apply_profile(&mut sh, a);
                        sh.reapply_at = Some(Instant::now() + Duration::from_secs(3));
                    }
                    sh.ec_last = sh.ec.state().shift_mode;
                } else if sh.reapply_at.is_some_and(|t| Instant::now() >= t) {
                    sh.reapply_at = None;
                    if let Some(a) = &active {
                        apply_profile(&mut sh, a);
                    }
                    sh.ec_last = sh.ec.state().shift_mode;
                } else if sh.config.ec_to_ppd && shift != sh.ec_last {
                    // Ignore writes from ourselves or from tuned's sysfs plugin settling after a switch.
                    if sh.last_write.elapsed() > Duration::from_secs(5) {
                        if let (Some(ppd), Some(p)) = (&ppd, shift.as_deref().and_then(|s| sh.config.profile_for_shift(s)).map(str::to_owned)) {
                            if Some(&p) != active.as_ref() {
                                let _ = ppd.set_active_profile(&p).await;
                                sh.ppd_last = Some(p);
                            }
                        }
                    }
                    sh.ec_last = shift;
                }
            }
            sh.ac_last = Some(ac);
            let fp = fingerprint(&mut sh, &active);
            if fp != sh.fingerprint {
                sh.fingerprint = fp;
                let _ = Daemon::state_changed(iface.signal_emitter()).await;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}
