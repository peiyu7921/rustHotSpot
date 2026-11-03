//! Windows 网络配置：定位 Wi-Fi Direct 虚拟网卡、设置其 IPv4 地址、配置 NetNat。
//!
//! 注意：Windows 会为 Wi-Fi Direct legacy AP 自动配置 192.168.137.1/24 并启动
//! ICS 的 DHCP。我们不需要（也无法关闭）那个 DHCP，而是把网卡改到自己的网段，
//! 让旧的 192.168.137.1 上残留的监听失效，然后用自建 DHCP 接管固定 IP。

use anyhow::{bail, Context, Result};
use std::ffi::c_void;
use std::net::Ipv4Addr;
use std::os::windows::process::CommandExt;
use std::process::Command;
use windows::core::PWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetAdaptersAddresses, GetUnicastIpAddressTable, GAA_FLAG_INCLUDE_PREFIX,
    IP_ADAPTER_ADDRESSES_LH, MIB_UNICASTIPADDRESS_TABLE,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC};

pub const NAT_NAME: &str = "RustHotspot";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn prefix_to_mask(prefix_len: u8) -> Ipv4Addr {
    let bits = prefix_len.min(32) as u32;
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    Ipv4Addr::from(mask)
}

fn netsh(args: &[&str]) -> Result<()> {
    let output = Command::new("netsh")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("调用 netsh 失败")?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "netsh {} 失败: {}{}",
            args.join(" "),
            stdout.trim(),
            stderr.trim()
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct AdapterInfo {
    pub if_index: u32,
    pub friendly_name: String,
}

fn pwstr_to_string(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { p.to_string().unwrap_or_default() }
}

/// 找到当前处于 Up 状态的「Microsoft Wi-Fi Direct Virtual Adapter」。
pub fn find_ap_adapter() -> Result<AdapterInfo> {
    let mut size: u32 = 16 * 1024;
    let mut buffer: Vec<u64> = Vec::new();
    let mut ok = false;
    for _ in 0..5 {
        let bytes = size as usize;
        // u64 保证 8 字节对齐，避免把 IP_ADAPTER_ADDRESSES_LH 强制对齐到未对齐内存。
        buffer = vec![0u64; (bytes + 7) / 8];
        let ret = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                GAA_FLAG_INCLUDE_PREFIX,
                None,
                Some(buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            )
        };
        if ret == ERROR_SUCCESS.0 {
            ok = true;
            break;
        }
        if ret == u32::from(windows::Win32::Foundation::ERROR_BUFFER_OVERFLOW.0) {
            continue;
        }
        bail!("GetAdaptersAddresses 失败: 错误码 {ret}");
    }
    if !ok {
        bail!("GetAdaptersAddresses 缓冲区不足，无法枚举网卡");
    }

    let mut cur = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    let mut fallback: Option<AdapterInfo> = None;
    while !cur.is_null() {
        let adapter = unsafe { &*cur };
        let description = pwstr_to_string(adapter.Description);
        let friendly = pwstr_to_string(adapter.FriendlyName);
        if description.contains("Wi-Fi Direct") {
            let info = AdapterInfo {
                if_index: unsafe { adapter.Anonymous1.Anonymous.IfIndex },
                friendly_name: friendly,
            };
            if adapter.OperStatus == IfOperStatusUp {
                return Ok(info);
            }
            if fallback.is_none() {
                fallback = Some(info);
            }
        }
        cur = adapter.Next;
    }

    match fallback {
        Some(info) => Ok(info),
        None => bail!("未找到 Wi-Fi Direct 虚拟网卡，请先确认无线网卡驱动支持 Wi-Fi Direct"),
    }
}

/// 给指定接口设置静态 IPv4 地址（会替换该接口上已有的主地址）。
pub fn set_ipv4(if_index: u32, addr: Ipv4Addr, prefix_len: u8) -> Result<()> {
    let index = if_index.to_string();
    let mask = prefix_to_mask(prefix_len).to_string();
    netsh(&[
        "interface", "ipv4", "set", "address", &index, "static", &addr.to_string(), &mask,
    ])
    .with_context(|| format!("为接口 {if_index} 配置 {addr}/{prefix_len} 失败"))
}

pub fn delete_ipv4(if_index: u32, addr: Ipv4Addr) -> Result<()> {
    let index = if_index.to_string();
    netsh(&["interface", "ipv4", "delete", "address", &index, &addr.to_string()])
        .with_context(|| format!("删除接口 {if_index} 地址 {addr} 失败"))
}

