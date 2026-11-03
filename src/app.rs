use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use charis_ui::image::{Fit, Image};
use charis_ui::prelude::*;

use crate::config::{Band, Config, MacBinding, Mode};
use crate::dhcp;
use crate::hotspot::native::ClientInfo;
use crate::netcfg;
use crate::tray::{self, TrayAction};
use crate::util;
use crate::worker::{WorkerCommand, WorkerEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Settings,
    Devices,
    Bindings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Stopped,
    Running,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusyKind {
    Starting,
    Stopping,
}

/// UI 消息。所有状态变更都经过这里，视图本身保持只读。
#[derive(Clone)]
pub enum Msg {
    Poll,
    Anim,
    Tab(Tab),
    Ssid(String),
    Passphrase(String),
    ShowPassword(bool),
    Mode(Mode),
    Hidden(bool),
    Band(usize),
    Subnet(String),
    Dns(String),
    Start,
    Stop,
    ToggleTheme,
    CopyCredentials,
    CopyText(String),
    BindLease([u8; 6], Ipv4Addr, String),
    ToggleAddBinding,
    NewMac(String),
    NewIp(String),
    NewName(String),
    AddBinding,
    RemoveBinding(usize),
    CloseRequested,
    TrayShow,
    TrayQuit,
}

pub fn run() -> Result<()> {
    // charis-ui 0.1 的 GPU 后端在部分 Windows 驱动/工具链组合下会直接崩溃
    // （0xC0000005），而这个小窗口用 CPU 渲染已经足够流畅。默认走 CPU，
    // 需要时可用 CHARIS_RENDERER=auto 尝试 GPU。
    if std::env::var_os("CHARIS_RENDERER").is_none() {
        std::env::set_var("CHARIS_RENDERER", "cpu");
    }

    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }

    let options = WindowOptions::new("RustHotSpot")
        .size(500.0, 650.0)
        .min_size(460.0, 520.0)
        .frameless(true)
        .icon_png(include_bytes!("../assets/icon.png"));

    tray::start();
    charis_ui::run(HotspotApp::new(), options).map_err(|err| anyhow::anyhow!("{err}"))
}

pub struct HotspotApp {
    config: Config,
    cmd_tx: Sender<WorkerCommand>,
    evt_rx: Receiver<WorkerEvent>,
    worker: Option<std::thread::JoinHandle<()>>,
    ap_if_index: Option<u32>,
    lease_source: Option<Arc<Mutex<HashMap<[u8; 6], dhcp::Lease>>>>,
    lease_list: Vec<dhcp::Lease>,
    busy: bool,
    busy_kind: Option<BusyKind>,
    busy_since: Option<Instant>,
    phase: Phase,
    message: String,
    message_error: bool,
    clients: Vec<ClientInfo>,
    last_refresh: Instant,
    show_password: bool,
    dns_text: String,
    new_mac: String,
    new_ip: String,
    new_name: String,
    show_add_binding: bool,
    active_tab: Tab,
    tab_anim: Option<Instant>,
    closing: bool,
    hidden: bool,
    tray_ready: bool,
    dark: bool,
    qr_image: Option<Image>,
    qr_key: String,
}

impl HotspotApp {
    pub fn new() -> Self {
        let config = Config::load();
        let dns_text = config.dns.join(", ");
        let dark = config.dark;
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("hotspot-worker".into())
            .spawn(move || crate::worker::worker_loop(cmd_rx, evt_tx))
            .ok();
        Self {
            config,
            cmd_tx,
            evt_rx,
            worker,
            ap_if_index: None,
            lease_source: None,
            lease_list: Vec::new(),
            busy: false,
            busy_kind: None,
            busy_since: None,
            phase: Phase::Stopped,
            message: "就绪".into(),
            message_error: false,
            clients: Vec::new(),
            last_refresh: Instant::now(),
            show_password: false,
            dns_text,
            new_mac: String::new(),
            new_ip: String::new(),
            new_name: String::new(),
            show_add_binding: false,
            active_tab: Tab::Settings,
            tab_anim: None,
            closing: false,
            hidden: false,
            tray_ready: false,
            dark,
            qr_image: None,
            qr_key: String::new(),
        }
    }

    fn is_running(&self) -> bool {
        matches!(self.phase, Phase::Running)
    }

    /// 处理工作线程事件；返回 true 表示现在可以关闭窗口。
    fn poll_worker(&mut self) -> bool {
        while let Ok(event) = self.evt_rx.try_recv() {
            match event {
                WorkerEvent::Started {
                    if_index,
                    message,
                    leases,
                } => {
                    self.ap_if_index = if_index;
                    self.lease_source = leases;
                    self.phase = Phase::Running;
                    self.busy = false;
                    self.busy_kind = None;
                    self.busy_since = None;
                    self.message = message;
                    self.message_error = false;
                    self.rebuild_qr();
                    self.tab_anim = Some(Instant::now());
                }
                WorkerEvent::Failed(err) => {
                    self.phase = Phase::Error;
                    self.busy = false;
                    self.busy_kind = None;
                    self.busy_since = None;
                    self.message = err;
                    self.message_error = true;
                }
                WorkerEvent::Stopped => {
                    self.phase = Phase::Stopped;
                    self.busy = false;
                    self.busy_kind = None;
                    self.busy_since = None;
                    self.ap_if_index = None;
                    self.lease_source = None;
                    self.lease_list.clear();
                    self.clients.clear();
                    self.message = "热点已停止".into();
                    self.message_error = false;
                    self.tab_anim = Some(Instant::now());
                }
                WorkerEvent::Clients(list) => {
                    self.clients = list;
                }
            }
        }

        if self.last_refresh.elapsed() >= Duration::from_millis(900) {
            self.last_refresh = Instant::now();
            if let Some(source) = &self.lease_source {
                if let Ok(map) = source.lock() {
                    let now = Instant::now();
                    self.lease_list = map
                        .values()
                        .filter(|lease| lease.expires > now)
                        .cloned()
                        .collect();
                    self.lease_list.sort_by_key(|lease| lease.ip);
                }
            } else {
                self.lease_list.clear();
            }
        }

        self.closing && !self.busy && !self.is_running()
    }

    fn save_config(&mut self) -> Result<()> {
        self.config.dark = self.dark;
        self.config.dns = self
            .dns_text
            .split(|c| c == ',' || c == ';' || c == ' ')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        if let Ok((_, prefix)) = crate::config::parse_subnet(&self.config.subnet) {
            if prefix == 24 {
                self.config.prefix_len = prefix;
                self.config.subnet = self.config.normalized_subnet();
            }
        }
        self.config.save()
    }

    fn binding_map(&self) -> Result<HashMap<[u8; 6], Ipv4Addr>> {
        self.config.validated_bindings()
    }

    fn ssid_error(&self) -> Option<String> {
        let len = self.config.ssid.chars().count();
        if len == 0 {
            Some("SSID 不能为空".into())
        } else if len > 32 {
            Some("SSID 最多 32 个字符".into())
        } else {
            None
        }
    }

    fn pass_error(&self) -> Option<String> {
        let len = self.config.passphrase.chars().count();
        if len < 8 {
            Some("密码至少需要 8 个字符".into())
        } else if len > 63 {
            Some("密码最多 63 个字符".into())
        } else {
            None
        }
    }

