//! Wi-Fi Direct legacy AP 后端。
//!
//! Windows 的「移动热点」API 不提供隐藏 SSID 的开关，但 Wi-Fi Direct 广播
//! 可以自定义 SSID/密码，并把 ListenStateDiscoverability 设为 None，使热点
//! 不响应探测请求、不出现在扫描列表中（客户端需手动输入 SSID 连接）。

use anyhow::{bail, Result};
use windows::core::HSTRING;
use windows::Devices::WiFiDirect::{
    WiFiDirectAdvertisementListenStateDiscoverability, WiFiDirectAdvertisementPublisher,
    WiFiDirectAdvertisementPublisherStatus,
};
use windows::Security::Credentials::PasswordCredential;

pub struct WifiDirectAp {
    publisher: Option<WiFiDirectAdvertisementPublisher>,
}

impl WifiDirectAp {
    pub fn start(ssid: &str, passphrase: &str, hidden: bool) -> Result<Self> {
        let ssid_len = ssid.chars().count();
        if ssid.is_empty() || ssid_len > 32 {
            bail!("SSID 长度必须在 1-32 个字符之间");
        }
        let pass_len = passphrase.chars().count();
        if pass_len < 8 || pass_len > 63 {
            bail!("热点密码长度必须在 8-63 个字符之间");
        }

        let publisher = WiFiDirectAdvertisementPublisher::new()?;
        let advertisement = publisher.Advertisement()?;
        advertisement.SetIsAutonomousGroupOwnerEnabled(true)?;

        let legacy = advertisement.LegacySettings()?;
        legacy.SetIsEnabled(true)?;
        legacy.SetSsid(&HSTRING::from(ssid))?;
        let credential = PasswordCredential::new()?;
        credential.SetPassword(&HSTRING::from(passphrase))?;
        legacy.SetPassphrase(&credential)?;

        advertisement.SetListenStateDiscoverability(if hidden {
            WiFiDirectAdvertisementListenStateDiscoverability::None
        } else {
            WiFiDirectAdvertisementListenStateDiscoverability::Normal
        })?;

        publisher.Start()?;
        // 给驱动一点时间完成 AP 初始化
        std::thread::sleep(std::time::Duration::from_millis(1500));

        let status = publisher.Status()?;
        if status == WiFiDirectAdvertisementPublisherStatus::Aborted {
            bail!("Wi-Fi Direct 热点启动失败（状态 Aborted）。请确认无线网卡支持 Wi-Fi Direct，且没有被其他热点占用。");
        }
        log::info!("Wi-Fi Direct AP 已启动，状态 {status:?}, SSID={ssid}, hidden={hidden}");

        Ok(Self {
            publisher: Some(publisher),
        })
    }

    pub fn stop(&mut self) {
        if let Some(publisher) = self.publisher.take() {
            if let Err(err) = publisher.Stop() {
                log::warn!("停止 Wi-Fi Direct AP 失败: {err}");
            }
        }
    }
}

impl Drop for WifiDirectAp {
    fn drop(&mut self) {
        self.stop();
    }
}
