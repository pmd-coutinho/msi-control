//! GTK4/libadwaita front-end for msi-controld.
//!
//! State flows one way: the daemon is the source of truth, the window renders a JSON snapshot.
//! A snapshot is fetched on the daemon's `StateChanged` signal and on a 1 s timer. Rendering is
//! diffed against the previous snapshot and skips any control whose write is still in flight, so
//! polling never fights the user. All calls are async; the UI thread never blocks.

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::{HashSet, VecDeque},
    rc::Rc,
    time::Duration,
};

const BUS_NAME: &str = "org.msicontrol.Daemon1";
const OBJ_PATH: &str = "/org/msicontrol/Daemon1";
const HIST_LEN: usize = 90;

const PROFILES: [(&str, &str, &str, (f64, f64, f64)); 3] = [
    ("power-saver", "Power saver", "power-profile-power-saver-symbolic", (0.247, 0.714, 0.545)),
    ("balanced", "Balanced", "power-profile-balanced-symbolic", (0.384, 0.627, 0.918)),
    ("performance", "Performance", "power-profile-performance-symbolic", (0.941, 0.541, 0.235)),
];

const MUX_MODES: [(&str, &str, &str); 3] = [
    ("hybrid", "Hybrid", "Intel drives the screen and NVIDIA helps on demand. Balanced battery and speed."),
    ("discrete", "Discrete", "NVIDIA drives the screen. Fastest and hottest. External ports on Intel go dark."),
    ("integrated", "Integrated", "Intel only, NVIDIA off. Longest battery life."),
];

type Hist = Rc<RefCell<VecDeque<f64>>>;

struct Tile {
    id: &'static str,
    button: gtk::ToggleButton,
    caption: gtk::Label,
    state: gtk::Label,
}

struct Ui {
    win: adw::ApplicationWindow,
    toast: adw::ToastOverlay,
    stack: gtk::Stack,
    proxy: RefCell<Option<gio::DBusProxy>>,
    guard: Cell<bool>,
    busy: Cell<bool>,
    last: RefCell<Value>,
    inflight: RefCell<HashSet<String>>,
    accent: Rc<Cell<(f64, f64, f64)>>,

    profiles: Vec<Tile>,
    override_note: gtk::Label,

    cpu_hist: Hist,
    gpu_hist: Hist,
    cpu_temp: gtk::Label,
    gpu_temp: gtk::Label,
    cpu_fan: gtk::Label,
    gpu_fan: gtk::Label,
    cpu_area: gtk::DrawingArea,
    gpu_area: gtk::DrawingArea,

    shift: adw::ToggleGroup,
    fan: adw::ToggleGroup,
    cooler: adw::SwitchRow,
    superbat: adw::SwitchRow,

    charge_row: adw::ActionRow,
    charge: gtk::Scale,
    charge_timer: RefCell<Option<glib::SourceId>>,

    fn_key: adw::ToggleGroup,
    win_key: adw::ToggleGroup,
    webcam: adw::SwitchRow,
    webcam_block: adw::SwitchRow,

    cpu_group: adw::PreferencesGroup,
    cpu_turbo: adw::SwitchRow,
    cpu_pl1: gtk::Scale,
    cpu_pl2: gtk::Scale,
    cpu_perf: gtk::Scale,

    gpu_group: adw::PreferencesGroup,
    gpu_row: adw::ActionRow,
    gpu_power: gtk::Scale,

    mux: Vec<Tile>,
    mux_banner: adw::Banner,
    mux_apply: gtk::Button,
    mux_draft: RefCell<Option<String>>,
    mux_busy: Cell<bool>,
}

// ---------------------------------------------------------------- widget helpers

fn toggle_group(items: &[(&str, &str)]) -> adw::ToggleGroup {
    let g = adw::ToggleGroup::new();
    g.set_valign(gtk::Align::Center);
    for (name, label) in items {
        g.add(adw::Toggle::builder().name(*name).label(*label).build());
    }
    g
}

fn row_with(title: &str, subtitle: &str, suffix: &impl IsA<gtk::Widget>) -> adw::ActionRow {
    let r = adw::ActionRow::builder().title(title).build();
    if !subtitle.is_empty() {
        r.set_subtitle(subtitle);
    }
    r.add_suffix(suffix);
    r
}