    fn subnet_error(&self) -> Option<String> {
        if self.config.mode != Mode::Hidden {
            return None;
        }
        self.config
            .hidden_network()
            .err()
            .map(|err| err.to_string())
    }

    fn dns_error(&self) -> Option<String> {
        if self.config.mode != Mode::Hidden {
            return None;
        }
        for entry in self
            .dns_text
            .split(|c| c == ',' || c == ';' || c == ' ')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if entry.parse::<Ipv4Addr>().is_err() {
                return Some(format!("DNS 地址非法: {entry}"));
            }
        }
        None
    }

    fn binding_problem(&self) -> Option<String> {
        if self.config.bindings.is_empty() {
            return None;
        }
        self.binding_map()
            .err()
            .map(|err| format!("绑定表有误: {err}"))
    }

    fn start_problem(&self) -> Option<String> {
        self.ssid_error()
            .or_else(|| self.pass_error())
            .or_else(|| self.subnet_error())
            .or_else(|| self.dns_error())
            .or_else(|| {
                if self.config.mode == Mode::Hidden {
                    self.binding_problem()
                } else {
                    None
                }
            })
    }

    fn start_hotspot(&mut self) {
        if self.busy || self.is_running() {
            return;
        }
        if let Some(problem) = self.start_problem() {
            self.phase = Phase::Error;
            self.message = problem;
            self.message_error = true;
            return;
        }
        if let Err(err) = self.save_config() {
            self.phase = Phase::Error;
            self.message = format!("保存配置失败: {err}");
            self.message_error = true;
            return;
        }
        self.busy = true;
        self.busy_kind = Some(BusyKind::Starting);
        self.busy_since = Some(Instant::now());
        self.phase = Phase::Stopped;
        self.message = "正在启动热点…".into();
        self.message_error = false;
        let _ = self
            .cmd_tx
            .send(WorkerCommand::Start(self.config.clone(), self.config.dns_list()));
    }

    fn stop_hotspot(&mut self) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.busy_kind = Some(BusyKind::Stopping);
        self.busy_since = Some(Instant::now());
        self.message = "正在停止热点…".into();
        self.message_error = false;
        let _ = self.cmd_tx.send(WorkerCommand::Stop);
    }

    fn hide_to_tray(&mut self, cx: &mut Cx<Msg>) {
        // 托盘不可用时绝不能把窗口藏起来，否则程序无法恢复。
        if !tray::is_ready() {
            self.request_quit(cx);
            return;
        }
        let _ = self.save_config();
        tray::hide_main_window();
        self.hidden = true;
        self.message = "已最小化到系统托盘（右键托盘图标可退出）".into();
        self.message_error = false;
    }

    fn request_quit(&mut self, cx: &mut Cx<Msg>) {
        if self.closing {
            return;
        }
        let _ = self.save_config();
        self.closing = true;
        if self.is_running() || self.busy {
            if !self.busy {
                self.busy = true;
                self.busy_kind = Some(BusyKind::Stopping);
                self.busy_since = Some(Instant::now());
                self.message = "正在停止热点…".into();
                self.message_error = false;
            }
            let _ = self.cmd_tx.send(WorkerCommand::Stop);
        } else {
            cx.close_window();
        }
    }

    fn wifi_qr_data(&self) -> String {
        fn esc(s: &str) -> String {
            let mut out = String::new();
            for ch in s.chars() {
                if matches!(ch, ';' | ',' | ':' | '"') || ch as u32 == 92 {
                    out.push(char::from(92u8));
                }
                out.push(ch);
            }
            out
        }
        let hidden = self.config.mode == Mode::Hidden && self.config.hidden;
        format!(
            "WIFI:T:WPA;S:{};P:{};{};;",
            esc(&self.config.ssid),
            esc(&self.config.passphrase),
            if hidden { "H:true" } else { "H:false" }
        )
    }

    fn rebuild_qr(&mut self) {
        let data = self.wifi_qr_data();
        if self.qr_key != data {
            self.qr_key = data.clone();
            self.qr_image = qr_image(&data, 320);
        }
    }

    fn check_binding(&self, mac: [u8; 6], ip: Ipv4Addr) -> Result<()> {
        if !self.config.is_hotspot_ip(ip) {
            anyhow::bail!("请使用热点网段内的地址（不能是网关、网络地址或广播地址）");
        }
        if self.config.bindings.iter().any(|binding| {
            util::parse_mac(&binding.mac).ok() != Some(mac)
                && binding.ip.trim().parse::<Ipv4Addr>().ok() == Some(ip)
        }) {
            anyhow::bail!("IP {ip} 已被其他绑定使用");
        }
        if self
            .lease_list
            .iter()
            .any(|lease| lease.ip == ip && lease.mac != mac)
        {
            anyhow::bail!("IP {ip} 当前正被其他设备使用");
        }
        Ok(())
    }

    fn add_binding_direct(&mut self, mac: [u8; 6], ip: Ipv4Addr, name: String) -> Result<()> {
        self.check_binding(mac, ip)?;
        let mac_text = util::format_mac(&mac);
        self.config
            .bindings
            .retain(|binding| util::parse_mac(&binding.mac).ok() != Some(mac));
        self.config.bindings.push(MacBinding {
            mac: mac_text.clone(),
            ip: ip.to_string(),
            name,
        });
        self.apply_bindings()?;
        self.message = format!("已绑定 {mac_text} → {ip}");
        self.message_error = false;
        Ok(())
    }

    fn add_binding(&mut self) -> Result<()> {
        let mac = util::parse_mac(&self.new_mac)?;
        let ip: Ipv4Addr = self
            .new_ip
            .trim()
            .parse()
            .with_context(|| format!("IP 地址非法: {}", self.new_ip))?;
        self.check_binding(mac, ip)?;
        let mac_text = util::format_mac(&mac);
        self.config
            .bindings
            .retain(|binding| util::parse_mac(&binding.mac).ok() != Some(mac));
        self.config.bindings.push(MacBinding {
            mac: mac_text,
            ip: ip.to_string(),
            name: self.new_name.trim().to_string(),
        });
        self.new_mac.clear();
        self.new_ip.clear();
        self.new_name.clear();
        self.apply_bindings()
    }

    fn apply_bindings(&mut self) -> Result<()> {
        if let Ok(map) = self.binding_map() {
            let _ = self.cmd_tx.send(WorkerCommand::SetBindings(map));
        }
        self.save_config()
    }
}

