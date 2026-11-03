//! 极简 DHCP 服务器。
//!
//! 只实现热点场景需要的部分：DISCOVER / REQUEST / RELEASE / DECLINE / INFORM，
//! 支持按 MAC 绑定固定 IP，其余客户端从地址池动态分配。
//!
//! Windows 会为 Wi-Fi Direct AP 自动保留 192.168.137.1 并让 ICS 在该地址上监听
//! 67 端口。我们把网卡改到自己的网段后，那个残留监听收不到本网段的广播，
//! 因此这里用 0.0.0.0:67（SO_REUSEADDR）接收广播，并用 IP_PKTINFO 只处理
//! 来自热点网卡的报文。

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::windows::io::AsRawSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const BOOTREQUEST: u8 = 1;
const BOOTREPLY: u8 = 2;

pub const DHCPDISCOVER: u8 = 1;
pub const DHCPOFFER: u8 = 2;
pub const DHCPREQUEST: u8 = 3;
pub const DHCPDECLINE: u8 = 4;
pub const DHCPACK: u8 = 5;
pub const DHCPNAK: u8 = 6;
pub const DHCPRELEASE: u8 = 7;
pub const DHCPINFORM: u8 = 8;

#[derive(Debug, Clone)]
pub struct Lease {
    pub mac: [u8; 6],
    pub ip: Ipv4Addr,
    pub hostname: String,
    pub expires: Instant,
    pub static_binding: bool,
}

#[derive(Debug, Clone)]
pub struct DhcpSettings {
    pub gateway: Ipv4Addr,
    pub prefix_len: u8,
    pub dns: Vec<Ipv4Addr>,
    pub lease_secs: u32,
    pub pool_start: u8,
    pub pool_end: u8,
    pub bindings: HashMap<[u8; 6], Ipv4Addr>,
    pub if_index: u32,
}

impl DhcpSettings {
    pub fn subnet_mask(&self) -> Ipv4Addr {
        let bits = self.prefix_len.min(32) as u32;
        let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
        Ipv4Addr::from(mask)
    }

    pub fn broadcast(&self) -> Ipv4Addr {
        let ip = u32::from(self.gateway);
        let mask = u32::from(self.subnet_mask());
        Ipv4Addr::from(ip | !mask)
    }

    fn pool_ip(&self, last: u8) -> Ipv4Addr {
        let o = self.gateway.octets();
        Ipv4Addr::new(o[0], o[1], o[2], last)
    }

    /// 网段地址（例如 10.10.10.0）。
    pub fn network(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.gateway) & u32::from(self.subnet_mask()))
    }

    /// 地址是否落在当前子网内。
    pub fn in_subnet(&self, ip: Ipv4Addr) -> bool {
        let mask = u32::from(self.subnet_mask());
        (u32::from(ip) & mask) == (u32::from(self.gateway) & mask)
    }

    /// 是否可以作为客户端地址：在子网内，且不是网络地址、网关或广播地址。
    pub fn is_assignable(&self, ip: Ipv4Addr) -> bool {
        self.in_subnet(ip) && ip != self.network() && ip != self.gateway && ip != self.broadcast()
    }

    /// 是否属于动态地址池。
    pub fn in_pool(&self, ip: Ipv4Addr) -> bool {
        let o = ip.octets();
        let g = self.gateway.octets();
        o[0] == g[0]
            && o[1] == g[1]
            && o[2] == g[2]
            && (self.pool_start..=self.pool_end).contains(&o[3])
            && self.is_assignable(ip)
    }

    /// 启动 DHCP 服务前的自检，避免把非法绑定或地址池带进网络配置。
    pub fn validate(&self) -> Result<()> {
        if self.prefix_len != 24 {
            anyhow::bail!("DHCP 目前只支持 /24 网段");
        }
        if !self.in_subnet(self.gateway) || self.gateway == self.network() {
            anyhow::bail!("网关地址不在热点网段内");
        }
        if self.pool_start < 2 || self.pool_end > 254 || self.pool_start > self.pool_end {
            anyhow::bail!("DHCP 地址池范围非法");
        }
        let mut seen = HashSet::new();
        for (mac, ip) in &self.bindings {
            if !self.is_assignable(*ip) {
                anyhow::bail!(
                    "固定绑定 {} 的地址 {ip} 不在热点网段内",
                    crate::util::format_mac(mac)
                );
            }
            if !seen.insert(*ip) {
                anyhow::bail!("固定绑定 IP 重复: {ip}");
            }
        }
        Ok(())
    }
}

