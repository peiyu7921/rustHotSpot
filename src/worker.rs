//! 热点工作线程。
//!
//! WinRT 的 Wi-Fi Direct 对象不是 agile 的，不能跨线程创建后交给 UI 使用
//! （会导致段错误）。因此所有热点对象都只在这个线程里创建、使用和销毁，
//! UI 通过 channel 发送命令、接收状态。

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{Config, Mode};
use crate::dhcp::{self, DhcpServer, DhcpSettings, Lease};
use crate::hotspot::{self, native::ClientInfo, wifidirect::WifiDirectAp, Backend};
use crate::netcfg;

pub enum WorkerCommand {
    Start(Config, Vec<Ipv4Addr>),
    Stop,
    SetBindings(HashMap<[u8; 6], Ipv4Addr>),
    Shutdown,
}

pub enum WorkerEvent {
    Started {
        if_index: Option<u32>,
        message: String,
        leases: Option<Arc<Mutex<HashMap<[u8; 6], Lease>>>>,
    },
    Failed(String),
    Stopped,
    Clients(Vec<ClientInfo>),
}

struct State {
    backend: Option<Backend>,
    dhcp: Option<DhcpServer>,
    if_index: Option<u32>,
    band_backup: Option<netcfg::BandBackup>,
    gateway: Option<Ipv4Addr>,
}

impl State {
    fn new() -> Self {
        Self {
            backend: None,
            dhcp: None,
            if_index: None,
            band_backup: None,
            gateway: None,
        }
    }

    fn cleanup(&mut self) {
        netcfg::remove_dhcp_firewall();
        self.dhcp.take();
        if let Some(mut backend) = self.backend.take() {
            backend.stop();
        }
        std::thread::sleep(Duration::from_millis(800));
        if let Some(backup) = self.band_backup.take() {
            netcfg::restore_band_preference(&backup);
        }
        if let Some(idx) = self.if_index.take() {
            netcfg::remove_nat();
            if let Some(gateway) = self.gateway.take() {
                let _ = netcfg::delete_ipv4(idx, gateway);
            }
            let _ = netcfg::delete_ipv4(idx, Ipv4Addr::new(192, 168, 137, 1));
        }
    }
}

pub fn worker_loop(cmd_rx: Receiver<WorkerCommand>, evt_tx: Sender<WorkerEvent>) {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }

    let mut state = State::new();

    loop {
        match cmd_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(WorkerCommand::Start(config, dns)) => {
                state.cleanup();
                state.gateway = config.gateway().ok();
                match build_hotspot(&config, dns, &mut state) {
                    Ok(message) => {
                        let leases = state.dhcp.as_ref().map(|d| d.leases_shared());
                        let _ = evt_tx.send(WorkerEvent::Started {
                            if_index: state.if_index,
                            message,
                            leases,
                        });
                    }
                    Err(err) => {
                        state.cleanup();
                        let _ = evt_tx.send(WorkerEvent::Failed(err.to_string()));
                    }
                }
            }
            Ok(WorkerCommand::Stop) => {
                state.cleanup();
                let _ = evt_tx.send(WorkerEvent::Stopped);
            }
            Ok(WorkerCommand::SetBindings(map)) => {
                if let Some(dhcp) = &state.dhcp {
                    dhcp.set_bindings(map);
                }
            }
            Ok(WorkerCommand::Shutdown) => {
                state.cleanup();
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(Backend::Native(hotspot)) = &state.backend {
                    let _ = evt_tx.send(WorkerEvent::Clients(hotspot.clients()));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                state.cleanup();
                break;
            }
        }
    }
}

fn build_hotspot(config: &Config, dns: Vec<Ipv4Addr>, state: &mut State) -> Result<String> {
    match config.mode {
        Mode::Standard => {
            let hotspot = hotspot::start_native(&config.ssid, &config.passphrase, config.band)?;
            state.backend = Some(Backend::Native(hotspot));
            Ok("标准热点已启动".to_string())
        }
        Mode::Hidden => {
            let (gateway, prefix_len) = config.hidden_network()?;
            let bindings = config.validated_bindings()?;
            match netcfg::apply_band_preference(config.band) {
                Ok(backup) => state.band_backup = backup,
                Err(err) => log::warn!("设置频段偏好失败，使用自动频段: {err}"),
            }
            let ap = start_ap_with_retry(&config.ssid, &config.passphrase, config.hidden)?;
            state.backend = Some(Backend::WifiDirect(ap));

            let adapter = wait_for_ap_adapter()?;
            state.if_index = Some(adapter.if_index);

            // 清掉 Windows 为 Wi-Fi Direct AP 自动配置的 ICS 地址和 APIPA 地址
            for ip in netcfg::list_ipv4(adapter.if_index) {
                if ip == Ipv4Addr::new(192, 168, 137, 1) || ip.is_link_local() || ip == gateway {
                    let _ = netcfg::delete_ipv4(adapter.if_index, ip);
                }
            }
            netcfg::set_ipv4(adapter.if_index, gateway, prefix_len)?;
            wait_for_ip(adapter.if_index, gateway, Duration::from_secs(6))?;

            let prefix = format!("{}.0/{}", config.normalized_subnet(), prefix_len);
            netcfg::ensure_nat(&prefix)?;

            let settings = DhcpSettings {
                gateway,
                prefix_len,
                dns,
                lease_secs: 86400,
                pool_start: 100,
                pool_end: 200,
                bindings,
                if_index: adapter.if_index,
            };
            netcfg::ensure_dhcp_firewall()?;
            state.dhcp = Some(dhcp::start(settings)?);
            log::info!(
                "隐藏热点已就绪: SSID={} 网关={} 网卡={}",
                config.ssid,
                gateway,
                adapter.friendly_name
            );
            Ok("隐藏热点已启动".to_string())
        }
    }
}

fn start_ap_with_retry(ssid: &str, passphrase: &str, hidden: bool) -> Result<WifiDirectAp> {
    let mut last_err = None;
    for attempt in 1..=3 {
        match WifiDirectAp::start(ssid, passphrase, hidden) {
            Ok(ap) => return Ok(ap),
            Err(err) => {
                log::warn!("第 {attempt} 次启动 Wi-Fi Direct AP 失败: {err}");
                last_err = Some(err);
                if attempt < 3 {
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Wi-Fi Direct AP 启动失败")))
}

fn wait_for_ap_adapter() -> Result<netcfg::AdapterInfo> {
    let mut last_err = None;
    for _ in 0..30 {
        match netcfg::find_ap_adapter() {
            Ok(adapter) => {
                let ips = netcfg::list_ipv4(adapter.if_index);
                if ips.iter().any(|ip| !ip.is_link_local()) {
                    log::info!("热点网卡 {} 就绪，地址 {:?}", adapter.friendly_name, ips);
                    return Ok(adapter);
                }
            }
            Err(err) => last_err = Some(err),
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    match last_err {
        Some(err) => Err(err).context("等待热点网卡就绪失败"),
        None => anyhow::bail!("等待热点网卡就绪超时"),
    }
}

fn wait_for_ip(if_index: u32, addr: Ipv4Addr, timeout: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if netcfg::is_preferred(if_index, addr) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::bail!("等待地址 {addr} 在接口 {if_index} 上完成 DAD 超时")
}
