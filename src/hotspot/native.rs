//! Windows 原生移动热点（NetworkOperatorTetheringManager）。
//!
//! 支持 SSID/密码/频段（2.4G、5G、自动），由系统自动完成 NAT 与 DHCP。
//! 限制：该 API 没有隐藏 SSID 的开关，也不能自定义 DHCP 租约。

use anyhow::{bail, Context, Result};
use windows::core::HSTRING;
use windows::Networking::Connectivity::NetworkInformation;
use windows::Networking::NetworkOperators::{
    NetworkOperatorTetheringAccessPointConfiguration, NetworkOperatorTetheringManager,
    TetheringOperationStatus,
};

use crate::config::Band;

#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub mac: String,
    pub host: String,
}

pub struct NativeHotspot {
    manager: NetworkOperatorTetheringManager,
}

impl NativeHotspot {
    pub fn new() -> Result<Self> {
        let profile = NetworkInformation::GetInternetConnectionProfile()
            .context("没有可用的 Internet 连接，无法创建移动热点")?;
        let manager = NetworkOperatorTetheringManager::CreateFromConnectionProfile(&profile)
            .context("创建移动热点管理器失败")?;
        Ok(Self { manager })
    }

    pub fn start(&self, ssid: &str, passphrase: &str, band: Band) -> Result<()> {
        if ssid.is_empty() || ssid.chars().count() > 32 {
            bail!("SSID 长度必须在 1-32 个字符之间");
        }
        if passphrase.chars().count() < 8 || passphrase.chars().count() > 63 {
            bail!("热点密码长度必须在 8-63 个字符之间");
        }

        let cfg = NetworkOperatorTetheringAccessPointConfiguration::new()?;
        cfg.SetSsid(&HSTRING::from(ssid))?;
        cfg.SetPassphrase(&HSTRING::from(passphrase))?;
        if let Err(err) = cfg.SetBand(super::band_of(band)) {
            log::warn!("当前系统不支持设置热点频段: {err}");
        }

        self.manager
            .ConfigureAccessPointAsync(&cfg)
            .context("配置热点参数失败")?
            .join()
            .context("应用热点参数失败")?;

        let result = self
            .manager
            .StartTetheringAsync()
            .context("启动热点失败")?
            .join()
            .context("等待热点启动失败")?;
        let status = result.Status().unwrap_or(TetheringOperationStatus::Unknown);
        if status != TetheringOperationStatus::Success {
            bail!("热点启动失败: {status:?}（{}）", result.AdditionalErrorMessage().map(|s| s.to_string()).unwrap_or_default());
        }
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        self.manager
            .StopTetheringAsync()
            .context("停止热点失败")?
            .join()
            .context("等待热点停止失败")?;
        Ok(())
    }

    pub fn clients(&self) -> Vec<ClientInfo> {
        let mut out = Vec::new();
        let Ok(list) = self.manager.GetTetheringClients() else {
            return out;
        };
        let size = list.Size().unwrap_or(0);
        for i in 0..size {
            let Ok(client) = list.GetAt(i) else { continue };
            let mac = client
                .MacAddress()
                .map(|s| s.to_string())
                .unwrap_or_default();
            let mut host = String::new();
            if let Ok(names) = client.HostNames() {
                if let Ok(n) = names.Size() {
                    if n > 0 {
                        if let Ok(name) = names.GetAt(0) {
                            host = name.ToString().map(|s| s.to_string()).unwrap_or_default();
                        }
                    }
                }
            }
            out.push(ClientInfo { mac, host });
        }
        out
    }
}