pub struct DhcpServer {
    stop: Arc<AtomicBool>,
    leases: Arc<Mutex<HashMap<[u8; 6], Lease>>>,
    bindings: Arc<Mutex<HashMap<[u8; 6], Ipv4Addr>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl DhcpServer {
    pub fn leases_shared(&self) -> Arc<Mutex<HashMap<[u8; 6], Lease>>> {
        self.leases.clone()
    }

    /// 运行中更新 MAC -> 固定 IP 绑定。
    pub fn set_bindings(&self, map: HashMap<[u8; 6], Ipv4Addr>) {
        // 新的固定 IP 可能和现有动态租约冲突：先清掉冲突/已改绑的租约，
        // 避免同一个地址同时出现在两个活跃租约里。
        let owners: HashMap<Ipv4Addr, [u8; 6]> =
            map.iter().map(|(mac, ip)| (*ip, *mac)).collect();
        if let Ok(mut leases) = self.leases.lock() {
            leases.retain(|mac, lease| {
                let binding_matches = map.get(mac).map_or(true, |ip| *ip == lease.ip);
                let ip_free = owners.get(&lease.ip).map_or(true, |owner| owner == mac);
                binding_matches && ip_free
            });
            for (mac, lease) in leases.iter_mut() {
                lease.static_binding = map.get(mac) == Some(&lease.ip);
            }
        }
        if let Ok(mut guard) = self.bindings.lock() {
            *guard = map;
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for DhcpServer {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start(mut settings: DhcpSettings) -> Result<DhcpServer> {
    settings.validate()?;
    let stop = Arc::new(AtomicBool::new(false));
    let leases: Arc<Mutex<HashMap<[u8; 6], Lease>>> = Arc::new(Mutex::new(HashMap::new()));
    let bindings = Arc::new(Mutex::new(std::mem::take(&mut settings.bindings)));

    // 接收 socket：0.0.0.0:67，允许端口复用，并打开 IP_PKTINFO
    let recv_sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("创建 DHCP 接收 socket 失败")?;
    recv_sock.set_reuse_address(true)?;
    recv_sock.set_broadcast(true)?;
    recv_sock
        .bind(&SocketAddr::from(([0, 0, 0, 0], 67)).into())
        .context("绑定 UDP 67 失败（端口可能被占用）")?;
    let recv_sock: UdpSocket = recv_sock.into();
    // 读超时让接收线程能周期性检查停止标志
    recv_sock
        .set_read_timeout(Some(Duration::from_millis(500)))
        .context("设置 DHCP 接收超时失败")?;

    // 打开 IP_PKTINFO，以便拿到接收报文的接口索引
    let one: i32 = 1;
    unsafe {
        let ret = windows::Win32::Networking::WinSock::setsockopt(
            windows::Win32::Networking::WinSock::SOCKET(recv_sock.as_raw_socket() as usize),
            0, // IPPROTO_IP
            windows::Win32::Networking::WinSock::IP_PKTINFO,
            Some(&one.to_ne_bytes()[..]),
        );
        if ret != 0 {
            log::warn!("设置 IP_PKTINFO 失败，将不做接口过滤: {}", io::Error::last_os_error());
        }
    }

    // 发送 socket 必须绑定在网关地址的 67 端口：DHCP 响应只能从 67 端口发出，
    // 随机源端口会被客户端直接丢弃。
    let send_sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("创建 DHCP 发送 socket 失败")?;
    send_sock.set_reuse_address(true)?;
    send_sock.set_broadcast(true)?;
    send_sock
        .bind(&SocketAddrV4::new(settings.gateway, 67).into())
        .with_context(|| format!("绑定发送地址 {}:67 失败", settings.gateway))?;
    let send_sock: UdpSocket = send_sock.into();

    let settings = Arc::new(settings);
    let stop2 = stop.clone();
    let leases2 = leases.clone();
    let bindings2 = bindings.clone();
    let handle = thread::Builder::new()
        .name("dhcp-server".into())
        .spawn(move || {
            run(recv_sock, send_sock, settings, stop2, leases2, bindings2);
        })
        .context("启动 DHCP 线程失败")?;

    Ok(DhcpServer {
        stop,
        leases,
        bindings,
        handle: Some(handle),
    })
}

fn run(
    recv_sock: UdpSocket,
    send_sock: UdpSocket,
    settings: Arc<DhcpSettings>,
    stop: Arc<AtomicBool>,
    leases: Arc<Mutex<HashMap<[u8; 6], Lease>>>,
    bindings: Arc<Mutex<HashMap<[u8; 6], Ipv4Addr>>>,
) {
    let mut buf = [0u8; 1500];
    while !stop.load(Ordering::SeqCst) {
        match raw::recv_packet(&recv_sock, &mut buf) {
            Ok((len, if_index)) => {
                if if_index != 0 && if_index != settings.if_index {
                    continue;
                }
                if let Some(req) = Request::parse(&buf[..len]) {
                    log::info!(
                        "DHCP 收到 type={} mac={} if={}",
                        msg_type_name(req.msg_type),
                        crate::util::format_mac(&req.chaddr),
                        if_index
                    );
                    if let Some(resp) = handle_request(&req, &settings, &leases, &bindings) {
                        if let Err(err) = raw::send_to_broadcast(
                            &send_sock,
                            &resp,
                            settings.gateway,
                            settings.if_index,
                            68,
                        ) {
                            log::warn!("发送 DHCP 响应失败: {err}");
                        }
                    }
                }
            }
            Err(err) => {
                if err.kind() == io::ErrorKind::WouldBlock || err.kind() == io::ErrorKind::TimedOut {
                    continue;
                }
                log::warn!("DHCP 接收失败: {err}");
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn msg_type_name(kind: u8) -> &'static str {
    match kind {
        DHCPDISCOVER => "DISCOVER",
        DHCPOFFER => "OFFER",
        DHCPREQUEST => "REQUEST",
        DHCPDECLINE => "DECLINE",
        DHCPACK => "ACK",
        DHCPNAK => "NAK",
        DHCPRELEASE => "RELEASE",
        DHCPINFORM => "INFORM",
        _ => "?",
    }
}

#[derive(Debug)]
struct Request {
    xid: [u8; 4],
    flags: u16,
    ciaddr: Ipv4Addr,
    chaddr: [u8; 6],
    msg_type: u8,
    requested_ip: Option<Ipv4Addr>,
    hostname: Option<String>,
}

impl Request {
    fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 240 || buf[0] != BOOTREQUEST || buf[2] != 6 {
            return None;
        }
        if buf[236..240] != MAGIC {
            return None;
        }
        let mut xid = [0u8; 4];
        xid.copy_from_slice(&buf[4..8]);
        let flags = u16::from_be_bytes([buf[10], buf[11]]);
        let ciaddr = Ipv4Addr::new(buf[12], buf[13], buf[14], buf[15]);
        let mut chaddr = [0u8; 6];
        chaddr.copy_from_slice(&buf[28..34]);

        let mut msg_type = 0;
        let mut requested_ip = None;
        let mut hostname = None;

        let mut i = 240;
        while i < buf.len() {
            let code = buf[i];
            if code == 255 {
                break;
            }
            if code == 0 {
                i += 1;
                continue;
            }
            if i + 1 >= buf.len() {
                break;
            }
            let len = buf[i + 1] as usize;
            if i + 2 + len > buf.len() {
                break;
            }
            let data = &buf[i + 2..i + 2 + len];
            match code {
                53 if !data.is_empty() => msg_type = data[0],
                50 if data.len() == 4 => {
                    requested_ip = Some(Ipv4Addr::new(data[0], data[1], data[2], data[3]))
                }
                12 => hostname = String::from_utf8(data.to_vec()).ok(),
                _ => {}
            }
            i += 2 + len;
        }

        if msg_type == 0 {
            return None;
        }
        Some(Self {
            xid,
            flags,
            ciaddr,
            chaddr,
            msg_type,
            requested_ip,
            hostname,
        })
    }
}

fn handle_request(
    req: &Request,
    settings: &DhcpSettings,
    leases: &Arc<Mutex<HashMap<[u8; 6], Lease>>>,
    bindings: &Arc<Mutex<HashMap<[u8; 6], Ipv4Addr>>>,
) -> Option<Vec<u8>> {
    let mut table = leases.lock().ok()?;
    let bindings = bindings.lock().ok()?;
    let now = Instant::now();

    match req.msg_type {
        DHCPDISCOVER => {
            let ip = allocate(req, settings, &mut table, now, &bindings)?;
            Some(build_reply(req, settings, DHCPOFFER, ip))
        }
        DHCPREQUEST => {
            let assigned = allocate(req, settings, &mut table, now, &bindings);
            let use_ip = if let Some(binding) = bindings.get(&req.chaddr).copied() {
                if !settings.is_assignable(binding) {
                    return Some(build_reply(req, settings, DHCPNAK, Ipv4Addr::UNSPECIFIED));
                }
                // 固定绑定必须拿到绑定的地址；请求别的地址直接 NAK。
                if req.requested_ip.is_some_and(|requested| requested != binding) {
                    return Some(build_reply(req, settings, DHCPNAK, Ipv4Addr::UNSPECIFIED));
                }
                binding
            } else if let Some(requested) = req.requested_ip {
                if is_usable(requested, req, settings, &table, &bindings, now) {
                    requested
                } else {
                    return Some(build_reply(req, settings, DHCPNAK, Ipv4Addr::UNSPECIFIED));
                }
            } else if let Some(assigned) = assigned {
                assigned
            } else {
                return Some(build_reply(req, settings, DHCPNAK, Ipv4Addr::UNSPECIFIED));
            };

            // 一个地址只能有一个活跃租约；固定绑定优先。
            table.retain(|mac, lease| *mac == req.chaddr || lease.ip != use_ip);
            table.insert(
                req.chaddr,
                Lease {
                    mac: req.chaddr,
                    ip: use_ip,
                    hostname: req.hostname.clone().unwrap_or_default(),
                    expires: now + Duration::from_secs(settings.lease_secs as u64),
                    static_binding: bindings.contains_key(&req.chaddr),
                },
            );
            Some(build_reply(req, settings, DHCPACK, use_ip))
        }
        DHCPRELEASE | DHCPDECLINE => {
            table.remove(&req.chaddr);
            None
        }
        DHCPINFORM => {
            if req.ciaddr.is_unspecified() || !settings.in_subnet(req.ciaddr) {
                return None;
            }
            Some(build_reply(req, settings, DHCPACK, req.ciaddr))
        }
        _ => None,
    }
}

fn allocate(
    req: &Request,
    settings: &DhcpSettings,
    table: &mut HashMap<[u8; 6], Lease>,
    now: Instant,
    bindings: &HashMap<[u8; 6], Ipv4Addr>,
) -> Option<Ipv4Addr> {
    if let Some(ip) = bindings.get(&req.chaddr) {
        // 绑定地址必须合法；异常配置下宁可不发 offer，也不把客户端带到错误的网段。
        return settings.is_assignable(*ip).then_some(*ip);
    }
    if let Some(lease) = table.get(&req.chaddr) {
        if lease.expires > now {
            return Some(lease.ip);
        }
    }

    // 过期租约不应继续占着地址池。
    table.retain(|_, lease| lease.expires > now);
    let used: HashSet<Ipv4Addr> = table.values().map(|lease| lease.ip).collect();
    let start = settings.pool_start.max(2).min(settings.pool_end);
    for last in start..=settings.pool_end {
        let candidate = settings.pool_ip(last);
        if settings.is_assignable(candidate)
            && !used.contains(&candidate)
            && !bindings.values().any(|binding| *binding == candidate)
        {
            return Some(candidate);
        }
    }
    None
}

fn is_usable(
    ip: Ipv4Addr,
    req: &Request,
    settings: &DhcpSettings,
    table: &HashMap<[u8; 6], Lease>,
    bindings: &HashMap<[u8; 6], Ipv4Addr>,
    now: Instant,
) -> bool {
    if !settings.is_assignable(ip) {
        return false;
    }
    let bound_to_client = bindings.get(&req.chaddr) == Some(&ip);
    if !bound_to_client && !settings.in_pool(ip) {
        return false;
    }
    if bindings.values().any(|binding| *binding == ip) && !bound_to_client {
        return false;
    }
    !table
        .iter()
        .any(|(mac, lease)| *mac != req.chaddr && lease.ip == ip && lease.expires > now)
}

fn build_reply(req: &Request, settings: &DhcpSettings, msg_type: u8, yiaddr: Ipv4Addr) -> Vec<u8> {
    let mut out = vec![0u8; 240];
    out[0] = BOOTREPLY;
    out[1] = 1;
    out[2] = 6;
    out[3] = 0;
    out[4..8].copy_from_slice(&req.xid);
    out[10..12].copy_from_slice(&req.flags.to_be_bytes());
    out[16..20].copy_from_slice(&yiaddr.octets());
    out[20..24].copy_from_slice(&settings.gateway.octets());
    out[28..34].copy_from_slice(&req.chaddr);
    out[236..240].copy_from_slice(&MAGIC);

    push_opt(&mut out, 53, &[msg_type]);
    push_opt(&mut out, 54, &settings.gateway.octets());

    if msg_type != DHCPNAK {
        push_opt(&mut out, 51, &settings.lease_secs.to_be_bytes());
        push_opt(&mut out, 1, &settings.subnet_mask().octets());
        push_opt(&mut out, 3, &settings.gateway.octets());
        if !settings.dns.is_empty() {
            let mut dns = Vec::with_capacity(settings.dns.len() * 4);
            for d in &settings.dns {
                dns.extend_from_slice(&d.octets());
            }
            push_opt(&mut out, 6, &dns);
        }
        if msg_type != DHCPINFORM {
            push_opt(&mut out, 28, &settings.broadcast().octets());
            let t1 = settings.lease_secs / 2;
            let t2 = settings.lease_secs * 7 / 8;
            push_opt(&mut out, 58, &t1.to_be_bytes());
            push_opt(&mut out, 59, &t2.to_be_bytes());
        }
    }
    out.push(255);
    out
}

fn push_opt(out: &mut Vec<u8>, code: u8, data: &[u8]) {
    if out.len() + 2 + data.len() > 576 {
        return;
    }
    out.push(code);
    out.push(data.len() as u8);
    out.extend_from_slice(data);
}

/// Windows 的 recvmsg 封装。std 不提供带控制信息的接收，这里直接调用 WSARecvMsg。
mod raw {
    use super::*;

    #[repr(C)]
    struct WsaBuf {
        len: u32,
        buf: *mut u8,
    }

    #[repr(C)]
    struct WsaMsg {
        name: *mut c_void,
        namelen: i32,
        buffers: *mut WsaBuf,
        buffer_count: u32,
        control: WsaBuf,
        flags: u32,
    }

    #[repr(C)]
    struct WsaCmsgHdr {
        cmsg_len: usize,
        cmsg_level: i32,
        cmsg_type: i32,
    }

    #[repr(C)]
    struct InPktInfo {
        ipi_addr: [u8; 4],
        ipi_ifindex: u32,
    }

    type WsaRecvMsgFn = unsafe extern "system" fn(
        s: usize,
        msg: *mut WsaMsg,
        bytes: *mut u32,
        overlapped: *mut c_void,
        completion: *mut c_void,
    ) -> i32;

    /// WSARecvMsg 不是 ws2_32.dll 的直接导出，需要经 WSAIoctl 取扩展函数指针。
    fn wsa_recv_msg(sock: &UdpSocket) -> Option<WsaRecvMsgFn> {
        use std::sync::OnceLock;
        use windows::core::GUID;
        use windows::Win32::Networking::WinSock::{
            SIO_GET_EXTENSION_FUNCTION_POINTER, WSAIoctl,
        };
        static FN: OnceLock<Option<WsaRecvMsgFn>> = OnceLock::new();
        *FN.get_or_init(|| unsafe {
            // WSAID_WSARECVMSG
            let guid = GUID::from_u128(0xf689_d7c8_6f1f_436b_8a53_e54fe351c322);
            let mut func: *mut c_void = std::ptr::null_mut();
            let mut bytes = 0u32;
            let ret = WSAIoctl(
                windows::Win32::Networking::WinSock::SOCKET(sock.as_raw_socket() as usize),
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                Some(&guid as *const _ as *const c_void),
                std::mem::size_of::<GUID>() as u32,
                Some(&mut func as *mut _ as *mut c_void),
                std::mem::size_of::<*mut c_void>() as u32,
                &mut bytes,
                None,
                None,
            );
            if ret != 0 || func.is_null() {
                log::warn!(
                    "WSAIoctl 获取 WSARecvMsg 失败: {}",
                    io::Error::last_os_error()
                );
                return None;
            }
            Some(std::mem::transmute::<*mut c_void, WsaRecvMsgFn>(func))
        })
    }

    type WsaSendMsgFn = unsafe extern "system" fn(
        s: usize,
        msg: *const WsaMsg,
        flags: u32,
        bytes: *mut u32,
        overlapped: *mut c_void,
        completion: *mut c_void,
    ) -> i32;

    fn wsa_send_msg(sock: &UdpSocket) -> Option<WsaSendMsgFn> {
        use std::sync::OnceLock;
        use windows::core::GUID;
        use windows::Win32::Networking::WinSock::{
            SIO_GET_EXTENSION_FUNCTION_POINTER, WSAIoctl,
        };
        static FN: OnceLock<Option<WsaSendMsgFn>> = OnceLock::new();
        *FN.get_or_init(|| unsafe {
            // WSAID_WSASENDMSG
            let guid = GUID::from_u128(0xa441_e712_754f_43ca_84a7_0dee44cf606d);
            let mut func: *mut c_void = std::ptr::null_mut();
            let mut bytes = 0u32;
            let ret = WSAIoctl(
                windows::Win32::Networking::WinSock::SOCKET(sock.as_raw_socket() as usize),
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                Some(&guid as *const _ as *const c_void),
                std::mem::size_of::<GUID>() as u32,
                Some(&mut func as *mut _ as *mut c_void),
                std::mem::size_of::<*mut c_void>() as u32,
                &mut bytes,
                None,
                None,
            );
            if ret != 0 || func.is_null() {
                log::warn!("WSAIoctl 获取 WSASendMsg 失败: {}", io::Error::last_os_error());
                return None;
            }
            Some(std::mem::transmute::<*mut c_void, WsaSendMsgFn>(func))
        })
    }

    /// 从热点网卡广播 DHCP 响应。普通 send_to 会按默认路由走 WLAN 网卡，
    /// 手机在 AP 网段上根本收不到，所以这里用 IP_PKTINFO 指定出接口。
    pub fn send_to_broadcast(
        sock: &UdpSocket,
        data: &[u8],
        gateway: Ipv4Addr,
        if_index: u32,
        client_port: u16,
    ) -> io::Result<()> {
        let Some(send_msg) = wsa_send_msg(sock) else {
            return sock
                .send_to(data, SocketAddr::from(([255, 255, 255, 255], client_port)))
                .map(|_| ());
        };

        #[repr(C)]
        struct SockAddrIn {
            family: u16,
            port: u16,
            addr: [u8; 4],
            zero: [u8; 8],
        }
        let name = SockAddrIn {
            family: 2u16, // AF_INET
            port: client_port.to_be(),
            addr: [255, 255, 255, 255],
            zero: [0u8; 8],
        };

        let mut wsa_buf = WsaBuf {
            len: data.len() as u32,
            buf: data.as_ptr() as *mut u8,
        };

        // u64 数组保证 8 字节对齐，否则解引用 WSACMSGHDR 会触发对齐 panic
        let mut control = [0u64; 8];
        let control_ptr = control.as_mut_ptr() as *mut u8;
        let cmsg_data_off = align(std::mem::size_of::<WsaCmsgHdr>());
        let cmsg_len = cmsg_data_off + std::mem::size_of::<InPktInfo>();
        unsafe {
            let hdr = control_ptr as *mut WsaCmsgHdr;
            (*hdr).cmsg_len = cmsg_len;
            (*hdr).cmsg_level = 0; // IPPROTO_IP
            (*hdr).cmsg_type = 19; // IP_PKTINFO
            let info = control_ptr.add(cmsg_data_off) as *mut InPktInfo;
            (*info).ipi_addr = gateway.octets();
            (*info).ipi_ifindex = if_index;
        }

        let msg = WsaMsg {
            name: &name as *const _ as *mut c_void,
            namelen: std::mem::size_of::<SockAddrIn>() as i32,
            buffers: &mut wsa_buf,
            buffer_count: 1,
            control: WsaBuf {
                len: align(cmsg_len) as u32,
                buf: control_ptr,
            },
            flags: 0,
        };
        let mut bytes = 0u32;
        let ret = unsafe {
            send_msg(
                sock.as_raw_socket() as usize,
                &msg,
                0,
                &mut bytes,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn align(n: usize) -> usize {
        (n + 7) & !7
    }

    pub fn recv_packet(sock: &UdpSocket, buf: &mut [u8]) -> io::Result<(usize, u32)> {
        // u64 数组保证控制信息按 8 字节对齐；WSACMSGHDR 需要这个对齐。
        let mut ctrl = [0u64; 32];
        let ctrl_ptr = ctrl.as_mut_ptr() as *mut u8;
        let ctrl_bytes = std::mem::size_of_val(&ctrl);
        let mut wsa_buf = WsaBuf {
            len: buf.len() as u32,
            buf: buf.as_mut_ptr(),
        };
        let mut msg = WsaMsg {
            name: std::ptr::null_mut(),
            namelen: 0,
            buffers: &mut wsa_buf,
            buffer_count: 1,
            control: WsaBuf {
                len: ctrl_bytes as u32,
                buf: ctrl_ptr,
            },
            flags: 0,
        };
        let mut bytes = 0u32;
        let Some(recv_msg) = wsa_recv_msg(sock) else {
            // 拿不到扩展函数时退化为普通接收（不做接口过滤）
            let (n, _) = sock.recv_from(buf)?;
            return Ok((n, 0));
        };
        let ret = unsafe {
            recv_msg(
                sock.as_raw_socket() as usize,
                &mut msg,
                &mut bytes,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }

        let mut if_index = 0u32;
        let ctrl_len = (msg.control.len as usize).min(ctrl_bytes);
        let mut off = 0usize;
        while off + std::mem::size_of::<WsaCmsgHdr>() <= ctrl_len {
            let hdr_ptr = unsafe { ctrl_ptr.add(off) as *const WsaCmsgHdr };
            let hdr = unsafe { &*hdr_ptr };
            if hdr.cmsg_len < std::mem::size_of::<WsaCmsgHdr>() {
                break;
            }
            if hdr.cmsg_level == 0 && hdr.cmsg_type == 19 {
                let data_off = off + align(std::mem::size_of::<WsaCmsgHdr>());
                if data_off + std::mem::size_of::<InPktInfo>() <= ctrl_len {
                    let info = unsafe { &*(ctrl_ptr.add(data_off) as *const InPktInfo) };
                    if_index = info.ipi_ifindex;
                }
            }
            let step = align(hdr.cmsg_len);
            if step == 0 {
                break;
            }
            off += step;
        }

        Ok((bytes as usize, if_index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_discover(mac: [u8; 6], requested: Option<Ipv4Addr>, hostname: Option<&str>) -> Vec<u8> {
        let mut buf = vec![0u8; 240];
        buf[0] = BOOTREQUEST;
        buf[1] = 1;
        buf[2] = 6;
        buf[4..8].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        buf[28..34].copy_from_slice(&mac);
        buf[236..240].copy_from_slice(&MAGIC);
        buf.extend_from_slice(&[53, 1, DHCPDISCOVER]);
        if let Some(ip) = requested {
            buf.extend_from_slice(&[50, 4]);
            buf.extend_from_slice(&ip.octets());
        }
        if let Some(host) = hostname {
            buf.push(12);
            buf.push(host.len() as u8);
            buf.extend_from_slice(host.as_bytes());
        }
        buf.push(255);
        buf
    }

    fn settings() -> DhcpSettings {
        DhcpSettings {
            gateway: Ipv4Addr::new(10, 10, 10, 1),
            prefix_len: 24,
            dns: vec![Ipv4Addr::new(223, 5, 5, 5)],
            lease_secs: 3600,
            pool_start: 100,
            pool_end: 110,
            bindings: HashMap::new(),
            if_index: 0,
        }
    }

    #[test]
    fn parse_discover_packet() {
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x11];
        let packet = make_discover(mac, Some(Ipv4Addr::new(10, 10, 10, 105)), Some("phone"));
        let req = Request::parse(&packet).expect("应当解析成功");
        assert_eq!(req.msg_type, DHCPDISCOVER);
        assert_eq!(req.chaddr, mac);
        assert_eq!(req.requested_ip, Some(Ipv4Addr::new(10, 10, 10, 105)));
        assert_eq!(req.hostname.as_deref(), Some("phone"));
    }

    #[test]
    fn reject_non_dhcp_packet() {
        assert!(Request::parse(&[0u8; 100]).is_none());
        let mut packet = make_discover([0; 6], None, None);
        packet[236] = 0;
        assert!(Request::parse(&packet).is_none());
    }

    #[test]
    fn offer_fields() {
        let mac = [0x02, 0, 0, 0, 0, 1];
        let packet = make_discover(mac, None, None);
        let req = Request::parse(&packet).unwrap();
        let cfg = settings();
        let reply = build_reply(&req, &cfg, DHCPOFFER, Ipv4Addr::new(10, 10, 10, 100));
        assert_eq!(reply[0], BOOTREPLY);
        assert_eq!(&reply[16..20], &[10, 10, 10, 100]);
        assert_eq!(&reply[20..24], &[10, 10, 10, 1]);
        assert_eq!(&reply[28..34], &mac);
        // 找 option 53
        let mut i = 240;
        let mut msg_type = None;
        while i < reply.len() && reply[i] != 255 {
            let code = reply[i];
            let len = reply[i + 1] as usize;
            if code == 53 && len == 1 {
                msg_type = Some(reply[i + 2]);
            }
            i += 2 + len;
        }
        assert_eq!(msg_type, Some(DHCPOFFER));
    }

    #[test]
    fn static_binding_wins() {
        let mac = [0x02, 0, 0, 0, 0, 2];
        let mut cfg = settings();
        cfg.bindings.insert(mac, Ipv4Addr::new(10, 10, 10, 50));
        let packet = make_discover(mac, None, None);
        let req = Request::parse(&packet).unwrap();
        let mut table = HashMap::new();
        let ip = allocate(&req, &cfg, &mut table, Instant::now(), &cfg.bindings).unwrap();
        assert_eq!(ip, Ipv4Addr::new(10, 10, 10, 50));
    }

    #[test]
    fn dynamic_pool_skips_used_addresses() {
        let cfg = settings();
        let mut table = HashMap::new();
        let mac_a = [0x02, 0, 0, 0, 0, 0x0a];
        let mac_b = [0x02, 0, 0, 0, 0, 0x0b];
        let packet_a = make_discover(mac_a, None, None);
        let packet_b = make_discover(mac_b, None, None);
        let req_a = Request::parse(&packet_a).unwrap();
        let req_b = Request::parse(&packet_b).unwrap();
        let now = Instant::now();
        let ip_a = allocate(&req_a, &cfg, &mut table, now, &cfg.bindings).unwrap();
        table.insert(
            mac_a,
            Lease {
                mac: mac_a,
                ip: ip_a,
                hostname: String::new(),
                expires: now + Duration::from_secs(3600),
                static_binding: false,
            },
        );
        let ip_b = allocate(&req_b, &cfg, &mut table, now, &cfg.bindings).unwrap();
        assert_ne!(ip_a, ip_b);
        assert_eq!(ip_a, Ipv4Addr::new(10, 10, 10, 100));
        assert_eq!(ip_b, Ipv4Addr::new(10, 10, 10, 101));
    }

    #[test]
    fn expired_leases_release_addresses() {
        let cfg = settings();
        let now = Instant::now();
        let old_mac = [0x02, 0, 0, 0, 0, 0x20];
        let new_mac = [0x02, 0, 0, 0, 0, 0x21];
        let mut table = HashMap::new();
        table.insert(
            old_mac,
            Lease {
                mac: old_mac,
                ip: Ipv4Addr::new(10, 10, 10, 100),
                hostname: String::new(),
                expires: now - Duration::from_secs(1),
                static_binding: false,
            },
        );
        let packet = make_discover(new_mac, None, None);
        let req = Request::parse(&packet).unwrap();
        let ip = allocate(&req, &cfg, &mut table, now, &cfg.bindings).unwrap();
        assert_eq!(ip, Ipv4Addr::new(10, 10, 10, 100));
        assert!(!table.contains_key(&old_mac));
    }

    #[test]
    fn requested_ip_must_come_from_pool() {
        let cfg = settings();
        let packet = make_discover([0x02, 0, 0, 0, 0, 0x30], None, None);
        let req = Request::parse(&packet).unwrap();
        let table = HashMap::new();
        let now = Instant::now();
        assert!(is_usable(
            Ipv4Addr::new(10, 10, 10, 100),
            &req,
            &cfg,
            &table,
            &cfg.bindings,
            now
        ));
        assert!(!is_usable(
            Ipv4Addr::new(10, 10, 10, 50),
            &req,
            &cfg,
            &table,
            &cfg.bindings,
            now
        ));
        assert!(!is_usable(
            Ipv4Addr::new(8, 8, 8, 8),
            &req,
            &cfg,
            &table,
            &cfg.bindings,
            now
        ));
    }

    #[test]
    fn pool_exhaustion_returns_none() {
        let cfg = settings();
        let now = Instant::now();
        let mut table = HashMap::new();
        for last in 100..=110u8 {
            let mac = [0x02, 0, 0, 0, 0, last];
            table.insert(
                mac,
                Lease {
                    mac,
                    ip: Ipv4Addr::new(10, 10, 10, last),
                    hostname: String::new(),
                    expires: now + Duration::from_secs(3600),
                    static_binding: false,
                },
            );
        }
        let packet = make_discover([0x02, 0, 0, 0, 0, 0x40], None, None);
        let req = Request::parse(&packet).unwrap();
        assert!(allocate(&req, &cfg, &mut table, now, &cfg.bindings).is_none());
    }

    #[test]
    fn settings_reject_bad_bindings() {
        let mut cfg = settings();
        cfg.bindings
            .insert([0x02, 0, 0, 0, 0, 0x50], Ipv4Addr::new(10, 10, 10, 1));
        assert!(cfg.validate().is_err());

        let mut cfg = settings();
        cfg.bindings
            .insert([0x02, 0, 0, 0, 0, 0x50], Ipv4Addr::new(10, 10, 10, 50));
        cfg.bindings
            .insert([0x02, 0, 0, 0, 0, 0x51], Ipv4Addr::new(10, 10, 10, 50));
        assert!(cfg.validate().is_err());
    }

    fn reply_type(reply: &[u8]) -> Option<u8> {
        let mut i = 240;
        while i + 1 < reply.len() && reply[i] != 255 {
            let code = reply[i];
            let len = reply[i + 1] as usize;
            if i + 2 + len > reply.len() {
                break;
            }
            if code == 53 {
                return Some(reply[i + 2]);
            }
            i += 2 + len;
        }
        None
    }

    fn binding_request(mac: [u8; 6], requested: Ipv4Addr) -> Request {
        Request {
            xid: [0x12, 0x34, 0x56, 0x78],
            flags: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            chaddr: mac,
            msg_type: DHCPREQUEST,
            requested_ip: Some(requested),
            hostname: None,
        }
    }

    #[test]
    fn fixed_binding_acks_bound_address() {
        let mac = [0x02, 0, 0, 0, 0, 0x60];
        let bound = Ipv4Addr::new(10, 10, 10, 50);
        let mut cfg = settings();
        cfg.bindings.insert(mac, bound);
        let leases = Arc::new(Mutex::new(HashMap::new()));
        let bindings = Arc::new(Mutex::new(cfg.bindings.clone()));
        let reply = handle_request(
            &binding_request(mac, bound),
            &cfg,
            &leases,
            &bindings,
        )
        .unwrap();
        assert_eq!(reply_type(&reply), Some(DHCPACK));
        assert_eq!(leases.lock().unwrap()[&mac].ip, bound);
    }

    #[test]
    fn fixed_binding_naks_other_address() {
        let mac = [0x02, 0, 0, 0, 0, 0x61];
        let mut cfg = settings();
        cfg.bindings
            .insert(mac, Ipv4Addr::new(10, 10, 10, 50));
        let leases = Arc::new(Mutex::new(HashMap::new()));
        let bindings = Arc::new(Mutex::new(cfg.bindings.clone()));
        let reply = handle_request(
            &binding_request(mac, Ipv4Addr::new(10, 10, 10, 101)),
            &cfg,
            &leases,
            &bindings,
        )
        .unwrap();
        assert_eq!(reply_type(&reply), Some(DHCPNAK));
        assert!(leases.lock().unwrap().is_empty());
    }
}