/// 地址是否已经完成重复地址检测（Preferred），可以安全绑定 socket。
pub fn is_preferred(if_index: u32, addr: Ipv4Addr) -> bool {
    let mut table: *mut MIB_UNICASTIPADDRESS_TABLE = std::ptr::null_mut();
    let ret = unsafe { GetUnicastIpAddressTable(AF_INET, &mut table) };
    if ret != ERROR_SUCCESS || table.is_null() {
        return false;
    }
    let mut found = false;
    unsafe {
        let table_ref = &*table;
        let entries = std::slice::from_raw_parts(table_ref.Table.as_ptr(), table_ref.NumEntries as usize);
        for entry in entries {
            if entry.InterfaceIndex == if_index && entry.Address.si_family == AF_INET {
                let raw = entry.Address.Ipv4.sin_addr.S_un.S_addr;
                let ip = Ipv4Addr::from(raw.to_ne_bytes());
                if ip == addr && entry.DadState == windows::Win32::Networking::WinSock::IpDadStatePreferred {
                    found = true;
                    break;
                }
            }
        }
        FreeMibTable(table as *const c_void);
    }
    found
}

/// 列出接口上所有 IPv4 地址。
pub fn list_ipv4(if_index: u32) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let mut table: *mut MIB_UNICASTIPADDRESS_TABLE = std::ptr::null_mut();
    let ret = unsafe { GetUnicastIpAddressTable(AF_INET, &mut table) };
    if ret != ERROR_SUCCESS || table.is_null() {
        return out;
    }
    unsafe {
        let table_ref = &*table;
        let entries = std::slice::from_raw_parts(table_ref.Table.as_ptr(), table_ref.NumEntries as usize);
        for entry in entries {
            if entry.InterfaceIndex == if_index && entry.Address.si_family == AF_INET {
                let raw = entry.Address.Ipv4.sin_addr.S_un.S_addr;
                out.push(Ipv4Addr::from(raw.to_ne_bytes()));
            }
        }
        FreeMibTable(table as *const c_void);
    }
    out
}

/// 创建/更新 NetNat，让客户端可以经本机上网。
pub fn ensure_nat(prefix_cidr: &str) -> Result<()> {
    let script = format!(
        "$ErrorActionPreference='Stop'; \
         Get-NetNat -Name '{NAT_NAME}' -ErrorAction SilentlyContinue | Remove-NetNat -Confirm:$false -ErrorAction SilentlyContinue; \
         New-NetNat -Name '{NAT_NAME}' -InternalIPInterfaceAddressPrefix '{prefix_cidr}' | Out-Null"
    );
    crate::util::run_powershell(&script)
        .with_context(|| format!("创建 NetNat（{prefix_cidr}）失败"))?;
    Ok(())
}

pub fn remove_nat() {
    let script = format!(
        "Get-NetNat -Name '{NAT_NAME}' -ErrorAction SilentlyContinue | Remove-NetNat -Confirm:$false -ErrorAction SilentlyContinue"
    );
    let _ = crate::util::run_powershell(&script);
}

#[derive(Debug, Clone)]
pub struct BandBackup {
    pub adapter: String,
    pub display_name: String,
    pub original: String,
}

