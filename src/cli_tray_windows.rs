use std::cell::Cell;
use std::future::pending;
use std::io;
use std::net::SocketAddr;
use std::ptr::null;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::oneshot;
use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NIN_SELECT,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, EndMenu, GetCursorPos, GetMessageW,
    GetSystemMetrics, HICON, IDI_APPLICATION, LoadIconW, MF_GRAYED, MF_STRING, MSG, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON,
    SW_SHOWNORMAL, SetForegroundWindow, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
    UnregisterClassW, WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_ENDSESSION, WM_NULL,
    WM_RBUTTONUP, WNDCLASSW,
};

const CLASS_NAME: &str = "9RouterTrayWindow";
const CALLBACK: u32 = WM_APP + 1;
const MENU_STATUS: usize = 100;
const MENU_OPEN: usize = 101;
const MENU_QUIT: usize = 102;
const MENU_AUTOSTART: usize = 103;
const KEY_SELECT: u32 = NIN_SELECT + 1;
const TIMEOUT: Duration = Duration::from_secs(3);
#[path = "cli_tray_windows_startup.rs"]
mod startup;
thread_local! {
    static THREAD_UI: Cell<*const Ui> = const { Cell::new(null()) };
}

pub struct TraySession {
    window: Arc<AtomicUsize>,
    quit: oneshot::Receiver<()>,
    closed: oneshot::Receiver<()>,
}

impl TraySession {
    pub fn is_registered(&self) -> bool {
        self.window.load(Ordering::Acquire) != 0
    }

    pub async fn wait(&mut self) {
        if (&mut self.quit).await.is_ok() {
            use std::io::Write;
            let _ = writeln!(io::stdout(), "Shutting down from tray...");
        } else {
            pending::<()>().await;
        }
    }

    pub async fn stop(self) {
        let window = self.window.load(Ordering::Acquire) as HWND;
        if !window.is_null() {
            unsafe { PostMessageW(window, WM_CLOSE, 0, 0) };
        }
        let _ = tokio::time::timeout(TIMEOUT, self.closed).await;
    }
}

pub fn enable_autostart() -> io::Result<()> {
    startup::Startup::new()?.enable()
}

pub fn detach_console() -> io::Result<()> {
    use windows_sys::Win32::System::Console::{
        FreeConsole, GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetStdHandle,
    };
    let kinds = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    let inherited = kinds.map(|kind| {
        let handle = unsafe { GetStdHandle(kind) };
        let mut mode = 0;
        unsafe { GetConsoleMode(handle, &mut mode) != 0 }
    });
    if unsafe { FreeConsole() } == 0 {
        return Err(io::Error::last_os_error());
    }
    for (kind, inherited) in kinds.into_iter().zip(inherited) {
        if inherited {
            unsafe { SetStdHandle(kind, std::ptr::null_mut()) };
        }
    }
    Ok(())
}

pub async fn start(address: SocketAddr) -> TraySession {
    let dashboard = dashboard_url(address);
    let (quit, quit_rx) = oneshot::channel();
    let (ready, ready_rx) = oneshot::channel();
    let (closed, closed_rx) = oneshot::channel();
    let window = Arc::new(AtomicUsize::new(0));
    let worker_window = window.clone();
    let worker_dashboard = dashboard.clone();
    let worker = std::thread::Builder::new()
        .name("9router-windows-tray".into())
        .spawn(move || {
            let mut ready = Some(ready);
            if let Err(error) = run(
                address.port(),
                worker_dashboard,
                worker_window,
                quit,
                &mut ready,
            ) && let Some(ready) = ready
            {
                let _ = ready.send(Err(error));
            }
            let _ = closed.send(());
        });
    let status = match worker {
        Ok(_) => match tokio::time::timeout(TIMEOUT, ready_rx).await {
            Ok(Ok(status)) => status,
            _ => Err(io::Error::other(
                "the Windows tray did not register within 3 seconds",
            )),
        },
        Err(error) => Err(error),
    };
    match status {
        Ok(()) => {
            println!("9Router is running in the Windows system tray. Dashboard at {dashboard}")
        }
        Err(error) => {
            println!("The Windows tray is unavailable. The server is still running.");
            println!("{error}");
            println!("Dashboard at {dashboard}");
        }
    }
    TraySession {
        window,
        quit: quit_rx,
        closed: closed_rx,
    }
}