fn tile(id: &'static str, class: &str, icon: Option<&str>, title: &str, caption: &str) -> Tile {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 4);
    outer.set_halign(gtk::Align::Fill);
    outer.set_valign(gtk::Align::Center);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    if let Some(icon) = icon {
        let i = gtk::Image::from_icon_name(icon);
        i.add_css_class("tile-icon");
        head.append(&i);
    }
    let t = gtk::Label::new(Some(title));
    t.add_css_class("heading");
    head.append(&t);
    let state = gtk::Label::new(None);
    state.add_css_class("tile-state");
    state.set_visible(false);
    state.set_hexpand(true);
    state.set_halign(gtk::Align::End);
    head.append(&state);
    outer.append(&head);
    let cap = gtk::Label::new(Some(caption));
    cap.add_css_class("dim-label");
    cap.add_css_class("caption");
    cap.set_wrap(true);
    cap.set_xalign(0.0);
    cap.set_max_width_chars(48);
    outer.append(&cap);
    let b = gtk::ToggleButton::new();
    b.set_child(Some(&outer));
    b.set_hexpand(true);
    b.add_css_class("flat");
    b.add_css_class("tile");
    b.add_css_class(class);
    Tile { id, button: b, caption: cap, state }
}

fn tile_row(tiles: &[Tile]) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.set_homogeneous(true);
    for t in tiles {
        row.append(&t.button);
    }
    for t in tiles.iter().skip(1) {
        t.button.set_group(Some(&tiles[0].button));
    }
    row
}

fn sensor_column(name: &str, temp: &gtk::Label, fan: &gtk::Label, area: &gtk::DrawingArea) -> gtk::Box {
    let col = gtk::Box::new(gtk::Orientation::Vertical, 4);
    col.set_hexpand(true);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let n = gtk::Label::new(Some(name));
    n.add_css_class("sensor-name");
    head.append(&n);
    fan.add_css_class("dim-label");
    fan.add_css_class("fan-reading");
    fan.set_hexpand(true);
    fan.set_halign(gtk::Align::End);
    head.append(fan);
    col.append(&head);
    let big = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    temp.add_css_class("temp");
    big.append(temp);
    let unit = gtk::Label::new(Some("°C"));
    unit.add_css_class("temp-unit");
    unit.set_valign(gtk::Align::Start);
    big.append(&unit);
    col.append(&big);
    col.append(area);
    col
}

fn spark(hist: Hist, color: Rc<dyn Fn() -> (f64, f64, f64)>) -> gtk::DrawingArea {
    let a = gtk::DrawingArea::builder().content_height(60).hexpand(true).build();
    a.set_draw_func(move |_, cr, w, h| {
        let h_ = hist.borrow();
        if h_.len() < 2 {
            return;
        }
        let (w, h) = (w as f64, h as f64);
        let (lo, hi) = (30.0, 100.0);
        let step = w / (HIST_LEN as f64 - 1.0);
        let x = |i: usize| w - (h_.len() - 1 - i) as f64 * step;
        let y = |t: f64| h - 2.0 - ((t.clamp(lo, hi) - lo) / (hi - lo)) * (h - 6.0);
        // 80 °C reference line
        cr.set_source_rgba(0.5, 0.5, 0.5, 0.3);
        cr.set_line_width(1.0);
        cr.set_dash(&[3.0, 4.0], 0.0);
        cr.move_to(0.0, y(80.0));
        cr.line_to(w, y(80.0));
        let _ = cr.stroke();
        cr.set_dash(&[], 0.0);
        let (r, g, b) = color();
        cr.set_line_width(2.0);
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        for (i, t) in h_.iter().enumerate() {
            if i == 0 {
                cr.move_to(x(i), y(*t));
            } else {
                cr.line_to(x(i), y(*t));
            }
        }
        cr.set_source_rgba(r, g, b, 1.0);
        let _ = cr.stroke_preserve();
        cr.line_to(w, h);
        cr.line_to(x(0), h);
        cr.close_path();
        cr.set_source_rgba(r, g, b, 0.12);
        let _ = cr.fill();
    });
    a
}

/// Scrolling the page must not move a power or charge limit by accident.
fn no_wheel(sc: &gtk::Scale) {
    let c = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    c.set_propagation_phase(gtk::PropagationPhase::Capture);
    c.connect_scroll(|_, _, _| glib::Propagation::Stop);
    sc.add_controller(c);
}

fn limit_scale(min: f64, max: f64, step: f64, unit: &'static str) -> gtk::Scale {
    let sc = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
    sc.set_width_request(220);
    sc.set_valign(gtk::Align::Center);
    sc.set_draw_value(true);
    sc.set_value_pos(gtk::PositionType::Left);
    sc.set_format_value_func(move |_, v| format!("{v:.0} {unit}"));
    no_wheel(&sc);
    sc
}

fn battery_text() -> String {
    let dir = std::fs::read_dir("/sys/class/power_supply").ok().and_then(|d| {
        d.filter_map(|e| e.ok().map(|e| e.path())).find(|p| {
            let rd = |f: &str| std::fs::read_to_string(p.join(f)).map(|s| s.trim().to_owned()).unwrap_or_default();
            rd("type") == "Battery" && rd("scope") != "Device" && p.join("capacity").exists()
        })
    });
    let Some(dir) = dir else { return String::new() };
    let rd = |f: &str| std::fs::read_to_string(dir.join(f)).map(|s| s.trim().to_owned()).unwrap_or_default();
    format!("Battery at {}%, {}.", rd("capacity"), rd("status").to_lowercase())
}

