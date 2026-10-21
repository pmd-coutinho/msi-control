use anyhow::Result;
use clap::{Parser, Subcommand};
use zbus::proxy;

#[proxy(
    interface = "org.msicontrol.Daemon1",
    default_service = "org.msicontrol.Daemon1",
    default_path = "/org/msicontrol/Daemon1"
)]
trait Daemon {
    fn get_state(&self) -> zbus::Result<String>;
    fn set_shift_mode(&self, mode: &str) -> zbus::Result<()>;
    fn set_fan_mode(&self, mode: &str) -> zbus::Result<()>;
    fn set_cooler_boost(&self, on: bool) -> zbus::Result<()>;
    fn set_super_battery(&self, on: bool) -> zbus::Result<()>;
    fn set_profile(&self, profile: &str) -> zbus::Result<()>;
    fn set_attr(&self, name: &str, value: &str) -> zbus::Result<()>;
    fn set_mux_mode(&self, mode: &str, dry_run: bool) -> zbus::Result<String>;
}

#[derive(Parser)]
#[command(version, about = "Control MSI laptop EC, GPU MUX and power profiles")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print full state as JSON
    Status,
    /// Set PPD profile (power-saver | balanced | performance); EC follows
    Profile { name: String },
    Shift { mode: String },
    Fan { mode: String },
    CoolerBoost { state: OnOff },
    SuperBattery { state: OnOff },
    /// Set any attribute: shift_mode fan_mode cooler_boost super_battery webcam webcam_block fn_key win_key charge_end
    Set { name: String, value: String },
    /// Stage GPU MUX mode (hybrid | discrete | integrated); fully power off afterwards
    Mux {
        mode: String,
        /// Run preflight checks only; write nothing
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Clone, clap::ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let conn = zbus::Connection::system().await?;
    let d = DaemonProxy::new(&conn).await?;
    match cli.cmd {
        Cmd::Status => {
            let v: serde_json::Value = serde_json::from_str(&d.get_state().await?)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::Profile { name } => d.set_profile(&name).await?,
        Cmd::Shift { mode } => d.set_shift_mode(&mode).await?,
        Cmd::Fan { mode } => d.set_fan_mode(&mode).await?,
        Cmd::CoolerBoost { state } => d.set_cooler_boost(matches!(state, OnOff::On)).await?,
        Cmd::Set { name, value } => d.set_attr(&name, &value).await?,
        Cmd::Mux { mode, dry_run } => println!("{}", d.set_mux_mode(&mode, dry_run).await?),
        Cmd::SuperBattery { state } => d.set_super_battery(matches!(state, OnOff::On)).await?,
    }
    Ok(())
}