fn dashboard_url(address: SocketAddr) -> String {
    if address.ip().is_unspecified() {
        if address.is_ipv6() {
            format!("http://[::1]:{}/dashboard", address.port())
        } else {
            format!("http://localhost:{}/dashboard", address.port())
        }
    } else {
        format!("http://{address}/dashboard")
    }
}

struct Ui {
    port: u16,
    dashboard: Vec<u16>,
    taskbar_created: u32,
    icon: Icon,
    quit: Cell<Option<oneshot::Sender<()>>>,
    startup: Option<startup::Startup>,
}

fn run(
    port: u16,
    dashboard: String,
    identity: Arc<AtomicUsize>,
    quit: oneshot::Sender<()>,
    ready: &mut Option<oneshot::Sender<io::Result<()>>>,
) -> io::Result<()> {
    let class = WindowClass::register()?;
    let ui = Ui {
        port,
        dashboard: wide(&dashboard),
        taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
        icon: Icon::load(),
        quit: Cell::new(Some(quit)),
        startup: startup::Startup::new().ok(),
    };
    let _registered = RegisteredUi::install(&ui);
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.name.as_ptr(),
            class.name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            class.instance,
            null(),
        )
    };
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    let window = Window { hwnd, identity };
    if !ui.add_icon(hwnd) {
        return Err(io::Error::other(
            "Shell_NotifyIconW could not register the 9Router icon",
        ));
    }
    window.identity.store(hwnd as usize, Ordering::Release);
    if ready.take().is_none_or(|ready| ready.send(Ok(())).is_err()) {
        return Ok(());
    }
    let mut message = MSG::default();
    loop {
        match unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) } {
            -1 => return Err(io::Error::last_os_error()),
            0 => return Ok(()),
            _ => unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            },
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let pointer = THREAD_UI.try_with(Cell::get).unwrap_or(null());
    let Some(ui) = (unsafe { pointer.as_ref() }) else {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    };
    match message {
        CALLBACK
            if matches!(
                (lparam as u32) & 0xffff,
                WM_CONTEXTMENU | WM_RBUTTONUP | KEY_SELECT
            ) =>
        {
            ui.popup(hwnd);
        }
        WM_COMMAND if wparam >> 16 == 0 => match wparam {
            MENU_OPEN => ui.open_dashboard(hwnd),
            MENU_AUTOSTART => {
                if let Some(startup) = &ui.startup
                    && let Err(error) = startup.toggle()
                {
                    eprintln!("Could not update Windows login startup: {error}");
                }
            }
            MENU_QUIT => ui.request_quit(),
            _ => (),
        },
        WM_CLOSE => unsafe {
            EndMenu();
            PostQuitMessage(0);
        },
        WM_ENDSESSION if wparam != 0 => {
            ui.request_quit();
            unsafe {
                EndMenu();
                PostQuitMessage(0);
            }
        }
        event if ui.taskbar_created != 0 && event == ui.taskbar_created => {
            ui.add_icon(hwnd);
        }
        _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
    0
}

impl Ui {
    fn request_quit(&self) {
        if let Some(quit) = self.quit.take() {
            let _ = quit.send(());
        }
    }