fn mode_label(id: &str) -> String {
    MUX_MODES.iter().find(|m| m.0 == id).map_or(id.to_owned(), |m| m.1.to_owned())
}

fn pretty(id: &str) -> String {
    let mut c = id.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

// ---------------------------------------------------------------- construction

fn build_ui(app: &adw::Application) -> Rc<Ui> {
    let accent = Rc::new(Cell::new(PROFILES[1].3));

    // Power profile tiles
    let profiles: Vec<Tile> = PROFILES
        .iter()
        .map(|(id, title, icon, _)| tile(id, &format!("tile-{id}"), Some(icon), title, "–"))
        .collect();
    let override_note = gtk::Label::new(None);
    override_note.add_css_class("override-note");
    override_note.set_xalign(0.0);
    override_note.set_wrap(true);
    override_note.set_visible(false);

    // Thermals
    let cpu_hist: Hist = Rc::default();
    let gpu_hist: Hist = Rc::default();
    let a2 = accent.clone();
    let cpu_area = spark(cpu_hist.clone(), Rc::new(move || a2.get()));
    let gpu_area = spark(gpu_hist.clone(), Rc::new(|| (0.62, 0.66, 0.72)));
    let (cpu_temp, gpu_temp) = (gtk::Label::new(Some("–")), gtk::Label::new(Some("–")));
    let (cpu_fan, gpu_fan) = (gtk::Label::new(None), gtk::Label::new(None));
    let band = gtk::Box::new(gtk::Orientation::Horizontal, 32);
    band.set_homogeneous(true);
    band.add_css_class("thermal-band");
    band.append(&sensor_column("CPU", &cpu_temp, &cpu_fan, &cpu_area));
    band.append(&sensor_column("GPU", &gpu_temp, &gpu_fan, &gpu_area));

    // Performance and cooling
    let shift = toggle_group(&[("eco", "Eco"), ("comfort", "Comfort"), ("turbo", "Turbo")]);
    let fan = toggle_group(&[("auto", "Auto"), ("silent", "Silent"), ("advanced", "Advanced")]);
    let cooler = adw::SwitchRow::builder().title("Cooler boost").subtitle("Run the fans at full speed.").build();
    let perf = adw::PreferencesGroup::builder().title("Performance and cooling").build();
    perf.add(&row_with("Shift mode", "The laptop's own performance mode.", &shift));
    perf.add(&row_with("Fan mode", "", &fan));
    perf.add(&cooler);

    // Battery
    let charge = gtk::Scale::with_range(gtk::Orientation::Horizontal, 20.0, 100.0, 5.0);
    charge.set_width_request(220);
    charge.set_valign(gtk::Align::Center);
    charge.set_draw_value(true);
    charge.set_value_pos(gtk::PositionType::Left);
    no_wheel(&charge);
    for m in [60.0, 80.0, 100.0] {
        charge.add_mark(m, gtk::PositionType::Bottom, None);
    }
    let charge_row = row_with("Charge limit", "", &charge);
    let superbat = adw::SwitchRow::builder().title("Super battery").subtitle("Cut performance to stretch battery life.").build();
    let batt = adw::PreferencesGroup::builder().title("Battery").build();
    batt.add(&charge_row);
    batt.add(&superbat);

    // Keys and camera
    let fn_key = toggle_group(&[("left", "Left"), ("right", "Right")]);
    let win_key = toggle_group(&[("left", "Left"), ("right", "Right")]);
    let webcam = adw::SwitchRow::builder().title("Webcam").build();
    let webcam_block = adw::SwitchRow::builder().title("Webcam hardware block").subtitle("Cuts power to the camera.").build();
    let keys = adw::PreferencesGroup::builder().title("Keyboard and camera").build();
    keys.add(&row_with("Fn key position", "", &fn_key));
    keys.add(&row_with("Windows key position", "", &win_key));
    keys.add(&webcam);
    keys.add(&webcam_block);

    // CPU limits
    let cpu_turbo = adw::SwitchRow::builder().title("Turbo boost").subtitle("Let cores run above their base speed.").build();
    let cpu_pl1 = limit_scale(15.0, 150.0, 1.0, "W");
    let cpu_pl2 = limit_scale(15.0, 220.0, 1.0, "W");
    let cpu_perf = limit_scale(20.0, 100.0, 5.0, "%");
    let cpu_group = adw::PreferencesGroup::builder()
        .title("CPU limits")
        .description("Lower these to run cooler and quieter. They reset on reboot, and a profile can set them again.")
        .build();
    cpu_group.add(&cpu_turbo);
    cpu_group.add(&row_with("Sustained power", "Held during long loads.", &cpu_pl1));
    cpu_group.add(&row_with("Burst power", "Short peaks above the sustained limit.", &cpu_pl2));
    cpu_group.add(&row_with("Top speed", "Share of the maximum frequency.", &cpu_perf));
    cpu_group.set_visible(false);

    // GPU limits
    let gpu_power = limit_scale(5.0, 175.0, 1.0, "W");
    let gpu_row = row_with("Power limit", "", &gpu_power);
    let gpu_group = adw::PreferencesGroup::builder().title("GPU limits").build();
    gpu_group.add(&gpu_row);
    gpu_group.set_visible(false);

    // Graphics
    let mux: Vec<Tile> = MUX_MODES.iter().map(|(id, title, cap)| tile(id, &format!("tile-{id}"), None, title, cap)).collect();
    let mux_banner = adw::Banner::new("");
    let mux_apply = gtk::Button::with_label("Switch graphics…");
    mux_apply.add_css_class("suggested-action");
    mux_apply.set_halign(gtk::Align::End);
    mux_apply.set_sensitive(false);
    let gfx = adw::PreferencesGroup::builder()
        .title("Graphics")
        .description("Switching writes firmware and takes effect after a full power-off, not a reboot.")
        .build();
    let gfx_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
    let mux_row = tile_row(&mux);
    gfx_box.append(&mux_row);
    gfx_box.append(&mux_banner);
    gfx_box.append(&mux_apply);
    gfx.add(&gfx_box);

    // Layout
    let left = gtk::Box::new(gtk::Orientation::Vertical, 18);
    left.set_hexpand(true);
    left.append(&perf);
    left.append(&batt);
    let right = gtk::Box::new(gtk::Orientation::Vertical, 18);
    right.set_hexpand(true);
    right.append(&keys);
    right.append(&cpu_group);
    right.append(&gpu_group);
    let cols = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    cols.set_homogeneous(true);
    cols.append(&left);
    cols.append(&right);

    let profile_title = gtk::Label::new(Some("Power profile"));
    profile_title.add_css_class("title-4");
    profile_title.set_xalign(0.0);
    let page = gtk::Box::new(gtk::Orientation::Vertical, 14);
    page.set_margin_top(18);
    page.set_margin_bottom(28);
    page.set_margin_start(16);
    page.set_margin_end(16);
    page.append(&profile_title);
    let prof_row = tile_row(&profiles);
    page.append(&prof_row);
    page.append(&override_note);
    let sp = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sp.set_margin_top(6);
    page.append(&sp);
    page.append(&band);
    let sp = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sp.set_margin_top(6);
    page.append(&sp);
    page.append(&gfx);
    page.append(&cols);

    let clamp = adw::Clamp::builder().maximum_size(1040).tightening_threshold(760).child(&page).build();
    let scroller = gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();

    let offline = adw::StatusPage::builder()
        .icon_name("dialog-warning-symbolic")
        .title("msi-controld isn't running")
        .description("Start it with “systemctl start msi-controld”. If it keeps stopping, run “journalctl -u msi-controld”.")
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(&scroller, Some("main"));
    stack.add_named(&offline, Some("offline"));
    stack.set_visible_child_name("offline");

    let toast = adw::ToastOverlay::new();
    toast.set_child(Some(&stack));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&toast));

    let win = adw::ApplicationWindow::builder()
        .application(app)
        .title("MSI Control")
        .default_width(1000)
        .default_height(780)
        .width_request(360)
        .height_request(400)
        .content(&view)
        .build();

    // Stack the two columns and the tiles on narrow windows.
    let bp = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 760sp").unwrap());
    bp.add_setter(&cols, "orientation", Some(&gtk::Orientation::Vertical.to_value()));
    bp.add_setter(&band, "orientation", Some(&gtk::Orientation::Vertical.to_value()));
    bp.add_setter(&prof_row, "orientation", Some(&gtk::Orientation::Vertical.to_value()));
    bp.add_setter(&mux_row, "orientation", Some(&gtk::Orientation::Vertical.to_value()));
    win.add_breakpoint(bp);

    Rc::new(Ui {
        win,
        toast,
        stack,
        proxy: RefCell::new(None),
        guard: Cell::new(false),
        busy: Cell::new(false),
        last: RefCell::new(Value::Null),
        inflight: RefCell::default(),
        accent,
        profiles,
        override_note,
        cpu_hist,
        gpu_hist,
        cpu_temp,
        gpu_temp,
        cpu_fan,
        gpu_fan,
        cpu_area,
        gpu_area,
        shift,
        fan,
        cooler,
        superbat,
        charge_row,
        charge,
        charge_timer: RefCell::new(None),
        fn_key,
        win_key,
        webcam,
        webcam_block,
        cpu_group,
        cpu_turbo,
        cpu_pl1,
        cpu_pl2,
        cpu_perf,
        gpu_group,
        gpu_row,
        gpu_power,
        mux,
        mux_banner,
        mux_apply,
        mux_draft: RefCell::new(None),
        mux_busy: Cell::new(false),
    })
}

