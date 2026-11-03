use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::PathBuf;

/// 热点后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Mode {
    /// 隐藏模式：Wi-Fi Direct legacy AP，可隐藏 SSID、可绑定固定 IP。
    #[default]
    Hidden,
    /// 标准模式：Windows 原生移动热点，可手动选择 2.4G/5G。
    Standard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Band {
    #[default]
    Auto,
    TwoPointFour,
    Five,
}

impl Band {
    pub fn label(self) -> &'static str {
        match self {
            Band::Auto => "自动",
            Band::TwoPointFour => "2.4 GHz",
            Band::Five => "5 GHz",
        }
    }
}

/// 一条 MAC -> IP 的静态绑定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacBinding {
    pub mac: String,
    pub ip: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub mode: Mode,
    pub ssid: String,
    pub passphrase: String,
    pub band: Band,
    /// 仅隐藏模式生效。
    #[serde(default = "default_true")]
    pub hidden: bool,
    /// 隐藏模式下由本程序接管的网段前三段，例如 "10.10.10"。
    #[serde(default = "default_subnet")]
    pub subnet: String,
    #[serde(default = "default_prefix")]
    pub prefix_len: u8,
    /// 下发给客户端的 DNS，留空则使用系统 DNS 推导。
    #[serde(default)]
    pub dns: Vec<String>,
    /// 界面主题：true 为深色。
    #[serde(default = "default_true")]
    pub dark: bool,
    #[serde(default)]
    pub bindings: Vec<MacBinding>,
}

fn default_true() -> bool {
    true
}

fn default_subnet() -> String {
    "10.10.10".to_string()
}

fn default_prefix() -> u8 {
    24
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Hidden,
            ssid: "RustHotspot".to_string(),
            passphrase: "12345678".to_string(),
            band: Band::Auto,
            hidden: true,
            subnet: default_subnet(),
            prefix_len: 24,
            dns: vec!["223.5.5.5".to_string(), "119.29.29.29".to_string()],
            dark: true,
            bindings: Vec::new(),
        }
    }
}

impl Config {
    pub fn dir() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("RustHotSpot")
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
                log::warn!("配置文件解析失败，使用默认值: {err}");
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建配置目录失败: {}", dir.display()))?;
        let text = serde_json::to_string_pretty(self)?;
        let path = Self::path();
        let tmp = dir.join("config.json.tmp");
        std::fs::write(&tmp, &text)
            .with_context(|| format!("写入临时配置失败: {}", tmp.display()))?;
        if let Err(err) = std::fs::rename(&tmp, &path) {
            // 极少数情况下（例如目标被占用）rename 会失败，退回直接覆盖。
            log::warn!("原子替换配置失败，改为直接写入: {err}");
            std::fs::write(&path, &text).context("写入配置文件失败")?;
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(())
    }

    /// 网关地址。支持 "10.10.10"、"10.10.10.1"、"10.10.10.0/24" 等写法。
    pub fn gateway(&self) -> Result<Ipv4Addr> {
        Ok(parse_subnet(&self.subnet)?.0)
    }

    /// 隐藏模式使用的网关和前缀。当前 DHCP 地址池按 /24 设计，因此只接受 /24。
    pub fn hidden_network(&self) -> Result<(Ipv4Addr, u8)> {
        let (gateway, prefix) = parse_subnet(&self.subnet)?;
        if prefix != 24 {
            bail!("隐藏模式目前只支持 /24 网段（当前为 /{prefix}）");
        }
        Ok((gateway, prefix))
    }

    /// 地址是否属于隐藏模式热点网段，并排除网络地址、网关和广播地址。
    pub fn is_hotspot_ip(&self, ip: Ipv4Addr) -> bool {
        let Ok((gateway, _)) = self.hidden_network() else {
            return false;
        };
        let g = gateway.octets();
        let o = ip.octets();
        o[0] == g[0]
            && o[1] == g[1]
            && o[2] == g[2]
            && !matches!(o[3], 0 | 1 | 255)
    }

    /// 校验绑定表并转换成 DHCP 需要的 MAC -> IP 映射。
    ///
    /// 这样 UI、工作线程和 DHCP 服务共享同一套校验：MAC 合法、IP 在热点网段内、
    /// 不使用网关/网络/广播地址，并且 MAC 和 IP 都不会重复。
    pub fn validated_bindings(&self) -> Result<HashMap<[u8; 6], Ipv4Addr>> {
        let (gateway, _) = self.hidden_network()?;
        let g = gateway.octets();
        let mut map = HashMap::new();
        let mut used_ips = HashSet::new();
        for binding in &self.bindings {
            let mac = crate::util::parse_mac(&binding.mac)
                .with_context(|| format!("绑定项 MAC 非法: {}", binding.mac))?;
            let ip: Ipv4Addr = binding
                .ip
                .trim()
                .parse()
                .with_context(|| format!("绑定项 IP 非法: {}", binding.ip))?;
            let o = ip.octets();
            if o[0] != g[0] || o[1] != g[1] || o[2] != g[2] || matches!(o[3], 0 | 1 | 255) {
                bail!("绑定 IP {ip} 不在热点网段内，或使用了保留地址");
            }
            if map.insert(mac, ip).is_some() {
                bail!("MAC 地址重复: {}", binding.mac);
            }
            if !used_ips.insert(ip) {
                bail!("IP {ip} 已被其他绑定使用");
            }
        }
        Ok(map)
    }

