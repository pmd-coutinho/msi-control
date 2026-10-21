//! Status-notifier tray icon: shows the active power profile and lets you change it.
//! Works on KDE Plasma and on Hyprland bars that host StatusNotifierItems (Noctalia, Waybar).

use futures_util::StreamExt;
use ksni::{
    MenuItem, ToolTip, Tray, TrayMethods,
    menu::{RadioGroup, RadioItem, StandardItem},
};
use std::{process::Command, time::Duration};
use zbus::proxy;

#[proxy(
    interface = "org.msicontrol.Daemon1",
    default_service = "org.msicontrol.Daemon1",
    default_path = "/org/msicontrol/Daemon1"
)]
trait Daemon {
    fn get_state(&self) -> zbus::Result<String>;
    fn set_profile(&self, profile: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    fn state_changed(&self) -> zbus::Result<()>;
}

const PROFILES: [(&str, &str); 3] = [("power-saver", "Power saver"), ("balanced", "Balanced"), ("performance", "Performance")];

#[derive(Default)]
struct MsiTray {
    online: bool,
    profile: Option<String>,
    cpu: Option<u64>,
    gpu: Option<u64>,
}

impl Tray for MsiTray {
    fn id(&self) -> String {
        "msi-control".into()
    }

    fn title(&self) -> String {
        "MSI Control".into()
    }

    fn icon_name(&self) -> String {
        match (self.online, self.profile.as_deref()) {
            (true, Some(p)) => format!("power-profile-{p}-symbolic"),
            _ => "dialog-warning-symbolic".into(),
        }
    }

    fn tool_tip(&self) -> ToolTip {
        let description = if !self.online {
            "msi-controld is not running".to_owned()
        } else {
            let t = |v: Option<u64>| v.map_or("–".to_owned(), |v| format!("{v} °C"));
            format!("{}. CPU {}, GPU {}.", self.profile_label(), t(self.cpu), t(self.gpu))
        };
        ToolTip { title: "MSI Control".into(), description, ..Default::default() }
    }

    // Left click opens the window.
    fn activate(&mut self, _x: i32, _y: i32) {
        open_window();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let selected = PROFILES.iter().position(|(id, _)| Some(*id) == self.profile.as_deref()).unwrap_or(1);
        vec![
            RadioGroup {
                selected,
                select: Box::new(|this: &mut Self, i| {
                    let id = PROFILES[i].0;
                    this.profile = Some(id.to_owned());
                    tokio::spawn(async move {
                        if let Ok(c) = zbus::Connection::system().await {
                            if let Ok(d) = DaemonProxy::new(&c).await {
                                let _ = d.set_profile(id).await;
                            }
                        }
                    });
                }),
                options: PROFILES
                    .iter()
                    .map(|(id, label)| RadioItem {
                        label: (*label).into(),
                        icon_name: format!("power-profile-{id}-symbolic"),
                        enabled: self.online,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem { label: "Open MSI Control".into(), activate: Box::new(|_| open_window()), ..Default::default() }.into(),
            StandardItem { label: "Quit".into(), icon_name: "application-exit".into(), activate: Box::new(|_| std::process::exit(0)), ..Default::default() }.into(),
        ]
    }
}

impl MsiTray {
    fn profile_label(&self) -> &str {
        PROFILES.iter().find(|(id, _)| Some(*id) == self.profile.as_deref()).map_or("Unknown profile", |p| p.1)
    }
}

fn open_window() {
    let _ = Command::new("msi-control-gui").spawn();
}

async fn fetch(conn: &zbus::Connection) -> Option<(Option<String>, Option<u64>, Option<u64>)> {
    let d = DaemonProxy::new(conn).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&d.get_state().await.ok()?).ok()?;
    Some((
        v["ppd_profile"].as_str().map(str::to_owned),
        v.pointer("/ec/cpu_temp").and_then(|x| x.as_u64()),
        v.pointer("/ec/gpu_temp").and_then(|x| x.as_u64()),
    ))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let handle = MsiTray::default().spawn().await?;
    let conn = zbus::Connection::system().await?;
    let mut changed = DaemonProxy::new(&conn).await?.receive_state_changed().await?;
    loop {
        let state = fetch(&conn).await;
        handle
            .update(|t| match state {
                Some((p, c, g)) => (t.online, t.profile, t.cpu, t.gpu) = (true, p, c, g),
                None => t.online = false,
            })
            .await;
        // Redraw on the daemon's signal, and every few seconds for the temperatures.
        tokio::select! {
            _ = changed.next() => {}
            _ = tokio::time::sleep(Duration::from_secs(3)) => {}
        }
    }
}