// ---------------------------------------------------------------- daemon access

impl Ui {
    async fn proxy(self: &Rc<Self>) -> Result<gio::DBusProxy, glib::Error> {
        if let Some(p) = self.proxy.borrow().clone() {
            return Ok(p);
        }
        let p = gio::DBusProxy::for_bus_future(
            gio::BusType::System,
            gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES,
            None,
            BUS_NAME,
            OBJ_PATH,
            BUS_NAME,
        )
        .await?;
        let weak = Rc::downgrade(self);
        p.connect_local("g-signal", false, move |args| {
            let name: String = args.get(2).and_then(|v| v.get().ok()).unwrap_or_default();
            if name == "StateChanged" {
                if let Some(ui) = weak.upgrade() {
                    glib::spawn_future_local(async move { ui.refresh(false).await });
                }
            }
            None
        });
        *self.proxy.borrow_mut() = Some(p.clone());
        Ok(p)
    }

    async fn call(self: &Rc<Self>, method: &str, args: Option<glib::Variant>, timeout_ms: i32) -> Result<glib::Variant, glib::Error> {
        let p = self.proxy().await?;
        p.call_future(method, args.as_ref(), gio::DBusCallFlags::ALLOW_INTERACTIVE_AUTHORIZATION, timeout_ms).await
    }

