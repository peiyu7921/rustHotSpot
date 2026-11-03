<p align="center">
  <img src="assets/icon.png" width="96" alt="RustHotSpot 图标">
</p>

<h1 align="center">RustHotSpot</h1>

<p align="center">Windows 上的 Wi-Fi 热点工具：隐藏 SSID、2.4G/5G 频段、按 MAC 固定 IP。</p>

RustHotSpot 是一个单文件的 Windows 桌面应用，用 Rust 和 WinRT 实现，重点解决
Windows 原生「移动热点」做不到的三件事：隐藏网络名称、指定频段、给设备固定 IP。
界面基于 [charis-ui](https://crates.io/crates/charis-ui)，带深色/浅色主题和连接二维码。

> 需要 Windows 10/11（x64）和管理员权限。首次启动会请求 UAC。

## 功能

| 功能 | 实现方式 |
| --- | --- |
| **隐藏 SSID** | Wi-Fi Direct legacy AP，`ListenStateDiscoverability = None`，热点不响应探测、不出现在扫描列表 |
| **2.4G / 5G** | 标准模式使用 `TetheringWiFiBand`；隐藏模式通过网卡 `Preferred Band` 高级属性控制 |
| **设备固定 IP** | 隐藏模式下内置 DHCP 服务器，按 MAC 绑定地址，配合 `NetNat` 转发上网 |
| **实时设备列表** | 隐藏模式显示 DHCP 租约，标准模式显示系统热点客户端；动态地址可一键转固定 |
| **连接二维码** | 运行后自动生成 Wi-Fi 二维码，手机扫码即可加入 |
| **单文件分发** | exe 内嵌应用图标，无安装器、无额外 DLL |

## 界面

窗口很小，主要操作都在顶部：

- **设置**：SSID、密码（可临时显示）、工作模式、频段、隐藏 SSID；隐藏模式下还能设置网段和 DNS。
  运行后二维码卡片会出现在设置页顶部，并附带「复制」按钮。
- **设备**：查看当前连入的设备。隐藏模式显示 MAC、IP、主机名和租约类型；标准模式显示系统客户端。
- **固定 IP**：维护「MAC → IP」绑定表，支持添加、删除和备注。
- 标题栏右侧切换深色/浅色主题；底部状态栏显示最新消息、当前模式和频段。

热点运行时，设置字段会被锁定；停止后即可修改。

## 使用

### 运行已构建的 exe

如果你拿到的是构建好的目录：

1. 双击 `rust-hotspot.exe`，在 UAC 提示中选择「是」。
2. 在「设置」里填写 SSID 和密码，选择模式与频段。
3. 需要固定 IP 时，切到「固定 IP」添加绑定。
4. 点右上角「启动」。

需要清空所有本地配置时，双击同目录的 `reset-config.cmd`，它会删除配置并用默认值重新启动。

### 从源码构建

需要：

- Rust 1.89+
- MinGW-w64（GNU 工具链，`windres` 用于嵌入图标）或 MSVC 工具链 + Windows SDK

```powershell
cargo build --release
```

产物：`target\release\rust-hotspot.exe`。

运行测试：

```powershell
cargo test
```

界面默认使用 **CPU 渲染**，以避开 charis-ui 0.1 的 GPU 后端在部分 Windows 驱动上会触发
`0xC0000005` 的问题。小窗口用 CPU 渲染已经足够流畅；如果你想尝试 GPU：

```powershell
$env:CHARIS_RENDERER="auto"
.\rust-hotspot.exe
```

如果出现黑屏或闪退，去掉这个环境变量，回到默认 CPU 渲染。

## 两种模式

Windows 官方 API 有两套能力，互不重叠，所以程序提供两个后端：

| | 隐藏模式（默认） | 标准模式 |
| --- | --- | --- |
| SSID 隐藏 | ✅ 支持 | ❌ 不支持 |
| 频段 | 自动 / 2.4G / 5G（网卡 `Preferred Band`） | 自动 / 2.4G / 5G（系统接口） |
| 固定 IP | ✅ 内置 DHCP 按 MAC 分配 | ❌ 由系统 ICS 管理 |
| DHCP / NAT | 程序内置 + `NetNat` | 系统 ICS 自动完成 |
| 稳定性 | 依赖网卡 Wi-Fi Direct 驱动 | 通常更稳定 |

### 隐藏模式

使用 `WiFiDirectAdvertisementPublisher` 启动 legacy AP，并把发现状态设为 `None`。
热点不会出现在扫描列表里，手机/电脑需要在 Wi-Fi 设置中选择「添加网络 / 手动连接」，
手动输入 SSID 和密码。

默认自动选择频段。选了 2.4G/5G 后，程序会改写网卡的 `Preferred Band` 高级属性，
停止时恢复原值；这个操作会重置网卡，上游 Wi-Fi 会短暂断开。

### 标准模式

使用 `NetworkOperatorTetheringManager`（Windows 原生移动热点）。频段可选、系统自动
处理 NAT 和 DHCP，更稳定；但不能隐藏 SSID，也不能给设备固定 IP。部分驱动不支持
`SetBand` 时，程序会退回自动频段。

## 连接设备

- 标准模式：正常扫描并连接 SSID。
- 隐藏模式：在设备上手动添加网络，输入 SSID 和密码。
- 运行后也可以用手机扫描「设置」页顶部的二维码；隐藏网络在部分手机上仍需要手动添加。
- 「设备」页里，动态租约右侧的「绑定」可以把当前 IP 直接转成固定绑定。

固定 IP 的可用地址应在热点网段内（默认 `10.10.10.1` 是网关，动态池为
`10.10.10.100 – 10.10.10.200`）。网关、网络地址、广播地址不能作为绑定地址，
同一个 IP 也不能绑定给多个 MAC。

## 配置

配置保存在：

```
%APPDATA%\RustHotSpot\config.json
```

隐藏模式的热点网段目前只支持 `/24`（DHCP 地址池按 `/24` 设计）。配置文件包含
SSID、密码、模式、频段、隐藏开关、网段、DNS、主题和固定 IP 绑定。**密码以明文
保存在这个文件里**，不要把配置文件发给别人。

重置方式：

1. 双击发行目录中的 `reset-config.cmd`；或
2. 手动删除 `%APPDATA%\RustHotSpot` 文件夹，再启动程序。

新配置的默认值：

| 项目 | 默认值 |
| --- | --- |
| SSID | `RustHotspot` |
| 密码 | `12345678` |
| 模式 | 隐藏模式（Wi-Fi Direct） |
| 频段 | 自动 |
| 隐藏 SSID | 开 |
| 网段 | `10.10.10/24` |
| DNS | `223.5.5.5`、`119.29.29.29` |
| 主题 | 深色 |
| 固定 IP | 空 |

建议首次使用就改掉默认密码。

## 隐藏模式的工作原理

<details>
<summary>展开技术细节</summary>

1. 用 `WiFiDirectAdvertisementPublisher` 启动一个 legacy AP，设置 SSID/密码，并把
   `ListenStateDiscoverability` 设为 `None`。
2. Windows 会自动给虚拟网卡配置 `192.168.137.1/24`，并让 ICS 在
   `192.168.137.1:67` 上启动 DHCP。程序删除该地址，改成自己的网段（默认
   `10.10.10.1/24`）——旧监听绑定在已删除的地址上，收不到新网段的广播，于是端口
   交给本程序使用。
3. 用 `New-NetNat` 建立对应网段的 NAT，客户端可以正常上网。
4. 内置 DHCP 服务器监听 UDP 67，按 MAC 绑定表分配固定 IP，其余客户端从
   `10.10.10.100 – 10.10.10.200` 动态分配；DNS 使用界面里配置的地址。
   - 响应通过 `WSASendMsg` + `IP_PKTINFO` 从热点网卡发出（普通 `send_to` 会按
     默认路由走 WLAN，手机收不到），源端口固定为 67。
   - Windows 会为 Wi-Fi Direct AP 自动起一个 ICS 的 DHCP（`192.168.137.1:67`），
     和本程序抢答，导致手机拿到 `192.168.137.x`。程序启动时会添加一条按服务
     （`SharedAccess`）过滤的出站防火墙规则挡住它，并放行入站 UDP 67；停止时两条
     规则一起删除。
5. 如果选了 2.4G/5G，启动前通过 `Set-NetAdapterAdvancedProperty` 写网卡的
   `Preferred Band`，停止时恢复原值。实测 Intel AX201：2.4G 时 AP 走信道 6，
   5G 时走 36。注意这会重置网卡，上游 Wi-Fi 会短暂断开重连。

启动/停止都在独立的工作线程里执行（WinRT 的 Wi-Fi Direct 对象不是 agile 的，
不能跨线程），界面只负责发命令和显示状态，因此点「启动」不会卡住窗口。

</details>

## 命令行自检

```powershell
# 需要先以管理员身份打开终端
.\rust-hotspot.exe --headless 30
```

这会实际启动热点、保持 30 秒、打印租约信息，然后自动清理退出。输出包括启动状态、
SSID、网卡索引、地址、租约和停止状态；适合排查网卡/驱动问题。

> `--headless` 需要管理员权限；release 版是 GUI 子系统、没有控制台输出，建议在管理员
> 终端里运行（或把 stderr 重定向到文件）查看结果。

## 已知限制

- 需要管理员权限（配置网卡地址、`NetNat`、监听 67 端口）。
- 隐藏模式的频段依赖网卡驱动的 `Preferred Band`（或类似）高级属性；没有该属性的
  网卡会回退为自动频段。设置/恢复频段时网卡会重置，上游 Wi-Fi 短暂断连。
- 隐藏模式下程序会创建名为 `RustHotspot` 的 `NetNat`，退出时自动删除。
- Wi-Fi Direct 依赖无线网卡驱动支持；部分老网卡/驱动会返回 `Aborted`。
- DHCP 使用 `0.0.0.0:67`（`SO_REUSEADDR`）接收广播，并通过 `IP_PKTINFO` 只处理
  热点网卡的报文。如果机器同时在用 Hyper-V/ICS 共享，注意可能存在的端口竞争。
- 固定 IP 只对隐藏模式生效；标准模式的 DHCP 由系统 ICS 管理，无法定制租约。
- 配置中的热点密码是明文保存的。

## 排错

**双击后没窗口 / 闪退**

- 先确认 UAC 提示没有被取消；取消 UAC 会让程序直接退出。
- 保持默认 CPU 渲染（不要设置 `CHARIS_RENDERER=auto`），并更新无线网卡驱动。
- 如果仍然闪退，可以尝试从终端运行并重定向日志，查看是否有具体报错。

**手机拿到 `192.168.137.x`**

系统 ICS 的 DHCP 抢答了。程序会尽量用防火墙规则挡掉它；如果仍然出现，重启一次
热点，并确认没有其他 ICS/Hyper-V 共享正在使用同一块网卡。

**隐藏 SSID 搜不到**

这是预期行为，需要在设备上手动添加网络，而不是等待扫描结果。

**切换频段后电脑断网**

设置 `Preferred Band` 会重置无线网卡，上游 Wi-Fi 会短暂断开并自动重连，属于正常现象。

**任务栏图标还是旧的**

Windows 会缓存任务栏图标。取消固定程序后重新固定，或者重启资源管理器。

**清理残留的 NetNat（仅异常退出后需要，管理员 PowerShell）**

```powershell
Get-NetNat
Remove-NetNat -Name RustHotspot
```

## 代码结构

```
src/
  main.rs               入口、管理员提权、--headless 参数
  app.rs                charis-ui 界面、状态刷新与启动/停止流程
  config.rs             配置持久化（%APPDATA%\RustHotSpot\config.json）
  hotspot/mod.rs        热点后端抽象
  hotspot/native.rs     标准模式后端
  hotspot/wifidirect.rs 隐藏模式后端
  netcfg.rs             网卡地址、NetNat 配置
  dhcp.rs               内置 DHCP 服务器（静态租约）
  worker.rs             热点工作线程
  util.rs               提权、PowerShell、MAC 工具
build.rs                把 assets/icon.ico 嵌入 exe
assets/icon.ico         应用图标（16–256 多尺寸）
assets/icon.png         窗口图标（256×256）
```

## 关于绕过系统 API

<details>
<summary>展开调研记录</summary>

实际测过本机 Intel AX201 的所有用户态入口：

| 通道 | 结果 |
| --- | --- |
| `WlanHostedNetwork*`（Win7 SoftAP） | `ERROR_INVALID_PARAMETER(87)`，驱动不支持 |
| `WlanSetInterface` 的 AP / IHV 扩展站点模式 | `87`，不支持 |
| NDISUIO 用户态 OID 通道 | 设备不存在（Win10+ 已移除用户态接口） |
| Intel IHV 扩展 `IntelIHVRouter10.dll` | 只导出 `Dot11ExtIhv*` 回调，非应用 API |
| `WlanSvc` 注册表 | 无隐藏 SSID 开关（`HiddenSsidList` 是客户端扫描列表） |
| WinRT 类型成员 | `WiFiDirectAdvertisement` 无隐藏属性 |
| SSID 设为空串 | `Start()` 返回 `Aborted` |
| Wi-Fi Direct + `ListenStateDiscoverability=None` | 唯一可行 |

结论：隐藏 SSID 由网卡固件/驱动决定，用户态没有绕过路径。要完全脱离系统 API，
只能自研 WDI/NDIS 驱动，或把 USB 无线网卡直通给 Linux 虚拟机跑 hostapd。

</details>
