//! Windows 系统托盘：关闭主窗口时隐藏到托盘，托盘图标右键菜单可重新显示或真正退出。

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, EnumThreadWindows, GetCursorPos, GetMessageW, IsIconic, IsWindowVisible,
    LoadIconW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, ShowWindow, TrackPopupMenu, TranslateMessage, MF_SEPARATOR, MF_STRING, MSG,
    SW_HIDE, SW_RESTORE, SW_SHOW, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON, WINDOW_STYLE,
    WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP,
    WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW,
};

/// 托盘自己的隐藏消息窗口（与主窗口同进程，但在独立的线程和隐藏窗口里）。
const TRAY_WINDOW_CLASS: PCWSTR = w!("RustHotSpotTrayWindow");

const TRAY_ICON_ID: u32 = 1;
const WM_TRAY: u32 = WM_APP + 1;
const ID_SHOW: usize = 1;
const ID_QUIT: usize = 2;

/// 用户在托盘上触发的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Show,
    Quit,
}

type Handler = Box<dyn Fn(TrayAction) + Send + Sync + 'static>;

static STARTED: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static HANDLER: Mutex<Option<Handler>> = Mutex::new(None);
static QUEUE: Mutex<VecDeque<TrayAction>> = Mutex::new(VecDeque::new());

/// 托盘图标是否已经成功挂上。关闭窗口前要检查，避免托盘不可用时把窗口藏起来
/// 导致程序无法恢复。
pub fn is_ready() -> bool {
    READY.load(Ordering::SeqCst)
}

/// 启动托盘线程；重复调用无副作用。
pub fn start() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("hotspot-tray".into())
        .spawn(|| unsafe { run() });
}

/// 绑定动作处理器。UI 就绪前发生的点击会排队，绑定后补发。
pub fn attach(handler: impl Fn(TrayAction) + Send + Sync + 'static) {
    if let Ok(mut h) = HANDLER.lock() {
        *h = Some(Box::new(handler));
    }
    // 先装好处理器，再排空队列，保证期间新到的动作不会丢失。
    loop {
        let next = QUEUE.lock().ok().and_then(|mut q| q.pop_front());
        let Some(action) = next else { break };
        dispatch(action);
    }
}

/// 隐藏主窗口（关闭按钮的行为）。
pub fn hide_main_window() {
    if let Some(hwnd) = main_window() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// 重新显示主窗口并置于前台。
pub fn show_main_window() {
    if let Some(hwnd) = main_window() {
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            } else {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// 移除托盘图标并结束托盘线程（应用退出时调用）。
pub fn shutdown() {
    READY.store(false, Ordering::SeqCst);
    let raw = TRAY_HWND.swap(0, Ordering::SeqCst);
    if raw != 0 {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(raw as *mut c_void)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

fn dispatch(action: TrayAction) {
    let handler = HANDLER.lock().ok().and_then(|h| h.as_ref().map(|f| f(action)));
    if handler.is_none() {
        if let Ok(mut queue) = QUEUE.lock() {
            queue.push_back(action);
        }
    }
}

fn main_window() -> Option<HWND> {
    let cached = MAIN_HWND.load(Ordering::Relaxed);
    if cached != 0 {
        return Some(HWND(cached as *mut c_void));
    }
    // 只在 UI 线程调用：该线程上唯一的可见顶层窗口就是 charis-ui 主窗口，
    // 托盘窗口在独立的托盘线程里，不会混淆。
    unsafe {
        let mut found = HWND::default();
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(find_thread_window),
            LPARAM(&mut found as *mut HWND as isize),
        );
        if found.0.is_null() {
            return None;
        }
        MAIN_HWND.store(found.0 as isize, Ordering::Relaxed);
        Some(found)
    }
}

unsafe extern "system" fn find_thread_window(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let found = &mut *(lparam.0 as *mut HWND);
    if IsWindowVisible(hwnd).as_bool() {
        *found = hwnd;
        return windows::Win32::Foundation::FALSE;
    }
    windows::Win32::Foundation::TRUE
}

unsafe fn run() {
    let Ok(module) = GetModuleHandleW(None) else {
        return;
    };
    let hinstance = HINSTANCE(module.0);

    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance,
        lpszClassName: TRAY_WINDOW_CLASS,
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return;
    }

    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        TRAY_WINDOW_CLASS,
        w!("RustHotSpotTray"),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinstance),
        None,
    ) else {
        return;
    };
    TRAY_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
    TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::SeqCst);

    if !add_icon(hwnd) {
        let _ = DestroyWindow(hwnd);
        return;
    }
    READY.store(true, Ordering::SeqCst);

    let mut msg = MSG::default();
    loop {
        let ret = GetMessageW(&mut msg, None, 0, 0);
        if ret.0 <= 0 {
            break;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    READY.store(false, Ordering::SeqCst);
}

/// 注册托盘图标。Explorer 重启后（TaskbarCreated）会重新调用。
unsafe fn add_icon(hwnd: HWND) -> bool {
    let module = GetModuleHandleW(None).unwrap_or_default();
    let hinstance = HINSTANCE(module.0);
    // 资源 ID 1 是 build.rs 嵌入的应用图标；失败时退回系统默认图标（IDI_APPLICATION = 32512）。
    let icon = LoadIconW(Some(hinstance), PCWSTR(1 as *const u16))
        .or_else(|_| LoadIconW(None, PCWSTR(32512u32 as _)))
        .unwrap_or_default();

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    copy_wide(&mut nid.szTip, "RustHotSpot - 单击显示窗口，右键退出");
    Shell_NotifyIconW(NIM_ADD, &nid).as_bool()
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY => {
            match lparam.0 as u32 {
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => dispatch(TrayAction::Show),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match (wparam.0 & 0xffff) as usize {
                ID_SHOW => dispatch(TrayAction::Show),
                ID_QUIT => dispatch(TrayAction::Quit),
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            READY.store(false, Ordering::SeqCst);
            let nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: TRAY_ICON_ID,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ if msg != 0 && msg == TASKBAR_CREATED.load(Ordering::SeqCst) => {
            // 资源管理器重启后通知区域被清空，重新挂上图标。
            READY.store(add_icon(hwnd), Ordering::SeqCst);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let _ = AppendMenuW(menu, MF_STRING, ID_SHOW, w!("显示主窗口"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("退出"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // 菜单需要窗口在前台，点击别处才会自动关闭。
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN,
        pt.x,
        pt.y,
        Some(0),
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
}

fn copy_wide<const N: usize>(dst: &mut [u16; N], text: &str) {
    let mut i = 0;
    for unit in text.encode_utf16().take(N.saturating_sub(1)) {
        dst[i] = unit;
        i += 1;
    }
    dst[i] = 0;
}