    fn show_error(&self, e: glib::Error) {
        let mut e = e;
        gio::DBusError::strip_remote_error(&mut e);
        let t = adw::Toast::new(e.message());
        t.set_timeout(6);
        self.toast.add_toast(t);
    }

    /// Send a write, keep the control out of the diff until it settles, then resync.
    fn write(self: &Rc<Self>, path: &str, method: &'static str, args: glib::Variant, timeout_ms: i32) {
        self.inflight.borrow_mut().insert(path.to_owned());
        let (ui, path) = (self.clone(), path.to_owned());
        glib::spawn_future_local(async move {
            let r = ui.call(method, Some(args), timeout_ms).await;
            ui.inflight.borrow_mut().remove(&path);
            if let Err(e) = r {
                ui.show_error(e);
                *ui.last.borrow_mut() = Value::Null; // force a full re-render: reverts the control
            }
            ui.refresh(false).await;
        });
    }

    fn set_attr(self: &Rc<Self>, name: &str, value: &str) {
        self.write(&format!("/ec/{name}"), "SetAttr", (name, value).to_variant(), 25_000);
    }

    async fn refresh(self: &Rc<Self>, sample: bool) {
        if self.busy.replace(true) {
            return;
        }
        let res = self.call("GetState", None, 5_000).await;
        self.busy.set(false);
        match res {
            Ok(v) => {
                let text: String = v.child_value(0).get().unwrap_or_default();
                if let Ok(st) = serde_json::from_str::<Value>(&text) {
                    self.render(&st, sample);
                    self.stack.set_visible_child_name("main");
                }
            }
            Err(_) => {
                *self.proxy.borrow_mut() = None;
                self.stack.set_visible_child_name("offline");
            }
        }
    }

    // ------------------------------------------------------------ rendering