    fn add_icon(&self, hwnd: HWND) -> bool {
        let mut data = identity(hwnd);
        let mut tip = [0; 128];
        for (slot, value) in tip
            .iter_mut()
            .take(127)
            .zip(format!("9Router - Port {}", self.port).encode_utf16())
        {
            *slot = value;
        }
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = CALLBACK;
        data.hIcon = self.icon.handle;
        data.szTip = tip;
        if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
            return false;
        }
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) };
        true
    }

    fn popup(&self, hwnd: HWND) {
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            return;
        }
        let status = wide(&format!("9Router (Port {})", self.port));
        let open = wide("Open Dashboard");
        let quit = wide("Quit");
        let (startup_label, startup_flags) =
            match self.startup.as_ref().and_then(|entry| entry.enabled().ok()) {
                Some(true) => ("Disable Auto-start", MF_STRING),
                Some(false) => ("Enable Auto-start", MF_STRING),
                None => ("Auto-start unavailable", MF_STRING | MF_GRAYED),
            };
        let startup_label = wide(startup_label);
        unsafe {
            AppendMenuW(menu, MF_STRING | MF_GRAYED, MENU_STATUS, status.as_ptr());
            AppendMenuW(menu, MF_STRING, MENU_OPEN, open.as_ptr());
            AppendMenuW(menu, startup_flags, MENU_AUTOSTART, startup_label.as_ptr());
            AppendMenuW(menu, MF_STRING, MENU_QUIT, quit.as_ptr());
            let mut point = POINT::default();
            if GetCursorPos(&mut point) != 0 {
                SetForegroundWindow(hwnd);
                TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, 0, hwnd, null());
                PostMessageW(hwnd, WM_NULL, 0, 0);
            }
            DestroyMenu(menu);
        }
    }

    fn open_dashboard(&self, hwnd: HWND) {
        let operation = wide("open");
        let outcome = unsafe {
            ShellExecuteW(
                hwnd,
                operation.as_ptr(),
                self.dashboard.as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL,
            )
        } as usize;
        if outcome <= 32 {
            eprintln!("Could not open the 9Router dashboard (Windows error {outcome})");
        }
    }
}

fn identity(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    }
}

struct RegisteredUi;

impl RegisteredUi {
    fn install(ui: &Ui) -> Self {
        THREAD_UI.with(|slot| slot.set(std::ptr::from_ref(ui)));
        Self
    }
}

impl Drop for RegisteredUi {
    fn drop(&mut self) {
        let _ = THREAD_UI.try_with(|slot| slot.set(null()));
    }
}

struct Window {
    hwnd: HWND,
    identity: Arc<AtomicUsize>,
}

impl Drop for Window {
    fn drop(&mut self) {
        self.identity.store(0, Ordering::Release);
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &identity(self.hwnd));
            DestroyWindow(self.hwnd);
        }
    }
}

struct WindowClass {
    name: Vec<u16>,
    instance: HINSTANCE,
}

impl WindowClass {
    fn register() -> io::Result<Self> {
        let name = wide(CLASS_NAME);
        let instance = unsafe { GetModuleHandleW(null()) };
        let definition = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            lpszClassName: name.as_ptr(),
            hInstance: instance,
            ..Default::default()
        };
        if instance.is_null() || unsafe { RegisterClassW(&definition) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { name, instance })
    }
}

impl Drop for WindowClass {
    fn drop(&mut self) {
        unsafe { UnregisterClassW(self.name.as_ptr(), self.instance) };
    }
}

struct Icon {
    handle: HICON,
    owned: bool,
}

impl Icon {
    fn load() -> Self {
        if let Some(bytes) = icon_resource() {
            let aligned: Vec<u32> = bytes
                .chunks(4)
                .map(|chunk| {
                    let mut number = [0; 4];
                    number[..chunk.len()].copy_from_slice(chunk);
                    u32::from_le_bytes(number)
                })
                .collect();
            let handle = unsafe {
                CreateIconFromResourceEx(
                    aligned.as_ptr().cast(),
                    bytes.len() as u32,
                    1,
                    0x0003_0000,
                    GetSystemMetrics(SM_CXSMICON),
                    GetSystemMetrics(SM_CYSMICON),
                    0,
                )
            };
            if !handle.is_null() {
                return Self {
                    handle,
                    owned: true,
                };
            }
        }
        Self {
            handle: unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) },
            owned: false,
        }
    }
}

impl Drop for Icon {
    fn drop(&mut self) {
        if self.owned {
            unsafe { DestroyIcon(self.handle) };
        }
    }
}

fn icon_resource() -> Option<&'static [u8]> {
    let ico = include_bytes!("../assets/tray/tray-icon.ico");
    let size = u32::from_le_bytes(ico.get(14..18)?.try_into().ok()?) as usize;
    let offset = u32::from_le_bytes(ico.get(18..22)?.try_into().ok()?) as usize;
    ico.get(offset..offset.checked_add(size)?)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