/// 通过网卡高级属性 "Preferred Band" 控制 Wi-Fi Direct AP 的频段。
///
/// 实测 Intel AX201：设为 2.4G 后 AP 走信道 6，设为 5G 后走信道 36。
/// 返回原值，停止热点时用于恢复。Band::Auto 不改动任何东西。
pub fn apply_band_preference(band: crate::config::Band) -> Result<Option<BandBackup>> {
    let cond = match band {
        crate::config::Band::Auto => return Ok(None),
        crate::config::Band::TwoPointFour => "$_ -match '2[.]4'",
        crate::config::Band::Five => "$_ -match '5' -and $_ -notmatch '2[.]4'",
    };

    // 第一步只做查询：先把适配器名、属性名、原值、目标值拿到手。
    // 设置频段会重置网卡，PowerShell 的输出可能因此丢失，所以不能把查询和设置
    // 放在同一次调用里。
    let query = format!(
        "$ErrorActionPreference='Stop';          $ad = Get-NetAdapter | Where-Object {{ $_.InterfaceDescription -match 'Wi-Fi|Wireless' }} | Select-Object -First 1;          if (-not $ad) {{ throw '未找到无线网卡' }};          $prop = Get-NetAdapterAdvancedProperty -Name $ad.Name | Where-Object {{ $_.DisplayName -match 'Band' -and $_.ValidDisplayValues -match '2[.]4' }} | Select-Object -First 1;          if (-not $prop) {{ throw '网卡不支持 Preferred Band' }};          $target = $prop.ValidDisplayValues | Where-Object {{ {cond} }} | Select-Object -First 1;          if (-not $target) {{ throw '没有目标频段选项' }};          Write-Output ($ad.Name + '|' + $prop.DisplayName + '|' + $prop.DisplayValue + '|' + $target)"
    );
    let info = crate::util::run_powershell(&query).context("读取网卡频段偏好失败")?;
    let line = info.trim().to_string();
    let parts: Vec<&str> = line.split('|').collect();
    if parts.len() < 4 {
        bail!("无法解析频段信息: {line}");
    }
    let backup = BandBackup {
        adapter: parts[0].to_string(),
        display_name: parts[1].to_string(),
        original: parts[2].to_string(),
    };
    let target = parts[3].to_string();

    // 第二步设置。即使命令因为网卡重置返回失败，也把备份交回调用方，
    // 保证停止热点时能恢复原值。
    let script = format!(
        "Set-NetAdapterAdvancedProperty -Name '{}' -DisplayName '{}' -DisplayValue '{}' -ErrorAction Stop | Out-Null;          Start-Sleep -Seconds 6",
        backup.adapter.replace("'", "''"),
        backup.display_name.replace("'", "''"),
        target.replace("'", "''")
    );
    if let Err(err) = crate::util::run_powershell(&script) {
        log::warn!("设置网卡频段失败（仍会尝试恢复原值）: {err}");
    }
    Ok(Some(backup))
}

pub fn restore_band_preference(backup: &BandBackup) {
    // AP 刚停止时网卡可能还在重置，设置会失败，重试几次。
    for attempt in 1..=3 {
        let script = format!(
            "Set-NetAdapterAdvancedProperty -Name '{}' -DisplayName '{}' -DisplayValue '{}' -ErrorAction Stop | Out-Null",
            backup.adapter.replace("'", "''"),
            backup.display_name.replace("'", "''"),
            backup.original.replace("'", "''")
        );
        match crate::util::run_powershell(&script) {
            Ok(_) => return,
            Err(err) => {
                log::warn!("恢复网卡频段偏好失败（第 {attempt} 次）: {err}");
                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        }
    }
}

pub const DHCP_FIREWALL_RULE: &str = "RustHotSpot Block ICS DHCP";

/// 阻止 Windows ICS 的 DHCP 响应。ICS 会为 Wi-Fi Direct AP 自动起一个 DHCP
/// 监听（192.168.137.1:67），和我们抢答，导致客户端拿到 192.168.137.x。
/// 按服务名只挡 SharedAccess 的出站 67 端口，不影响本程序自己的 DHCP。
pub fn ensure_dhcp_firewall() -> Result<()> {
    let script = format!(
        "Remove-NetFirewallRule -DisplayName '{DHCP_FIREWALL_RULE}' -ErrorAction SilentlyContinue;          Remove-NetFirewallRule -DisplayName 'RustHotSpot DHCP In' -ErrorAction SilentlyContinue;          New-NetFirewallRule -DisplayName '{DHCP_FIREWALL_RULE}' -Direction Outbound -Protocol UDP -LocalPort 67 -Action Block -Service SharedAccess -Profile Any -ErrorAction Stop | Out-Null;          New-NetFirewallRule -DisplayName 'RustHotSpot DHCP In' -Direction Inbound -Protocol UDP -LocalPort 67 -Action Allow -Profile Any -ErrorAction Stop | Out-Null"
    );
    crate::util::run_powershell(&script).context("添加 DHCP 防火墙规则失败")?;
    Ok(())
}

pub fn remove_dhcp_firewall() {
    let script = format!(
        "Remove-NetFirewallRule -DisplayName '{DHCP_FIREWALL_RULE}' -ErrorAction SilentlyContinue;          Remove-NetFirewallRule -DisplayName 'RustHotSpot DHCP In' -ErrorAction SilentlyContinue"
    );
    let _ = crate::util::run_powershell(&script);
}