    fn render(&self, st: &Value, sample: bool) {
        self.guard.set(true);
        let old = self.last.borrow().clone();
        let fresh = |path: &str| -> bool {
            !self.inflight.borrow().contains(path) && old.pointer(path) != st.pointer(path)
        };
        let s = |path: &str| st.pointer(path).and_then(Value::as_str).map(str::to_owned);
        let b = |path: &str| st.pointer(path).and_then(Value::as_bool).unwrap_or(false);
        let n = |path: &str| st.pointer(path).and_then(Value::as_f64);

        // Profile tiles, accent, override note
        let active = s("/ppd_profile");
        let ac = st.pointer("/ac_online").and_then(Value::as_bool).unwrap_or(true);
        let mapped = |id: &str, key: &str| -> Option<String> {
            let bat = (!ac).then(|| st.pointer(&format!("/battery_profiles/{id}/{key}")).and_then(Value::as_str)).flatten();
            bat.or_else(|| st.pointer(&format!("/profiles/{id}/{key}")).and_then(Value::as_str)).map(str::to_owned)
        };
        let profile_changed = fresh("/ppd_profile") || fresh("/ec/shift_mode") || fresh("/profiles") || fresh("/battery_profiles") || fresh("/ac_online");
        if profile_changed && !self.inflight.borrow().contains("/ppd_profile") {
            for t in &self.profiles {
                t.button.set_active(active.as_deref() == Some(t.id));
                let shift = mapped(t.id, "shift_mode").unwrap_or_else(|| "?".into());
                let fan = mapped(t.id, "fan_mode").unwrap_or_else(|| "?".into());
                t.caption.set_text(&format!("Shift mode {shift}, fan {fan}."));
                t.state.set_visible(false);
            }
            for (id, _, _, rgb) in PROFILES {
                self.win.remove_css_class(&format!("profile-{id}"));
                if Some(id) == active.as_deref() {
                    self.win.add_css_class(&format!("profile-{id}"));
                    self.accent.set(rgb);
                }
            }
            let want = active.as_deref().and_then(|a| mapped(a, "shift_mode"));
            match (&want, s("/ec/shift_mode")) {
                (Some(w), Some(live)) if *w != live => {
                    let name = PROFILES.iter().find(|p| Some(p.0) == active.as_deref()).map_or("This profile", |p| p.1);
                    self.override_note.set_text(&format!("Shift mode is {live} (set by hand). {name} sets {w}; it applies again the next time you pick a profile."));
                    self.override_note.set_visible(true);
                }
                _ => self.override_note.set_visible(false),
            }
        }

        // Thermals
        for (hist, area, tl, fl, tp, fp) in [
            (&self.cpu_hist, &self.cpu_area, &self.cpu_temp, &self.cpu_fan, "/ec/cpu_temp", "/ec/cpu_fan_pct"),
            (&self.gpu_hist, &self.gpu_area, &self.gpu_temp, &self.gpu_fan, "/ec/gpu_temp", "/ec/gpu_fan_pct"),
        ] {
            if let Some(t) = n(tp) {
                tl.set_text(&format!("{t:.0}"));
                if t >= 90.0 { tl.add_css_class("hot") } else { tl.remove_css_class("hot") }
                if sample {
                    let mut h = hist.borrow_mut();
                    if h.len() >= HIST_LEN {
                        h.pop_front();
                    }
                    h.push_back(t);
                }
                area.queue_draw();
            }
            if let Some(f) = n(fp) {
                fl.set_text(&format!("Fan {f:.0}%"));
            }
        }

        // Performance and cooling
        if fresh("/ec/shift_mode") {
            self.shift.set_active_name(s("/ec/shift_mode").as_deref());
        }
        if fresh("/ec/fan_mode") {
            self.fan.set_active_name(s("/ec/fan_mode").as_deref());
        }
        if fresh("/ec/cooler_boost") {
            self.cooler.set_active(b("/ec/cooler_boost"));
        }
        if fresh("/ec/super_battery") {
            self.superbat.set_active(b("/ec/super_battery"));
        }

        // Battery
        if fresh("/ec/charge_end") {
            if let Some(v) = n("/ec/charge_end") {
                self.charge.set_value(v);
            }
        }
        if !self.inflight.borrow().contains("/ec/charge_end") {
            let v = self.charge.value();
            self.charge_row.set_subtitle(&format!("Stops charging at {v:.0}%. {}", battery_text()));
        }

        // Keys and camera
        if fresh("/ec/fn_key") {
            self.fn_key.set_active_name(s("/ec/fn_key").as_deref());
        }
        if fresh("/ec/win_key") {
            self.win_key.set_active_name(s("/ec/win_key").as_deref());
        }
        if fresh("/ec/webcam") {
            self.webcam.set_active(b("/ec/webcam"));
        }
        if fresh("/ec/webcam_block") {
            self.webcam_block.set_active(b("/ec/webcam_block"));
        }

        // CPU limits (older daemons have no /cpu)
        self.cpu_group.set_visible(st.pointer("/cpu").is_some());
        if fresh("/cpu/no_turbo") {
            self.cpu_turbo.set_active(!b("/cpu/no_turbo"));
        }
        for (path, sc) in [("/cpu/pl1_w", &self.cpu_pl1), ("/cpu/pl2_w", &self.cpu_pl2), ("/cpu/max_perf_pct", &self.cpu_perf)] {
            if fresh(path) {
                if let Some(v) = n(path) {
                    sc.set_value(v);
                }
            }
        }

        // GPU limits. A sleeping GPU is left alone: asking it anything would wake it.
        self.gpu_group.set_visible(b("/gpu/present"));
        let awake = b("/gpu/awake");
        self.gpu_power.set_sensitive(awake);
        if !awake {
            self.gpu_group.set_description(Some("The GPU is asleep (Hybrid mode). Limits appear once something wakes it."));
            self.gpu_row.set_subtitle("");
        } else if let Some(e) = s("/gpu/error") {
            self.gpu_group.set_description(Some(&e));
        } else {
            self.gpu_group.set_description(Some("The shift mode sets this limit on its own; lowering it here lasts until the next shift-mode or profile change, or a driver reload."));
            let (draw, clk) = (n("/gpu/power_draw_w"), n("/gpu/clock_mhz"));
            self.gpu_row.set_subtitle(&format!("Drawing {} W at {} MHz.", draw.map_or("–".into(), |d| format!("{d:.0}")), clk.map_or("–".into(), |c| format!("{c:.0}"))));
            if let (Some(lo), Some(hi)) = (n("/gpu/power_min_w"), n("/gpu/power_max_w")) {
                if self.gpu_power.adjustment().lower() != lo || self.gpu_power.adjustment().upper() != hi {
                    self.gpu_power.set_range(lo, hi);
                }
            }
            if fresh("/gpu/power_limit_w") {
                if let Some(v) = n("/gpu/power_limit_w") {
                    self.gpu_power.set_value(v);
                }
            }
        }

        // Graphics
        let (cur, pend) = (s("/mux/current"), s("/mux/pending"));
        let avail: Vec<String> = st
            .pointer("/mux/available")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        if fresh("/mux") {
            *self.mux_draft.borrow_mut() = None;
        }
        let draft = self.mux_draft.borrow().clone().or_else(|| cur.clone());
        for t in &self.mux {
            t.button.set_sensitive(avail.iter().any(|a| a == t.id));
            t.button.set_active(draft.as_deref() == Some(t.id));
            let label = if cur.as_deref() == Some(t.id) {
                Some("Active now")
            } else if pend.as_deref() == Some(t.id) && pend != cur {
                Some("After power-off")
            } else {
                None
            };
            t.state.set_visible(label.is_some());
            t.state.set_text(label.unwrap_or(""));
        }
        match (&cur, &pend) {
            (Some(c), Some(p)) if c != p => {
                self.mux_banner.set_title(&format!("{} is staged. Fully power off, not reboot, to switch from {}.", mode_label(p), mode_label(c)));
                self.mux_banner.set_revealed(true);
            }
            _ => self.mux_banner.set_revealed(false),
        }
        let staged = cur != pend;
        if !self.mux_busy.get() {
            self.mux_apply.set_sensitive(!staged && draft.is_some() && draft != cur);
        }

        self.guard.set(false);
        *self.last.borrow_mut() = st.clone();
    }
}

