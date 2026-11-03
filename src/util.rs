use anyhow::{bail, Context, Result};
use std::os::windows::process::CommandExt;
use std::process::Command;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn is_admin() -> bool {
    unsafe { IsUserAnAdmin().as_bool() }
}

/// 以管理员身份重新启动自身。返回值表示是否成功拉起了提权进程。
pub fn relaunch_as_admin() -> Result<()> {
    let exe = std::env::current_exe().context("获取程序路径失败")?;
    let exe_w = HSTRING::from(exe.as_os_str());
    let file = PCWSTR(exe_w.as_ptr());

    // ShellExecuteW 的 lpParameters 需要一条完整的命令行；把当前参数原样带过去，
    // 否则 --headless 之类的参数会在提权重启时丢失。
    let args = std::env::args_os()
        .skip(1)
        .map(|arg| quote_arg(&arg))
        .collect::<Vec<_>>()
        .join(" ");
    let params_w = HSTRING::from(args.clone());
    let params = if args.is_empty() {
        PCWSTR::null()
    } else {
        PCWSTR(params_w.as_ptr())
    };

    let ret = unsafe { ShellExecuteW(None, w!("runas"), file, params, PCWSTR::null(), SW_SHOWNORMAL) };
    // ShellExecuteW 返回值 <= 32 表示失败
    if ret.0 as usize <= 32 {
        bail!("申请管理员权限失败（错误码 {}）", ret.0 as usize);
    }
    Ok(())
}

/// 按 Windows 命令行规则粗略引用一个参数；本项目参数都很简单，但路径可能带空格。
fn quote_arg(arg: &std::ffi::OsStr) -> String {
    let text = arg.to_string_lossy();
    if !text.is_empty() && !text.chars().any(|c| c.is_whitespace() || c == '"') {
        return text.into_owned();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        if ch == '"' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// 静默执行一段 PowerShell。用于 NetNat 等没有稳定 Rust 绑定的系统配置。
pub fn run_powershell(script: &str) -> Result<String> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("启动 PowerShell 失败")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        bail!("PowerShell 执行失败: {}", err.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn parse_mac(text: &str) -> Result<[u8; 6]> {
    let clean: String = text
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase();
    if clean.len() != 12 {
        bail!("MAC 地址格式不正确: {text}");
    }
    let mut mac = [0u8; 6];
    for i in 0..6 {
        mac[i] = u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16)
            .with_context(|| format!("MAC 地址格式不正确: {text}"))?;
    }
    Ok(mac)
}

pub fn format_mac(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_roundtrip() {
        let mac = parse_mac("AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(mac, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert_eq!(format_mac(&mac), "aa:bb:cc:dd:ee:ff");
        assert_eq!(parse_mac("aabb.ccdd.eeff").unwrap(), mac);
    }

    #[test]
    fn mac_rejects_bad_input() {
        assert!(parse_mac("").is_err());
        assert!(parse_mac("zz:zz:zz:zz:zz:zz").is_err());
        assert!(parse_mac("aa:bb:cc:dd:ee").is_err());
    }

    #[test]
    fn quote_admin_args() {
        use std::ffi::OsStr;
        assert_eq!(quote_arg(OsStr::new("--headless")), "--headless");
        assert_eq!(quote_arg(OsStr::new("30")), "30");
        assert_eq!(quote_arg(OsStr::new("a b")), "\"a b\"");
    }
}
