// SPDX-License-Identifier: AGPL-3.0-only
//! Small Windows notification-area shell for the existing Ozma setup UI.
//!
//! This process owns the tray icon and can open/reopen the controller's setup
//! UI in the default browser and probes public health endpoints for truthful
//! tray status. It does not own controller/node lifecycle or session state.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod windows_shell {
    use std::env;
    use std::ffi::OsStr;
    use std::io::{Read, Write};
    use std::iter::once;
    use std::net::{TcpStream, ToSocketAddrs};
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;

    use windows::core::{w, Error, Result, PCWSTR};
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD,
        NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
        DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, MessageBoxW, PostMessageW,
        PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
        TrackPopupMenu, TranslateMessage, HICON, HMENU, IDI_APPLICATION, MB_ICONERROR, MB_OK,
        MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, SW_SHOWNORMAL, TPM_RIGHTBUTTON, WM_APP,
        WM_COMMAND, WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
    };

    const TRAY_CALLBACK: u32 = WM_APP + 7;
    const TRAY_ID: u32 = 1;
    const OPEN_SETUP: usize = 1001;
    const OPEN_DASHBOARD: usize = 1002;
    const REVIEW_LOCAL_ACCESS: usize = 1003;
    const QUIT: usize = 1004;
    const OPEN_HELP: usize = 1005;
    const TRAY_STATUS_CHANGED: u32 = WM_APP + 8;

    #[derive(Clone, Copy)]
    struct HealthStatus {
        controller: &'static str,
        node: &'static str,
    }

    static HEALTH_STATUS: Mutex<HealthStatus> = Mutex::new(HealthStatus {
        controller: "checking...",
        node: "checking...",
    });
    static HEALTH_STOP: Mutex<Option<Sender<()>>> = Mutex::new(None);
    static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
    static TRAY_ICON: AtomicIsize = AtomicIsize::new(0);
    static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);

    fn controller_url() -> String {
        env::var("OZMA_CONTROLLER_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7380".to_owned())
            .trim_end_matches('/')
            .to_owned()
    }

    fn local_node_url() -> String {
        env::var("OZMA_LOCAL_NODE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7382".to_owned())
            .trim_end_matches('/')
            .to_owned()
    }

    fn help_url() -> String {
        env::var("OZMA_HELP_URL").unwrap_or_else(|_| format!("{}/ui/help", controller_url()))
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(once(0)).collect()
    }

    /// Probe an existing unauthenticated health route without handling any
    /// credentials. HTTPS is intentionally reported as unknown here rather
    /// than downgraded or treated as reachable without checking TLS.
    fn health_status(base_url: &str, path: &str) -> &'static str {
        let Some(address_part) = base_url.strip_prefix("http://") else {
            return "status unavailable (HTTP only)";
        };
        let authority = address_part.split('/').next().unwrap_or_default();
        if authority.is_empty() || authority.contains('@') {
            return "status unavailable (invalid address)";
        }
        let Some(address) = address_with_default_http_port(authority) else {
            return "status unavailable (invalid address)";
        };
        let Ok(mut addresses) = address.to_socket_addrs() else {
            return "unreachable";
        };
        let Some(address) = addresses.next() else {
            return "unreachable";
        };
        let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(500))
        else {
            return "unreachable";
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
        let base_path = address_part
            .strip_prefix(authority)
            .unwrap_or_default()
            .trim_end_matches('/');
        let request_path = format!("{base_path}{path}");
        let request = format!(
            "GET {request_path} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
        );
        if stream.write_all(request.as_bytes()).is_err() {
            return "unreachable";
        }
        let mut response = [0_u8; 64];
        let Ok(length) = stream.read(&mut response) else {
            return "unreachable";
        };
        let first_line = String::from_utf8_lossy(&response[..length]);
        if first_line.starts_with("HTTP/1.") && first_line.split_whitespace().nth(1) == Some("200")
        {
            "reachable"
        } else {
            "unreachable"
        }
    }

    fn address_with_default_http_port(authority: &str) -> Option<String> {
        if authority.starts_with('[') {
            let closing = authority.find(']')?;
            let suffix = &authority[closing + 1..];
            return match suffix {
                "" => Some(format!("{authority}:80")),
                value if value.starts_with(':') && value.len() > 1 => Some(authority.to_owned()),
                _ => None,
            };
        }

        match authority.matches(':').count() {
            0 => Some(format!("{authority}:80")),
            1 if !authority.ends_with(':') => Some(authority.to_owned()),
            // IPv6 literals must be bracketed in an HTTP authority.
            _ => None,
        }
    }

    fn update_health_status(controller: &'static str, node: &'static str) {
        if let Ok(mut status) = HEALTH_STATUS.lock() {
            status.controller = controller;
            status.node = node;
        }
        let hwnd = TRAY_HWND.load(Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                let _ = PostMessageW(
                    HWND(hwnd),
                    TRAY_STATUS_CHANGED,
                    WPARAM::default(),
                    LPARAM::default(),
                );
            }
        }
    }

    fn health_worker(receiver: Receiver<()>) {
        loop {
            match receiver.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => {}
            }

            let controller_url = controller_url();
            let controller = health_status(&controller_url, "/health");
            match receiver.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => {}
            }

            let node_url = local_node_url();
            let node = health_status(&node_url, "/api/v1/health");
            update_health_status(controller, node);

            match receiver.recv_timeout(Duration::from_secs(5)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn start_health_worker() -> std::io::Result<()> {
        let (sender, receiver) = mpsc::channel();
        if let Ok(mut stop) = HEALTH_STOP.lock() {
            *stop = Some(sender);
        }
        thread::Builder::new()
            .name("ozma-tray-health".to_owned())
            .spawn(move || health_worker(receiver))
            .map(drop)
    }

    fn stop_health_worker() {
        if let Ok(mut stop) = HEALTH_STOP.lock() {
            if let Some(sender) = stop.take() {
                let _ = sender.send(());
            }
        }
    }

    fn cached_health_status() -> HealthStatus {
        HEALTH_STATUS
            .lock()
            .map(|status| *status)
            .unwrap_or(HealthStatus {
                controller: "status unavailable",
                node: "status unavailable",
            })
    }

    fn icon_near_executable() -> Option<HICON> {
        let executable = env::current_exe().ok()?;
        let icon_path: PathBuf = executable.parent()?.join("ozma.ico");
        let path = wide(icon_path.as_os_str());
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{
                LoadImageW, IMAGE_ICON, LR_DEFAULTSIZE, LR_LOADFROMFILE,
            };
            let image = LoadImageW(
                HINSTANCE::default(),
                PCWSTR(path.as_ptr()),
                IMAGE_ICON,
                0,
                0,
                LR_DEFAULTSIZE | LR_LOADFROMFILE,
            )
            .ok()?;
            Some(HICON(image.0))
        }
    }

    fn tooltip_text(status: HealthStatus) -> String {
        let controller = if status.controller == "reachable" {
            "controller reachable"
        } else if status.controller == "checking..." {
            "checking controller"
        } else {
            "controller unavailable"
        };
        let node = if status.node == "reachable" {
            "this computer reachable"
        } else if status.node == "checking..." {
            "checking this computer"
        } else {
            "this computer unavailable"
        };
        format!("Ozma — {controller}; {node}")
    }

    unsafe fn set_tray_tooltip(hwnd: HWND, icon: HICON, add_icon: bool) -> Result<()> {
        let mut data = NOTIFYICONDATAW::default();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ID;
        data.uFlags = NIF_TIP | NIF_SHOWTIP;
        if add_icon {
            data.uFlags |= NIF_MESSAGE | NIF_ICON;
            data.uCallbackMessage = TRAY_CALLBACK;
            data.hIcon = icon;
        }
        let status = cached_health_status();
        let tip: Vec<u16> = tooltip_text(status)
            .encode_utf16()
            .take(data.szTip.len() - 1)
            .collect();
        data.szTip[..tip.len()].copy_from_slice(&tip);
        let operation = if add_icon { NIM_ADD } else { NIM_MODIFY };
        if Shell_NotifyIconW(operation, &data).as_bool() {
            Ok(())
        } else {
            Err(Error::from_win32())
        }
    }

    unsafe fn add_tray_icon(hwnd: HWND, icon: HICON) -> Result<()> {
        set_tray_tooltip(hwnd, icon, true)
    }

    unsafe fn remove_tray_icon(hwnd: HWND) {
        let mut data = NOTIFYICONDATAW::default();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ID;
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }

    unsafe fn open_url(url: &str) {
        let target = wide(OsStr::new(url));
        let result = ShellExecuteW(
            HWND::default(),
            w!("open"),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        if result.0 as usize <= 32 {
            show_error(&format!(
                "Could not open the Ozma UI in the default browser:\n{url}"
            ));
        }
    }

    unsafe fn show_menu(hwnd: HWND) {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let url = controller_url();
        let cached = cached_health_status();
        let node_url = local_node_url();
        let status = format!("Controller: {} ({url})", cached.controller);
        let status_wide = wide(OsStr::new(&status));
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(status_wide.as_ptr()));
        let node_status = format!("This computer: {} ({node_url})", cached.node);
        let node_status_wide = wide(OsStr::new(&node_status));
        let _ = AppendMenuW(
            menu,
            MF_STRING | MF_GRAYED,
            0,
            PCWSTR(node_status_wide.as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, OPEN_SETUP, w!("Open setup"));
        let _ = AppendMenuW(menu, MF_STRING, OPEN_DASHBOARD, w!("Open Ozma"));
        let _ = AppendMenuW(menu, MF_STRING, OPEN_HELP, w!("Help and setup guide"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            REVIEW_LOCAL_ACCESS,
            w!("Review access on this computer"),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, QUIT, w!("Quit tray shell"));

        let mut point = POINT::default();
        if GetCursorPos(&mut point).is_ok() {
            let _ = SetForegroundWindow(hwnd);
            let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, 0, hwnd, None);
            let _ = PostMessageW(hwnd, WM_NULL, WPARAM::default(), LPARAM::default());
        }
        let _ = DestroyMenu(menu);
    }

    pub fn show_error(message: &str) {
        let text = wide(OsStr::new(message));
        unsafe {
            let _ = MessageBoxW(
                HWND::default(),
                PCWSTR(text.as_ptr()),
                w!("Ozma"),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            TRAY_CALLBACK if lparam.0 as u32 == WM_RBUTTONUP => {
                show_menu(hwnd);
                LRESULT(0)
            }
            TRAY_CALLBACK if lparam.0 as u32 == WM_LBUTTONUP => {
                open_url(&format!("{}/ui/", controller_url()));
                LRESULT(0)
            }
            TRAY_STATUS_CHANGED => {
                let icon = TRAY_ICON.load(Ordering::SeqCst);
                if icon != 0 {
                    let _ = set_tray_tooltip(hwnd, HICON(icon), false);
                }
                LRESULT(0)
            }
            message if message == TASKBAR_CREATED.load(Ordering::SeqCst) => {
                let icon = TRAY_ICON.load(Ordering::SeqCst);
                if icon != 0 {
                    let _ = add_tray_icon(hwnd, HICON(icon));
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let base = controller_url();
                match wparam.0 & 0xffff {
                    OPEN_SETUP => open_url(&format!("{base}/ui/setup")),
                    OPEN_DASHBOARD => open_url(&format!("{base}/ui/")),
                    OPEN_HELP => open_url(&help_url()),
                    REVIEW_LOCAL_ACCESS => {
                        open_url(&format!("{}/api/v1/local/consent", local_node_url()))
                    }
                    QUIT => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                TRAY_HWND.store(0, Ordering::SeqCst);
                stop_health_worker();
                remove_tray_icon(hwnd);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }

    pub fn run() -> Result<()> {
        unsafe {
            let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
            if taskbar_created == 0 {
                return Err(Error::from_win32());
            }
            TASKBAR_CREATED.store(taskbar_created, Ordering::SeqCst);

            let module = GetModuleHandleW(PCWSTR::null())?;
            let instance = HINSTANCE(module.0);
            let class_name = w!("OzmaDesktopTrayWindow");
            let window_class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: class_name,
                ..Default::default()
            };
            if RegisterClassW(&window_class) == 0 {
                return Err(Error::from_win32());
            }

            let hwnd = CreateWindowExW(
                Default::default(),
                class_name,
                w!("Ozma"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                HWND::default(),
                HMENU::default(),
                instance,
                None,
            );
            if hwnd.0 == 0 {
                return Err(Error::from_win32());
            }
            TRAY_HWND.store(hwnd.0, Ordering::SeqCst);

            let icon = icon_near_executable()
                .or_else(|| LoadIconW(HINSTANCE::default(), IDI_APPLICATION).ok())
                .ok_or_else(Error::from_win32)?;
            TRAY_ICON.store(icon.0, Ordering::SeqCst);
            if let Err(error) = add_tray_icon(hwnd, icon) {
                TRAY_HWND.store(0, Ordering::SeqCst);
                let _ = DestroyWindow(hwnd);
                return Err(error);
            }

            if let Err(error) = start_health_worker() {
                TRAY_HWND.store(0, Ordering::SeqCst);
                let _ = DestroyWindow(hwnd);
                return Err(Error::new(
                    windows::core::HRESULT(
                        (0x80070000u32 | error.raw_os_error().unwrap_or(1) as u32) as i32,
                    ),
                    "unable to start tray status worker",
                ));
            }

            let mut message = MSG::default();
            let mut message_error = None;
            loop {
                let result = GetMessageW(&mut message, HWND::default(), 0, 0).0;
                if result <= 0 {
                    if result < 0 {
                        message_error = Some(Error::from_win32());
                    }
                    break;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            stop_health_worker();
            if let Some(error) = message_error {
                return Err(error);
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
fn main() {
    if let Err(error) = windows_shell::run() {
        windows_shell::show_error(&format!("Ozma tray shell failed: {error}"));
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("ozma-desktop is currently a Windows-only notification-area shell");
    std::process::exit(1);
}