// ---------------------------------------------------------------- wiring

fn wire_group(ui: &Rc<Ui>, g: &adw::ToggleGroup, attr: &'static str) {
    let ui = ui.clone();
    g.connect_active_name_notify(move |g| {
        if ui.guard.get() {
            return;
        }
        if let Some(n) = g.active_name() {
            ui.set_attr(attr, &n);
        }
    });
}

fn wire_switch(ui: &Rc<Ui>, r: &adw::SwitchRow, attr: &'static str) {
    let ui = ui.clone();
    r.connect_active_notify(move |r| {
        if !ui.guard.get() {
            ui.set_attr(attr, if r.is_active() { "on" } else { "off" });
        }
    });
}

fn wire(ui: &Rc<Ui>) {
    for t in &ui.profiles {
        let (ui2, id) = (ui.clone(), t.id);
        t.button.connect_toggled(move |b| {
            if !ui2.guard.get() && b.is_active() {
                ui2.write("/ppd_profile", "SetProfile", (id,).to_variant(), 25_000);
            }
        });
    }
    wire_group(ui, &ui.shift, "shift_mode");
    wire_group(ui, &ui.fan, "fan_mode");
    wire_group(ui, &ui.fn_key, "fn_key");
    wire_group(ui, &ui.win_key, "win_key");
    wire_switch(ui, &ui.cooler, "cooler_boost");
    wire_switch(ui, &ui.superbat, "super_battery");
    wire_switch(ui, &ui.webcam, "webcam");
    wire_switch(ui, &ui.webcam_block, "webcam_block");

    // Charge limit: show the value live, write once the slider rests.
    let u = ui.clone();
    ui.charge.connect_value_changed(move |s| {
        if u.guard.get() {
            return;
        }
        u.inflight.borrow_mut().insert("/ec/charge_end".into());
        u.charge_row.set_subtitle(&format!("Stops charging at {:.0}%. {}", s.value(), battery_text()));
        if let Some(id) = u.charge_timer.borrow_mut().take() {
            id.remove();
        }
        let (u2, v) = (u.clone(), s.value() as u32);
        let id = glib::timeout_add_local_once(Duration::from_millis(600), move || {
            u2.charge_timer.borrow_mut().take();
            u2.set_attr("charge_end", &v.to_string());
        });
        *u.charge_timer.borrow_mut() = Some(id);
    });

    // CPU limits
    let u = ui.clone();
    ui.cpu_turbo.connect_active_notify(move |r| {
        if !u.guard.get() {
            u.write("/cpu/no_turbo", "SetAttr", ("cpu_turbo", if r.is_active() { "on" } else { "off" }).to_variant(), 25_000);
        }
    });
    for (sc, attr, path) in [(&ui.cpu_pl1, "cpu_pl1", "/cpu/pl1_w"), (&ui.cpu_pl2, "cpu_pl2", "/cpu/pl2_w"), (&ui.cpu_perf, "cpu_max_perf", "/cpu/max_perf_pct"), (&ui.gpu_power, "gpu_power", "/gpu/power_limit_w")] {
        let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
        let u = ui.clone();
        sc.connect_value_changed(move |s| {
            if u.guard.get() {
                return;
            }
            u.inflight.borrow_mut().insert(path.to_owned());
            if let Some(id) = timer.borrow_mut().take() {
                id.remove();
            }
            let (u2, t2, v) = (u.clone(), timer.clone(), (s.value().round() as u32).to_string());
            let id = glib::timeout_add_local_once(Duration::from_millis(600), move || {
                t2.borrow_mut().take();
                u2.write(path, "SetAttr", (attr, v.as_str()).to_variant(), 25_000);
            });
            *timer.borrow_mut() = Some(id);
        });
    }

    // Graphics: tiles only choose a target; the button runs checks, asks, then writes.
    for t in &ui.mux {
        let (u, id) = (ui.clone(), t.id);
        t.button.connect_toggled(move |b| {
            if u.guard.get() || !b.is_active() {
                return;
            }
            *u.mux_draft.borrow_mut() = Some(id.to_owned());
            let cur = u.last.borrow().pointer("/mux/current").and_then(Value::as_str).map(str::to_owned);
            let staged = u.last.borrow().pointer("/mux/pending").and_then(Value::as_str).map(str::to_owned) != cur;
            u.mux_apply.set_sensitive(!staged && cur.as_deref() != Some(id));
        });
    }
    let u = ui.clone();
    ui.mux_apply.connect_clicked(move |btn| {
        let Some(mode) = u.mux_draft.borrow().clone() else { return };
        if u.mux_busy.replace(true) {
            return;
        }
        let (u, btn) = (u.clone(), btn.clone());
        glib::spawn_future_local(async move {
            btn.set_sensitive(false);
            mux_flow(&u, &btn, &mode).await;
            u.mux_busy.set(false);
            u.refresh(false).await;
        });
    });
}