impl Drop for HotspotApp {
    fn drop(&mut self) {
        tray::shutdown();
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl App for HotspotApp {
    type Msg = Msg;

    fn update(&mut self, msg: Msg, cx: &mut Cx<Msg>) {
        // 托盘线程拿不到 Cx，第一次 update 时把 proxy 交给它，之后托盘点击直接变成消息。
        if !self.tray_ready {
            self.tray_ready = true;
            let proxy = cx.proxy();
            tray::attach(move |action| {
                proxy.send(match action {
                    TrayAction::Show => Msg::TrayShow,
                    TrayAction::Quit => Msg::TrayQuit,
                });
            });
        }
        match msg {
            Msg::Poll => {
                if self.poll_worker() {
                    cx.close_window();
                }
            }
            Msg::Anim => {
                if let Some(start) = self.tab_anim {
                    if start.elapsed() >= Duration::from_millis(240) {
                        self.tab_anim = None;
                    }
                }
            }
            Msg::Tab(tab) => {
                if self.active_tab != tab {
                    self.active_tab = tab;
                    self.tab_anim = Some(Instant::now());
                }
            }
            Msg::Ssid(value) => {
                if !self.is_running() {
                    self.config.ssid = value;
                }
            }
            Msg::Passphrase(value) => {
                if !self.is_running() {
                    self.config.passphrase = value;
                }
            }
            Msg::ShowPassword(show) => self.show_password = show,
            Msg::Mode(mode) => {
                if !self.is_running() && self.config.mode != mode {
                    self.config.mode = mode;
                    self.tab_anim = Some(Instant::now());
                }
            }
            Msg::Hidden(hidden) => {
                if !self.is_running() {
                    self.config.hidden = hidden;
                }
            }
            Msg::Band(index) => {
                if !self.is_running() {
                    self.config.band = match index {
                        1 => Band::TwoPointFour,
                        2 => Band::Five,
                        _ => Band::Auto,
                    };
                }
            }
            Msg::Subnet(value) => {
                if !self.is_running() {
                    self.config.subnet = value;
                }
            }
            Msg::Dns(value) => {
                if !self.is_running() {
                    self.dns_text = value;
                }
            }
            Msg::Start => self.start_hotspot(),
            Msg::Stop => self.stop_hotspot(),
            Msg::ToggleTheme => {
                self.dark = !self.dark;
                self.config.dark = self.dark;
                if let Err(err) = self.config.save() {
                    self.message = format!("保存主题失败: {err}");
                    self.message_error = true;
                }
            }
            Msg::CopyCredentials => {
                let text = format!(
                    "网络名称: {}\n密码: {}",
                    self.config.ssid, self.config.passphrase
                );
                cx.copy_to_clipboard(text);
                self.message = "连接信息已复制到剪贴板".into();
                self.message_error = false;
            }
            Msg::CopyText(text) => {
                cx.copy_to_clipboard(text.clone());
                self.message = format!("已复制: {text}");
                self.message_error = false;
            }
            Msg::BindLease(mac, ip, name) => {
                if let Err(err) = self.add_binding_direct(mac, ip, name) {
                    self.message = format!("绑定失败: {err}");
                    self.message_error = true;
                }
            }
            Msg::ToggleAddBinding => {
                self.show_add_binding = !self.show_add_binding;
                if self.show_add_binding {
                    cx.focus("new-mac");
                } else {
                    self.new_mac.clear();
                    self.new_ip.clear();
                    self.new_name.clear();
                }
            }
            Msg::NewMac(value) => self.new_mac = value,
            Msg::NewIp(value) => self.new_ip = value,
            Msg::NewName(value) => self.new_name = value,
            Msg::AddBinding => match self.add_binding() {
                Ok(()) => {
                    self.show_add_binding = false;
                    self.message = "固定 IP 绑定已保存".into();
                    self.message_error = false;
                }
                Err(err) => {
                    self.message = format!("添加失败: {err}");
                    self.message_error = true;
                }
            },
            Msg::RemoveBinding(index) => {
                if index < self.config.bindings.len() {
                    let removed = self.config.bindings.remove(index);
                    match self.apply_bindings() {
                        Ok(()) => {
                            self.message = format!("已删除绑定 {}", removed.mac);
                            self.message_error = false;
                        }
                        Err(err) => {
                            self.message = format!("保存配置失败: {err}");
                            self.message_error = true;
                        }
                    }
                }
            }
            Msg::CloseRequested => self.hide_to_tray(cx),
            Msg::TrayShow => {
                self.hidden = false;
                tray::show_main_window();
            }
            Msg::TrayQuit => self.request_quit(cx),
        }
    }

    fn subscriptions(&self) -> Subscriptions<Msg> {
        // 热点运行/忙碌/退出中需要轮询工作线程；第一次 update 前也轮询一次，
        // 好把托盘代理装好。之后托盘点击通过 proxy 直接唤醒事件循环。
        let polling = self.is_running() || self.busy || self.closing || !self.tray_ready;
        let animating = !self.hidden && (self.busy || self.tab_anim.is_some());
        Subscriptions::none()
            .on_close_request(Msg::CloseRequested)
            .every_if(polling, Duration::from_millis(200), Msg::Poll)
            .every_if(animating, Duration::from_millis(16), Msg::Anim)
    }

    fn theme(&self) -> Theme {
        let base = if self.dark {
            ThemeConfig::dark()
        } else {
            ThemeConfig::light()
        };
        let mut theme = Theme::from_config(ThemeConfig {
            accent: Accent::Teal.color(self.dark),
            gray: GrayTint::Slate,
            radius: 10.0,
            scaling: 1.0,
            density: Density::Default,
            font: FontFamily::Named("Microsoft YaHei UI".into()),
            ..base
        });
        theme.transition = 0.18;
        theme.titlebar_height = 40.0;
        theme
    }

    fn view(&self) -> Element<Msg> {
        let th = theme();
        let c = th.colors.clone();

        let mut root = col()
            .size_full()
            .bg(c.background)
            .child(self.window_chrome())
            .child(self.main_ui());

        if self.busy {
            root = root.child(self.busy_overlay());
        }
        root
    }
}

impl HotspotApp {
    // ------------------------------------------------------------- chrome

    fn window_chrome(&self) -> Element<Msg> {
        let th = theme();
        let c = th.colors.clone();
        let win = window_info();
        row()
            .h(th.titlebar_height)
            .w_full()
            .shrink(0.0)
            .items_center()
            .bg(c.chrome)
            .border_b(1.0, c.border)
            .child(
                row()
                    .h_full()
                    .pl(12.0)
                    .pr(8.0)
                    .gap(7.0)
                    .items_center()
                    .window_drag_area()
                    .child(
                        icon(wifi_icon())
                            .font_size(14.0)
                            .color(c.accent)
                            .pointer_events(false),
                    )
                    .child(
                        text("RustHotSpot")
                            .font_size(12.0)
                            .semibold()
                            .color(c.text_muted)
                            .pointer_events(false),
                    ),
            )
            .child(row().h_full().grow(1.0).min_w(0.0).window_drag_area())
            .child(self.titlebar_right())
            .child(window_controls(win.maximized))
    }

    fn titlebar_right(&self) -> Element<Msg> {
        row()
            .pr(4.0)
            .items_center()
            .child(
                icon_button(if self.dark { Icon::Sun } else { Icon::Moon })
                    .tooltip(if self.dark {
                        "切换到浅色"
                    } else {
                        "切换到深色"
                    })
                    .on_click(Msg::ToggleTheme),
            )
    }

    fn main_ui(&self) -> Element<Msg> {
        let th = theme();
        let c = th.colors.clone();
        col()
            .size_full()
            .bg(c.background)
            .child(self.header_ui())
            .child(
                col()
                    .grow(1.0)
                    .min_h(0.0)
                    .scroll_y()
                    .id("page-scroll")
                    .child(col().w_full().p(14.0).gap(10.0).child(self.page_ui())),
            )
            .child(self.status_bar_ui())
    }

    fn header_ui(&self) -> Element<Msg> {
        let th = theme();
        let c = th.colors.clone();
        col()
            .shrink(0.0)
            .px(16.0)
            .pt(10.0)
            .pb(10.0)
            .gap(8.0)
            .border_b(1.0, c.border)
            .child(
                row()
                    .items_center()
                    .gap(10.0)
                    .child(self.status_tile())
                    .child(
                        col()
                            .gap(2.0)
                            .grow(1.0)
                            .min_w(0.0)
                            .child(
                                text(self.status_title())
                                    .font_size(16.0)
                                    .semibold()
                                    .color(c.text),
                            )
                            .child(
                                text(self.status_subtitle())
                                    .font_size(11.0)
                                    .color(c.text_muted)
                                    .ellipsis(),
                            ),
                    )
                    .child(self.header_action()),
            )
            .child(self.tabs_ui())
    }

    fn status_tile(&self) -> Element<Msg> {
        let th = theme();
        let c = th.colors.clone();
        let (bg, fg, glyph) = match self.phase {
            Phase::Running => (c.accent, c.accent_text, wifi_icon()),
            Phase::Error => (c.danger.with_alpha(0.16), c.danger, Icon::Warning),
            Phase::Stopped => (c.accent_soft, c.accent, wifi_icon()),
        };
        div()
            .center()
            .square(34.0)
            .shrink(0.0)
            .rounded(10.0)
            .bg(bg)
            .color(fg)
            .transition(0.2)
            .child(icon(glyph).font_size(17.0))
    }

    fn status_title(&self) -> &'static str {
        match self.phase {
            Phase::Running => {
                if self.config.mode == Mode::Hidden {
                    "隐藏热点运行中"
                } else {
                    "热点运行中"
                }
            }
            Phase::Error => "启动失败",
            Phase::Stopped => "热点已停止",
        }
    }

    fn status_subtitle(&self) -> String {
        match self.phase {
            Phase::Running => {
                let gateway = self
                    .config
                    .gateway()
                    .map(|g| g.to_string())
                    .unwrap_or_else(|_| "-".into());
                format!("{} · {gateway}", self.config.ssid)
            }
            Phase::Error => "请检查设置后重试".into(),
            Phase::Stopped => "配置好名称与密码后即可启动".into(),
        }
    }

    fn tabs_ui(&self) -> Element<Msg> {
        let device_count = if self.config.mode == Mode::Standard {
            self.clients.len()
        } else {
            self.lease_list.len()
        };
        let binding_count = self.config.bindings.len();
        let device_label = if device_count > 0 {
            format!("设备 {device_count}")
        } else {
            "设备".to_string()
        };
        let binding_label = if binding_count > 0 {
            format!("固定 IP {binding_count}")
        } else {
            "固定 IP".to_string()
        };
        segmented(vec![
            (
                "设置".to_string(),
                self.active_tab == Tab::Settings,
                Msg::Tab(Tab::Settings),
            ),
            (
                device_label,
                self.active_tab == Tab::Devices,
                Msg::Tab(Tab::Devices),
            ),
            (
                binding_label,
                self.active_tab == Tab::Bindings,
                Msg::Tab(Tab::Bindings),
            ),
        ])
    }

    fn header_action(&self) -> Element<Msg> {
        let th = theme();
        if self.busy {
            let label = match self.busy_kind {
                Some(BusyKind::Stopping) => "停止中…",
                _ => "启动中…",
            };
            button(label).disabled(true).h(34.0).px(14.0)
        } else if self.is_running() {
            danger_button("停止")
                .with_icon(stop_icon())
                .h(34.0)
                .px(14.0)
                .on_click(Msg::Stop)
        } else {
            primary_button("启动")
                .with_icon(wifi_icon())
                .h(34.0)
                .px(14.0)
                .on_click(Msg::Start)
        }
        .class("primary-action")
        .apply(|e| e.shadows(th.shadow_sm.clone()))
    }

    fn page_ui(&self) -> Element<Msg> {
        let (opacity, offset) = match self.tab_anim {
            Some(start) => {
                let t = (start.elapsed().as_secs_f32() / 0.22).clamp(0.0, 1.0);
                let eased = Easing::Standard.apply(t);
                (eased, (1.0 - eased) * 10.0)
            }
            None => (1.0, 0.0),
        };
        let page = match self.active_tab {
            Tab::Settings => self.settings_page(),
            Tab::Devices => self.devices_page(),
            Tab::Bindings => self.bindings_page(),
        };
        col()
            .opacity(opacity)
            .translate(0.0, offset)
            .animate_layout(0.22)
            .child(page)
    }

    // ----------------------------------------------------------- settings

    fn settings_page(&self) -> Element<Msg> {
        let mut page = col().gap(10.0);
        if self.is_running() {
            page = page.child(self.share_card());
        }
        page = page.child(self.network_card());
        if self.config.mode == Mode::Hidden {
            page = page.child(self.lan_card());
        }
        page
    }

    fn network_card(&self) -> Element<Msg> {
        let c = theme().colors.clone();
        let running = self.is_running();
        let hidden_mode = self.config.mode == Mode::Hidden;
        let ssid_error = self.ssid_error();
        let pass_error = self.pass_error();
        let mode_hint = if hidden_mode {
            "可隐藏 SSID，并接管 DHCP 与固定 IP"
        } else {
            "更稳定，但不能隐藏 SSID 或自定义 IP"
        };
        let band_tip = (hidden_mode && self.config.band != Band::Auto)
            .then_some("切换频段会重置无线网卡，上游网络短暂断开");

        card()
            .p(14.0)
            .gap(10.0)
            .animate_layout(0.2)
            .child(card_header(
                wifi_icon(),
                "无线网络",
                "名称、密码与工作模式",
                div(),
            ))
            .child(
                form_grid()
                    .child(field_label("名称"))
                    .child(
                        col()
                            .gap(4.0)
                            .child(
                                row()
                                    .gap(8.0)
                                    .items_center()
                                    .child(
                                        text_input(self.config.ssid.clone(), Msg::Ssid)
                                            .id("ssid")
                                            .placeholder("例如 RustHotspot")
                                            .disabled(running)
                                            .grow(1.0),
                                    )
                                    .child_if(hidden_mode, || {
                                        row()
                                            .items_center()
                                            .gap(6.0)
                                            .child(
                                                switch(self.config.hidden)
                                                    .disabled(running)
                                                    .on_click(Msg::Hidden(!self.config.hidden)),
                                            )
                                            .child(
                                                text("隐藏")
                                                    .font_size(11.0)
                                                    .color(c.text_muted),
                                            )
                                    }),
                            )
                            .child_if(ssid_error.is_some(), || {
                                field_hint(ssid_error.clone().unwrap_or_default(), true)
                            }),
                    )
                    .child(field_label("密码"))
                    .child(
                        col()
                            .gap(4.0)
                            .child(
                                row()
                                    .gap(6.0)
                                    .items_center()
                                    .child(
                                        text_input(self.config.passphrase.clone(), Msg::Passphrase)
                                            .id("password")
                                            .apply(|e| {
                                                if self.show_password {
                                                    e
                                                } else {
                                                    e.password()
                                                }
                                            })
                                            .placeholder("至少 8 个字符")
                                            .disabled(running)
                                            .grow(1.0),
                                    )
                                    .child(
                                        icon_button(eye_icon(self.show_password))
                                            .tooltip(if self.show_password {
                                                "隐藏密码"
                                            } else {
                                                "显示密码"
                                            })
                                            .on_click(Msg::ShowPassword(!self.show_password)),
                                    ),
                            )
                            .child_if(pass_error.is_some(), || {
                                field_hint(pass_error.clone().unwrap_or_default(), true)
                            }),
                    )
                    .child(field_label("模式"))
                    .child(
                        segmented(vec![
                            (
                                "Wi-Fi Direct".to_string(),
                                self.config.mode == Mode::Hidden,
                                Msg::Mode(Mode::Hidden),
                            ),
                            (
                                "系统热点".to_string(),
                                self.config.mode == Mode::Standard,
                                Msg::Mode(Mode::Standard),
                            ),
                        ])
                        .pointer_events(!running)
                        .opacity(if running { 0.6 } else { 1.0 }),
                    )
                    .child(field_label("频段"))
                    .child(
                        pick_list(
                            ["自动", "2.4 GHz", "5 GHz"],
                            Some(band_index(self.config.band)),
                            Msg::Band,
                        )
                        .w_full()
                        .disabled(running),
                    ),
            )
            .child(field_hint(mode_hint, false))
            .child_if(band_tip.is_some(), || {
                field_hint(band_tip.unwrap_or_default(), false)
            })
    }

    fn lan_card(&self) -> Element<Msg> {
        let th = theme();
        let c = th.colors.clone();
        let running = self.is_running();
        let subnet_error = self.subnet_error();
        let dns_error = self.dns_error();

        let gateway = self
            .config
            .gateway()
            .map(|g| g.to_string())
            .unwrap_or_else(|_| "-".into());
        let base = self.config.normalized_subnet();
        let prefix = crate::config::parse_subnet(&self.config.subnet)
            .map(|(_, p)| p)
            .unwrap_or(self.config.prefix_len);
        let pool_hint = format!("网关 {gateway} · 动态地址池 {base}.100 – {base}.200");

        card()
            .p(14.0)
            .gap(10.0)
            .animate_layout(0.2)
            .child(card_header(
                network_icon(),
                "局域网",
                "DHCP、NAT 与固定地址",
                div(),
            ))
            .child(
                form_grid()
                    .child(field_label("网段"))
                    .child(
                        col()
                            .gap(4.0)
                            .child(
                                row()
                                    .items_center()
                                    .gap(8.0)
                                    .child(
                                        text_input(self.config.subnet.clone(), Msg::Subnet)
                                            .id("subnet")
                                            .placeholder("10.10.10")
                                            .disabled(running)
                                            .grow(1.0),
                                    )
                                    .child(tag(format!("/{prefix}"), c.text_muted)),
                            )
                            .child_if(subnet_error.is_some(), || {
                                field_hint(subnet_error.clone().unwrap_or_default(), true)
                            }),
                    )
                    .child(field_label("DNS"))
                    .child(
                        col()
                            .gap(4.0)
                            .child(
                                text_input(self.dns_text.clone(), Msg::Dns)
                                    .id("dns")
                                    .placeholder("223.5.5.5, 119.29.29.29")
                                    .disabled(running),
                            )
                            .child_if(dns_error.is_some(), || {
                                field_hint(dns_error.clone().unwrap_or_default(), true)
                            }),
                    ),
            )
            .child(
                row()
                    .items_center()
                    .gap(6.0)
                    .p(8.0)
                    .rounded(7.0)
                    .bg(c.hover)
                    .child(icon(Icon::Info).font_size(12.0).color(c.text_faint))
                    .child(text(pool_hint).font_size(10.5).color(c.text_faint)),
            )
    }

    fn share_card(&self) -> Element<Msg> {
        let th = theme();
        let c = th.colors.clone();
        let hidden = self.config.mode == Mode::Hidden && self.config.hidden;
        let password = if self.show_password {
            self.config.passphrase.clone()
        } else {
            "••••••••".to_string()
        };
        let gateway = self
            .config
            .gateway()
            .map(|g| g.to_string())
            .unwrap_or_else(|_| "-".into());

        let qr = match &self.qr_image {
            Some(src) => div()
                .p(8.0)
                .bg(Color::WHITE)
                .rounded(10.0)
                .shadow(Shadow::new(
                    0.0,
                    3.0,
                    12.0,
                    0.0,
                    Color::BLACK.with_alpha(0.16),
                ))
                .child(image(src.clone()).w(120.0).h(120.0).fit(Fit::Contain)),
            None => div()
                .center()
                .square(136.0)
                .rounded(10.0)
                .bg(c.input)
                .child(icon(Icon::Warning).font_size(22.0).color(c.text_faint)),
        };

        card()
            .p(14.0)
            .gap(10.0)
            .child(card_header(
                smartphone_icon(),
                "扫码连接",
                "扫码，或复制信息后手动连接",
                button("复制")
                    .with_icon(copy_icon())
                    .h(28.0)
                    .px(10.0)
                    .font_size(11.0)
                    .on_click(Msg::CopyCredentials),
            ))
            .child(
                row()
                    .gap(14.0)
                    .items_center()
                    .child(qr)
                    .child(
                        col()
                            .gap(8.0)
                            .grow(1.0)
                            .min_w(0.0)
                            .child(info_field("网络名称", &self.config.ssid, false))
                            .child(info_field("密码", &password, true))
                            .child(info_field("网关", &gateway, true))
                            .child_if(hidden, || {
                                row()
                                    .items_center()
                                    .gap(6.0)
                                    .child(tag("隐藏", c.warning))
                                    .child(
                                        text("需在 Wi-Fi 设置中手动添加")
                                            .font_size(10.5)
                                            .color(c.text_muted),
                                    )
                            })
                            .child(
                                text(format!(
                                    "{} · {}",
                                    if self.config.mode == Mode::Hidden {
                                        "Wi-Fi Direct"
                                    } else {
                                        "系统热点"
                                    },
                                    self.config.band.label()
                                ))
                                .font_size(10.5)
                                .color(c.text_faint),
                            ),
                    ),
            )
    }

    // ------------------------------------------------------------- pages

    fn devices_page(&self) -> Element<Msg> {
        let th = theme();
        let c = th.colors.clone();
        if !self.is_running() {
            return card()
                .p(14.0)
                .gap(8.0)
                .child(card_header(
                    device_icon(),
                    "设备",
                    "热点未运行",
                    tag("离线", c.text_muted),
                ))
                .child(empty_state(
                    device_icon(),
                    "暂无设备",
                    "启动后，连入热点的设备会显示在这里",
                    Some(
                        primary_button("启动")
                            .with_icon(wifi_icon())
                            .on_click(Msg::Start),
                    ),
                ));
        }

        let standard = self.config.mode == Mode::Standard;
        let count = if standard {
            self.clients.len()
        } else {
            self.lease_list.len()
        };

        let mut rows = col().w_full().gap(5.0).animate_layout(0.18);
        if count == 0 {
            rows = rows.child(empty_state(
                device_icon(),
                "暂无设备",
                if standard {
                    "等待设备连接到系统热点"
                } else {
                    "等待客户端发送 DHCP 请求"
                },
                None,
            ));
        } else if standard {
            for client in &self.clients {
                let name = if client.host.is_empty() {
                    "未命名设备".to_string()
                } else {
                    client.host.clone()
                };
                let mac = client.mac.clone();
                rows = rows.child(
                    row()
                        .key(format!("client-{mac}"))
                        .p(9.0)
                        .gap(9.0)
                        .items_center()
                        .rounded(8.0)
                        .bg(c.input)
                        .border(1.0, c.border)
                        .transition(0.12)
                        .hover(|s| s.bg(c.elevated))
                        .child(status_dot(c.success))
                        .child(
                            col()
                                .gap(1.0)
                                .grow(1.0)
                                .min_w(0.0)
                                .child(text(name).font_size(12.0).medium().ellipsis())
                                .child(
                                    row()
                                        .gap(8.0)
                                        .child(text(mac.clone()).mono().font_size(10.5).color(c.text_muted))
                                        .child(
                                            text("系统分配 IP")
                                                .font_size(10.5)
                                                .color(c.text_faint),
                                        ),
                                ),
                        )
                        .child(
                            icon_button(copy_icon())
                                .tooltip("复制 MAC 地址")
                                .on_click(Msg::CopyText(mac)),
                        ),
                );
            }
        } else {
            for lease in &self.lease_list {
                let name = if lease.hostname.is_empty() {
                    "未命名设备".to_string()
                } else {
                    lease.hostname.clone()
                };
                let mac = util::format_mac(&lease.mac);
                let ip = lease.ip.to_string();
                let static_binding = lease.static_binding;
                let dot_color = if static_binding { c.success } else { c.text_faint };
                let bind = (!static_binding).then(|| {
                    button("绑定")
                        .h(26.0)
                        .px(9.0)
                        .font_size(11.0)
                        .on_click(Msg::BindLease(lease.mac, lease.ip, lease.hostname.clone()))
                });
                rows = rows.child(
                    row()
                        .key(format!("lease-{mac}"))
                        .p(9.0)
                        .gap(9.0)
                        .items_center()
                        .rounded(8.0)
                        .bg(c.input)
                        .border(1.0, c.border)
                        .transition(0.12)
                        .hover(|s| s.bg(c.elevated))
                        .child(status_dot(dot_color))
                        .child(
                            col()
                                .gap(1.0)
                                .grow(1.0)
                                .min_w(0.0)
                                .child(text(name).font_size(12.0).medium().ellipsis())
                                .child(
                                    row()
                                        .gap(8.0)
                                        .child(text(mac.clone()).mono().font_size(10.5).color(c.text_muted))
                                        .child(text(ip.clone()).mono().font_size(10.5).color(c.accent)),
                                ),
                        )
                        .child_if(static_binding, || tag("固定", c.success))
                        .child_if(!static_binding, || bind.unwrap())
                        .child(
                            icon_button(copy_icon())
                                .tooltip("复制 IP 地址")
                                .on_click(Msg::CopyText(ip.clone())),
                        ),
                );
            }
        }

        card()
            .p(14.0)
            .gap(10.0)
            .child(card_header(
                device_icon(),
                "设备",
                format!("{count} 台 · 自动刷新"),
                if standard {
                    tag("系统热点", c.text_muted)
                } else {
                    tag("DHCP 租约", c.accent)
                },
            ))
            .child(rows)
    }

    fn bindings_page(&self) -> Element<Msg> {
        let c = theme().colors.clone();
        let standard = self.config.mode == Mode::Standard;
        let problem = self.binding_problem();
        let count = self.config.bindings.len();

        let mut list = col().gap(5.0).animate_layout(0.18);
        if self.config.bindings.is_empty() {
            list = list.child(empty_state(
                link_icon(),
                "还没有固定 IP 绑定",
                "添加后，该 MAC 每次都会拿到同一个 IP",
                Some(
                    button("添加绑定")
                        .with_icon(Icon::Plus)
                        .h(30.0)
                        .px(12.0)
                        .font_size(11.5)
                        .on_click(Msg::ToggleAddBinding),
                ),
            ));
        } else {
            for (index, binding) in self.config.bindings.iter().enumerate() {
                let name = if binding.name.trim().is_empty() {
                    "未命名设备".to_string()
                } else {
                    binding.name.clone()
                };
                let mac = binding.mac.clone();
                let ip = binding.ip.clone();
                list = list.child(
                    row()
                        .key(format!("binding-{mac}"))
                        .p(9.0)
                        .gap(9.0)
                        .items_center()
                        .rounded(8.0)
                        .bg(c.input)
                        .border(1.0, c.border)
                        .transition(0.12)
                        .hover(|s| s.bg(c.elevated))
                        .child(
                            div()
                                .center()
                                .square(26.0)
                                .rounded(7.0)
                                .bg(c.accent_soft)
                                .color(c.accent)
                                .child(icon(link_icon()).font_size(13.0)),
                        )
                        .child(
                            col()
                                .gap(1.0)
                                .grow(1.0)
                                .min_w(0.0)
                                .child(text(name).font_size(12.0).medium().ellipsis())
                                .child(text(mac.clone()).mono().font_size(10.5).color(c.text_muted)),
                        )
                        .child(text(ip).mono().font_size(11.5).color(c.accent))
                        .child(
                            icon_button(trash_icon())
                                .tooltip("删除这个绑定")
                                .on_click(Msg::RemoveBinding(index)),
                        ),
                );
            }
        }

        let add_panel = self.show_add_binding.then(|| {
            div()
                .p(12.0)
                .gap(10.0)
                .rounded(9.0)
                .bg(c.background)
                .border(1.0, c.accent.with_alpha(0.35))
                .animate_layout(0.22)
                .child(
                    div()
                        .grid(vec![
                            Track::Fr(1.3),
                            Track::Fr(1.0),
                            Track::Fr(0.8),
                        ])
                        .gap_xy(8.0, 4.0)
                        .child(
                            col()
                                .gap(3.0)
                                .child(text("MAC 地址").font_size(10.5).color(c.text_muted))
                                .child(
                                    text_input(self.new_mac.clone(), Msg::NewMac)
                                        .id("new-mac")
                                        .placeholder("AA:BB:CC:DD:EE:FF"),
                                ),
                        )
                        .child(
                            col()
                                .gap(3.0)
                                .child(text("固定 IP").font_size(10.5).color(c.text_muted))
                                .child(
                                    text_input(self.new_ip.clone(), Msg::NewIp)
                                        .id("new-ip")
                                        .placeholder("10.10.10.50")
                                        .on_submit(Msg::AddBinding),
                                ),
                        )
                        .child(
                            col()
                                .gap(3.0)
                                .child(text("备注").font_size(10.5).color(c.text_muted))
                                .child(
                                    text_input(self.new_name.clone(), Msg::NewName)
                                        .id("new-name")
                                        .placeholder("客厅电视")
                                        .on_submit(Msg::AddBinding),
                                ),
                        ),
                )
                .child(
                    row()
                        .justify(Justify::End)
                        .gap(8.0)
                        .child(
                            ghost_button("取消")
                                .h(30.0)
                                .px(12.0)
                                .font_size(12.0)
                                .on_click(Msg::ToggleAddBinding),
                        )
                        .child(
                            primary_button("保存")
                                .with_icon(Icon::Check)
                                .h(30.0)
                                .px(12.0)
                                .font_size(12.0)
                                .on_click(Msg::AddBinding),
                        ),
                )
        });

        card()
            .p(14.0)
            .gap(10.0)
            .child(card_header(
                link_icon(),
                "固定 IP",
                format!("{count} 条绑定 · 重连后保持不变"),
                button(if self.show_add_binding {
                    "收起"
                } else {
                    "添加"
                })
                .with_icon(if self.show_add_binding {
                    Icon::ChevronUp
                } else {
                    Icon::Plus
                })
                .h(28.0)
                .px(10.0)
                .font_size(11.0)
                .on_click(Msg::ToggleAddBinding),
            ))
            .child_if(standard, || {
                row()
                    .items_center()
                    .gap(6.0)
                    .p(8.0)
                    .rounded(7.0)
                    .bg(c.warning.with_alpha(0.12))
                    .child(icon(Icon::Info).font_size(12.0).color(c.warning))
                    .child(
                        text("固定 IP 仅在 Wi-Fi Direct 模式下生效")
                            .font_size(10.5)
                            .color(c.warning),
                    )
            })
            .child_if(self.show_add_binding, || add_panel.unwrap())
            .child(list)
            .child_if(problem.is_some(), || {
                row()
                    .items_center()
                    .gap(6.0)
                    .child(icon(Icon::Warning).font_size(12.0).color(c.danger))
                    .child(
                        text(problem.clone().unwrap_or_default())
                            .font_size(10.5)
                            .color(c.danger),
                    )
            })
    }

    fn status_bar_ui(&self) -> Element<Msg> {
        let th = theme();
        let c = th.colors.clone();
        let color = if self.message_error {
            c.danger
        } else {
            c.status_text
        };
        let message = self.message.clone();
        let mode = if self.config.mode == Mode::Hidden {
            "Wi-Fi Direct"
        } else {
            "系统热点"
        };
        status_bar()
            .child(
                row()
                    .grow(1.0)
                    .min_w(0.0)
                    .items_center()
                    .gap(5.0)
                    .px(4.0)
                    .color(color)
                    .child(
                        icon(if self.message_error {
                            Icon::Warning
                        } else {
                            Icon::Info
                        })
                        .font_size(11.0),
                    )
                    .child(
                        text(message.clone())
                            .font_size(11.0)
                            .ellipsis()
                            .tooltip(message),
                    ),
            )
            .child(status_item(
                None,
                format!("{mode} · {}", self.config.band.label()),
            ))
    }

    fn busy_overlay(&self) -> Element<Msg> {
        let th = theme();
        let c = th.colors.clone();
        let since = self.busy_since.unwrap_or_else(Instant::now);
        let elapsed = since.elapsed();
        let t = (elapsed.as_secs_f32() / 0.2).clamp(0.0, 1.0);
        let eased = Easing::Standard.apply(t);
        let angle = elapsed.as_secs_f32() * 5.2;
        let accent = c.accent;
        let (title, subtitle) = match self.busy_kind {
            Some(BusyKind::Stopping) => (
                "正在停止热点",
                if self.closing {
                    "正在清理网卡、NAT 与 DHCP，然后退出"
                } else {
                    "正在恢复网卡设置并清理 DHCP"
                },
            ),
            _ => ("正在启动热点", "正在配置网卡、NAT 与 DHCP，请稍候"),
        };

        div()
            .fixed()
            .inset0()
            .z_index(200)
            .center()
            .bg(Color::BLACK.with_alpha(0.5 * eased))
            .child(
                card()
                    .w(300.0)
                    .p(20.0)
                    .items_center()
                    .gap(10.0)
                    .bg(c.elevated)
                    .border(1.0, c.border_strong)
                    .shadows(th.shadow_popover.clone())
                    .opacity(eased)
                    .translate(0.0, (1.0 - eased) * 12.0)
                    .child(
                        canvas(move |cv, rect| {
                            let center = rect.center();
                            for i in 0..8 {
                                let a =
                                    i as f32 / 8.0 * std::f32::consts::TAU + angle;
                                let alpha = 0.12 + 0.88 * ((i as f32 + 1.0) / 8.0);
                                cv.fill_circle(
                                    Point::new(
                                        center.x + a.cos() * 12.0,
                                        center.y + a.sin() * 12.0,
                                    ),
                                    2.2,
                                    accent.with_alpha(alpha),
                                );
                            }
                        })
                        .square(30.0),
                    )
                    .child(text(title).font_size(13.5).semibold())
                    .child(
                        text(subtitle)
                            .font_size(11.0)
                            .color(c.text_muted)
                            .text_align(TextAlign::Center),
                    ),
            )
    }
}

// ------------------------------------------------------------------ helpers

fn status_dot(color: Color) -> Element<Msg> {
    div()
        .center()
        .square(18.0)
        .shrink(0.0)
        .pill()
        .bg(color.with_alpha(0.16))
        .child(div().square(8.0).pill().bg(color))
}



fn card_header(
    icon_: Icon,
    title: impl Into<String>,
    desc: impl Into<String>,
    trailing: impl Into<Element<Msg>>,
) -> Element<Msg> {
    let th = theme();
    let c = th.colors.clone();
    row()
        .items_center()
        .gap(11.0)
        .child(
            div()
                .center()
                .square(30.0)
                .shrink(0.0)
                .rounded(8.0)
                .bg(c.accent_soft)
                .color(c.accent)
                .child(icon(icon_).font_size(15.0)),
        )
        .child(
            col()
                .gap(1.0)
                .grow(1.0)
                .min_w(0.0)
                .child(text(title.into()).font_size(13.5).semibold())
                .child(
                    text(desc.into())
                        .font_size(10.5)
                        .color(c.text_muted)
                        .ellipsis(),
                ),
        )
        .child(trailing.into())
}

fn form_grid() -> Element<Msg> {
    div()
        .grid(vec![Track::Px(64.0), Track::Fr(1.0)])
        .gap_xy(10.0, 10.0)
        .items(Align::Start)
}

fn field_label(label: &str) -> Element<Msg> {
    let th = theme();
    row()
        .h(th.control_height)
        .shrink(0.0)
        .items_center()
        .child(
            text(label.to_string())
                .font_size(12.0)
                .color(th.colors.text_muted),
        )
}

fn field_hint(text_: impl Into<String>, error: bool) -> Element<Msg> {
    let th = theme();
    let c = th.colors.clone();
    let text_ = text_.into();
    if error {
        row()
            .items_center()
            .gap(4.0)
            .child(icon(Icon::Warning).font_size(11.0).color(c.danger))
            .child(text(text_).font_size(11.0).color(c.danger))
    } else {
        text(text_).font_size(11.0).color(c.text_faint).into()
    }
}

fn info_field(label: &str, value: &str, mono: bool) -> Element<Msg> {
    let th = theme();
    let c = th.colors.clone();
    let mut value_text = text(value.to_string())
        .font_size(12.5)
        .color(c.text)
        .selectable()
        .ellipsis();
    if mono {
        value_text = value_text.mono();
    }
    col()
        .gap(1.0)
        .child(text(label.to_string()).font_size(10.5).color(c.text_faint))
        .child(value_text)
}

fn empty_state(
    icon_: Icon,
    title: &str,
    desc: &str,
    action: Option<Element<Msg>>,
) -> Element<Msg> {
    let th = theme();
    let c = th.colors.clone();
    let mut content = col()
        .w_full()
        .py(20.0)
        .items_center()
        .gap(6.0)
        .child(
            div()
                .center()
                .square(40.0)
                .rounded(11.0)
                .bg(c.hover)
                .child(icon(icon_).font_size(20.0).color(c.text_faint)),
        )
        .child(text(title.to_string()).font_size(13.5).semibold())
        .child(
            text(desc.to_string())
                .font_size(11.0)
                .color(c.text_muted)
                .text_align(TextAlign::Center),
        );
    if let Some(action) = action {
        content = content.child(div().pt(4.0).child(action));
    }
    content
}

fn band_index(band: Band) -> usize {
    match band {
        Band::Auto => 0,
        Band::TwoPointFour => 1,
        Band::Five => 2,
    }
}

// ------------------------------------------------------------------- icons

fn wifi_icon() -> Icon {
    Icon::svg(
        "M5 12.55a11 11 0 0 1 14.08 0 M8.53 16.11a6 6 0 0 1 6.95 0 M12 20h.01 M1.42 9a16 16 0 0 1 21.16 0",
    )
}

fn device_icon() -> Icon {
    Icon::svg(
        "M17 2H7a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V4a2 2 0 0 0-2-2z M12 18h.01",
    )
}

fn smartphone_icon() -> Icon {
    Icon::svg(
        "M17 2H7a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V4a2 2 0 0 0-2-2z M12 18h.01",
    )
}

fn link_icon() -> Icon {
    Icon::svg(
        "M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71 M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71",
    )
}

fn copy_icon() -> Icon {
    Icon::svg(
        "M20 9h-9a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h9a2 2 0 0 0 2-2v-9a2 2 0 0 0-2-2z M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1",
    )
}

fn eye_icon(visible: bool) -> Icon {
    if visible {
        Icon::svg(
            "M9.88 9.88a3 3 0 1 0 4.24 4.24 M10.73 5.08A10.43 10.43 0 0 1 12 5c7 0 10 7 10 7a13.16 13.16 0 0 1-1.67 2.68 M6.61 6.61A13.526 13.526 0 0 0 2 12s3 7 10 7a9.74 9.74 0 0 0 5.39-1.61 M2 2l20 20",
        )
    } else {
        Icon::svg(
            "M2 12s3-7 10-7 10 7 10 7-3 7-10 7-10-7-10-7z M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z",
        )
    }
}

fn trash_icon() -> Icon {
    Icon::svg(
        "M3 6h18 M8 6V4h8v2 M19 6l-1 14H6L5 6 M10 11v6 M14 11v6",
    )
}

fn network_icon() -> Icon {
    Icon::svg(
        "M9 2h6v6H9z M2 16h6v6H2z M16 16h6v6h-6z M12 8v4 M5 16v-2h14v2",
    )
}

fn stop_icon() -> Icon {
    Icon::svg_filled("M7 7h10v10H7z")
}

fn qr_image(data: &str, pixels: usize) -> Option<Image> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    let modules = code.width();
    // 四周留 4 个模块的静区，扫码器更容易定位。
    let total = modules + 8;
    let colors = code.to_colors();
    let mut rgba = vec![255u8; pixels * pixels * 4];
    let cell = pixels as f32 / total as f32;
    for y in 0..modules {
        for x in 0..modules {
            if colors[y * modules + x] == qrcode::Color::Dark {
                let x0 = ((x + 4) as f32 * cell).floor() as usize;
                let y0 = ((y + 4) as f32 * cell).floor() as usize;
                let x1 = (((x + 5) as f32 * cell).ceil() as usize).min(pixels);
                let y1 = (((y + 5) as f32 * cell).ceil() as usize).min(pixels);
                for py in y0..y1 {
                    for px in x0..x1 {
                        let index = (py * pixels + px) * 4;
                        rgba[index] = 0;
                        rgba[index + 1] = 0;
                        rgba[index + 2] = 0;
                        rgba[index + 3] = 255;
                    }
                }
            }
        }
    }
    Image::from_rgba(pixels as u32, pixels as u32, &rgba)
}