    /// 规范化后的网段前三段，例如 "10.10.10"。
    pub fn normalized_subnet(&self) -> String {
        match parse_subnet(&self.subnet) {
            Ok((gateway, _)) => {
                let o = gateway.octets();
                format!("{}.{}.{}", o[0], o[1], o[2])
            }
            Err(_) => self.subnet.trim().to_string(),
        }
    }

    pub fn dns_list(&self) -> Vec<Ipv4Addr> {
        let mut out = Vec::new();
        for d in &self.dns {
            if let Ok(ip) = d.trim().parse::<Ipv4Addr>() {
                out.push(ip);
            }
        }
        if out.is_empty() {
            out.push(Ipv4Addr::new(223, 5, 5, 5));
            out.push(Ipv4Addr::new(119, 29, 29, 29));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_subnet_variants() {
        let expect = (Ipv4Addr::new(192, 168, 123, 1), 24);
        assert_eq!(parse_subnet("192.168.123").unwrap(), expect);
        assert_eq!(parse_subnet("192.168.123.1").unwrap(), expect);
        assert_eq!(parse_subnet("192.168.123.0").unwrap(), expect);
        assert_eq!(parse_subnet(" 192.168.123.0/24 ").unwrap(), expect);
        assert_eq!(parse_subnet("10.10.10.0/16").unwrap().1, 16);
        assert!(parse_subnet("192.168").is_err());
        assert!(parse_subnet("abc").is_err());
        assert!(parse_subnet("").is_err());
    }

    #[test]
    fn gateway_and_dns() {
        let mut cfg = Config::default();
        cfg.subnet = "192.168.50".into();
        assert_eq!(cfg.gateway().unwrap(), Ipv4Addr::new(192, 168, 50, 1));
        cfg.dns = vec!["1.1.1.1".into(), "bad".into(), "8.8.8.8".into()];
        assert_eq!(
            cfg.dns_list(),
            vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)]
        );
    }

    #[test]
    fn hidden_network_requires_24() {
        let mut cfg = Config::default();
        assert!(cfg.hidden_network().is_ok());
        cfg.subnet = "10.10.0.0/16".into();
        assert!(cfg.hidden_network().is_err());
    }

    #[test]
    fn bindings_are_validated() {
        let mut cfg = Config::default();
        cfg.bindings = vec![MacBinding {
            mac: "AA:BB:CC:DD:EE:01".into(),
            ip: "10.10.10.50".into(),
            name: "tv".into(),
        }];
        let map = cfg.validated_bindings().unwrap();
        assert_eq!(
            map.get(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x01]),
            Some(&Ipv4Addr::new(10, 10, 10, 50))
        );

        // 同一个 IP 不能绑定给两个 MAC
        cfg.bindings.push(MacBinding {
            mac: "AA:BB:CC:DD:EE:02".into(),
            ip: "10.10.10.50".into(),
            name: String::new(),
        });
        assert!(cfg.validated_bindings().is_err());

        // 不能跨网段
        cfg.bindings = vec![MacBinding {
            mac: "AA:BB:CC:DD:EE:03".into(),
            ip: "192.168.1.50".into(),
            name: String::new(),
        }];
        assert!(cfg.validated_bindings().is_err());

        // 不能占用网关
        cfg.bindings = vec![MacBinding {
            mac: "AA:BB:CC:DD:EE:04".into(),
            ip: "10.10.10.1".into(),
            name: String::new(),
        }];
        assert!(cfg.validated_bindings().is_err());
    }
}

/// 解析用户输入的网段，返回 (网关地址, 前缀长度)。
/// 支持 "10.10.10"、"10.10.10.1"、"10.10.10.0/24" 三种写法。
pub fn parse_subnet(input: &str) -> Result<(Ipv4Addr, u8)> {
    let raw = input.trim();
    if raw.is_empty() {
        bail!("网段不能为空");
    }
    let (addr_part, prefix) = match raw.split_once('/') {
        Some((addr, prefix)) => {
            let prefix: u8 = prefix
                .trim()
                .parse()
                .with_context(|| format!("前缀长度非法: {prefix}"))?;
            if prefix > 32 {
                bail!("前缀长度必须在 0-32 之间");
            }
            (addr.trim(), Some(prefix))
        }
        None => (raw, None),
    };
    let octets: Vec<u8> = addr_part
        .split('.')
        .map(|part| part.trim().parse::<u8>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("网段格式不正确: {input}"))?;
    if octets.len() < 3 || octets.len() > 4 {
        bail!("网段需要 3 或 4 段，例如 10.10.10 或 192.168.123.1");
    }
    let a = octets[0];
    if a == 0 || a >= 224 {
        bail!("网段首段非法: {a}");
    }
    Ok((
        Ipv4Addr::new(a, octets[1], octets[2], 1),
        prefix.unwrap_or(24),
    ))
}