async fn mux_flow(u: &Rc<Ui>, btn: &gtk::Button, mode: &str) {
    {
        {
            if let Err(e) = u.call("SetMuxMode", Some((mode, true).to_variant()), 30_000).await {
                u.show_error(e);
                return;
            }
            let dlg = adw::AlertDialog::new(
                Some(&format!("Switch graphics to {}?", mode_label(mode))),
                Some("This writes firmware. Keep AC power connected and save your work. Afterwards, fully power off (not reboot) and start the laptop again."),
            );
            dlg.add_responses(&[("cancel", "Cancel"), ("apply", "Switch graphics")]);
            dlg.set_response_appearance("apply", adw::ResponseAppearance::Destructive);
            dlg.set_default_response(Some("cancel"));
            dlg.set_close_response("cancel");
            if dlg.choose_future(Some(btn)).await != "apply" {
                return;
            }
            btn.set_label("Switching…");
            // No effective timeout: this covers the admin prompt plus up to five seconds of firmware wait.
            let r = u.call("SetMuxMode", Some((mode, false).to_variant()), i32::MAX).await;
            btn.set_label("Switch graphics…");
            match r {
                Ok(_) => u.toast.add_toast(adw::Toast::new(&format!("{} is staged. Power off fully to switch.", pretty(mode)))),
                Err(e) => u.show_error(e),
            }
        }
    }
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id("org.msicontrol.Gui").build();
    app.connect_startup(|_| {
        let css = gtk::CssProvider::new();
        css.load_from_string(include_str!("style.css"));
        if let Some(d) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&d, &css, gtk::STYLE_PROVIDER_PRIORITY_USER);
        }
    });
    app.connect_activate(|app| {
        if let Some(w) = app.active_window() {
            w.present();
            return;
        }
        let ui = build_ui(app);
        wire(&ui);
        let u = ui.clone();
        glib::spawn_future_local(async move { u.refresh(true).await });
        let u = ui.clone();
        glib::timeout_add_seconds_local(1, move || {
            let u = u.clone();
            glib::spawn_future_local(async move { u.refresh(true).await });
            glib::ControlFlow::Continue
        });
        ui.win.present();
        // Keep the Ui alive for the lifetime of the window.
        let keep = RefCell::new(Some(ui));
        app.connect_shutdown(move |_| drop(keep.borrow_mut().take()));
    });
    app.run()
}