/// 无界面自检：启动热点、保持一段时间、清理退出。
pub fn run_headless(seconds: u64) -> Result<()> {
    let mut app = HotspotApp::new();
    app.start_hotspot();
    let wait_deadline = Instant::now() + Duration::from_secs(60);
    while !app.is_running() && Instant::now() < wait_deadline {
        let _ = app.poll_worker();
        if matches!(app.phase, Phase::Error) {
            anyhow::bail!("启动失败: {}", app.message);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if !app.is_running() {
        anyhow::bail!("启动超时: {}", app.message);
    }
    println!("STARTED: {}", app.message);
    println!("MODE: {:?}", app.config.mode);
    println!("SSID: {}", app.config.ssid);
    if let Some(idx) = app.ap_if_index {
        println!("IF_INDEX: {idx}");
        println!("HELPER_IPS: {:?}", netcfg::list_ipv4(idx));
    }
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
        let _ = app.poll_worker();
        for lease in &app.lease_list {
            println!(
                "LEASE {} {} {}",
                util::format_mac(&lease.mac),
                lease.ip,
                lease.hostname
            );
        }
    }
    app.stop_hotspot();
    let stop_deadline = Instant::now() + Duration::from_secs(30);
    while !matches!(app.phase, Phase::Stopped) && Instant::now() < stop_deadline {
        let _ = app.poll_worker();
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("STOPPED: {}", app.message);
    Ok(())
}

#[cfg(test)]
mod ui_smoke_tests {
    use super::*;
    use charis_ui::headless::Headless;

    #[test]
    fn pages_render_and_switch() {
        let mut app = HotspotApp::new();
        app.config.mode = Mode::Hidden;
        app.config.bindings.clear();
        app.phase = Phase::Stopped;
        let mut headless = Headless::new(app, 500.0, 650.0, 1.0);

        assert!(headless.rt.rect_of_text("无线网络").is_some());
        assert!(headless.rt.rect_of_text("设置").is_some());

        headless.rt.send(Msg::Tab(Tab::Devices));
        headless.advance(0.3);
        assert!(headless.rt.rect_of_text("暂无设备").is_some());

        headless.rt.send(Msg::Tab(Tab::Bindings));
        headless.advance(0.3);
        assert!(headless.rt.rect_of_text("固定 IP").is_some());
    }
}
