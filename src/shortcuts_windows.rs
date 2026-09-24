//! Windows: глобальный хоткей, меню в области уведомлений и канал команд
//! внутри той же сессии.
use crate::recorder::Recorder;
use anyhow::{bail, Result};
use std::ptr::{null, null_mut};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::{
            RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
        },
        Shell::{
            Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
        },
        WindowsAndMessaging::*,
    },
};

pub const SAVE: u32 = WM_APP + 1;
pub const QUIT: u32 = WM_APP + 2;
pub const CONFIGURE: u32 = WM_APP + 3;
const OPEN_FOLDER: u32 = WM_APP + 4;
const TRAY: u32 = WM_APP + 5;
const HOTKEY: i32 = 1;

#[derive(Clone, Default)]
pub struct ShortcutState {
    pub trigger: Option<String>,
    pub error: Option<String>,
    pub registered: bool,
}
impl ShortcutState {
    pub fn summary(&self) -> String {
        self.error
            .clone()
            .or_else(|| self.trigger.clone())
            .unwrap_or_else(|| "регистрируется…".into())
    }
}

pub struct Shortcuts {
    state: Arc<Mutex<ShortcutState>>,
    quit: Arc<AtomicBool>,
    window: usize,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Shortcuts {
    pub fn spawn(rec: Arc<Recorder>) -> Self {
        let state = Arc::new(Mutex::new(ShortcutState::default()));
        let quit = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let st = state.clone();
        let done = quit.clone();
        let thread = std::thread::spawn(move || {
            if let Err(e) = run(rec, st.clone(), done, tx) {
                st.lock().unwrap().error = Some(format!("Windows: {e:#}"));
                log::error!("Windows integration: {e:#}");
            }
        });
        let window = rx.recv().unwrap_or(0);
        Self {
            state,
            quit,
            window,
            thread: Some(thread),
        }
    }
    pub fn state(&self) -> ShortcutState {
        self.state.lock().unwrap().clone()
    }
    pub fn should_quit(&self) -> bool {
        self.quit.load(Ordering::Acquire)
    }
    pub fn available(&self) -> bool {
        self.window != 0
    }
    pub fn open_settings(&self) {
        if self.window != 0 {
            // SAFETY: handle belongs to our integration thread; no pointers in message.
            unsafe {
                PostMessageW(self.window as HWND, CONFIGURE, 0, 0);
            }
        }
    }
}
impl Drop for Shortcuts {
    fn drop(&mut self) {
        if self.window != 0 {
            // SAFETY: message requests destruction on the window's own thread.
            unsafe {
                PostMessageW(self.window as HWND, WM_CLOSE, 0, 0);
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn window_name() -> Vec<u16> {
    // A per-user name; Windows additionally isolates windows by interactive session.
    wide(&format!(
        "replay-rs commands: {}",
        crate::config::config_dir().display()
    ))
}
pub fn send_command(command: u32) -> Result<()> {
    let name = window_name();
    // SAFETY: valid UTF-16 string; messages carry no addresses or external data.
    unsafe {
        let hwnd = FindWindowW(null(), name.as_ptr());
        if hwnd.is_null() {
            bail!("replay-rs не запущен в этой сессии");
        }
        if PostMessageW(hwnd, command, 0, 0) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}
pub fn notify(title: &str, body: &str) {
    // SAFETY: both strings are NUL-terminated and kept alive for the call.
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(body).as_ptr(),
            wide(title).as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: called by Windows on the owner thread with a live window handle.
    unsafe {
        if msg == TRAY && (l as u32 == WM_RBUTTONUP || l as u32 == WM_LBUTTONUP) {
            let menu = CreatePopupMenu();
            if !menu.is_null() {
                AppendMenuW(
                    menu,
                    MF_STRING,
                    SAVE as usize,
                    wide("Сохранить реплей").as_ptr(),
                );
                AppendMenuW(
                    menu,
                    MF_STRING,
                    OPEN_FOLDER as usize,
                    wide("Папка с клипами").as_ptr(),
                );
                AppendMenuW(menu, MF_SEPARATOR, 0, null());
                AppendMenuW(menu, MF_STRING, QUIT as usize, wide("Выход").as_ptr());
                let mut point: POINT = std::mem::zeroed();
                GetCursorPos(&mut point);
                SetForegroundWindow(hwnd);
                let selected = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_NONOTIFY,
                    point.x,
                    point.y,
                    0,
                    hwnd,
                    null(),
                );
                DestroyMenu(menu);
                if selected != 0 {
                    PostMessageW(hwnd, selected as u32, 0, 0);
                }
                PostMessageW(hwnd, WM_NULL, 0, 0);
            }
            return 0;
        }
        if msg == WM_CLOSE {
            PostQuitMessage(0);
            return 0;
        }
        DefWindowProcW(hwnd, msg, w, l)
    }
}
fn run(
    rec: Arc<Recorder>,
    state: Arc<Mutex<ShortcutState>>,
    quit: Arc<AtomicBool>,
    ready: mpsc::SyncSender<usize>,
) -> Result<()> {
    // SAFETY: all UI resources are created, used and destroyed on this thread.
    // All Win32 strings and structs remain alive for their corresponding calls.
    unsafe {
        let instance = GetModuleHandleW(null());
        let class = wide("ReplayRsCommands");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            window_name().as_ptr(),
            0,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            UnregisterClassW(class.as_ptr(), instance);
            return Err(std::io::Error::last_os_error().into());
        }
        let registered = RegisterHotKey(
            hwnd,
            HOTKEY,
            MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
            b'S' as u32,
        ) != 0;
        {
            let mut st = state.lock().unwrap();
            st.registered = registered;
            if registered {
                st.trigger = Some("Ctrl+Alt+S".into());
            } else {
                st.error =
                    Some("Ctrl+Alt+S занят или недоступен; используйте кнопку сохранения".into());
            }
        }
        let mut icon: NOTIFYICONDATAW = std::mem::zeroed();
        icon.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        icon.hWnd = hwnd;
        icon.uID = 1;
        icon.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        icon.uCallbackMessage = TRAY;
        icon.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
        let tip = wide("replay-rs — Ctrl+Alt+S");
        icon.szTip[..tip.len()].copy_from_slice(&tip);
        if Shell_NotifyIconW(NIM_ADD, &icon) == 0 {
            log::warn!("не удалось добавить иконку в трей");
        }
        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        let _ = ready.send(hwnd as usize);
        let mut message: MSG = std::mem::zeroed();
        loop {
            let result = GetMessageW(&mut message, null_mut(), 0, 0);
            if result <= 0 {
                break;
            }
            match message.message {
                SAVE | WM_HOTKEY => {
                    let rec = rec.clone();
                    std::thread::spawn(move || match rec.save(None) {
                        Ok(outcome) => log::info!("сохранение: {outcome:?}"),
                        Err(e) => log::error!("сохранение не удалось: {e:#}"),
                    });
                }
                QUIT => {
                    quit.store(true, Ordering::Release);
                }
                CONFIGURE => {
                    std::thread::spawn(|| {
                        notify("Хоткей replay-rs", "Windows: Ctrl+Alt+S. В этой версии комбинация фиксирована. Если она занята, используйте кнопку сохранения или replay-rs.exe --save.")
                    });
                }
                OPEN_FOLDER => {
                    let _ = crate::platform::open_path(&rec.config().output);
                }
                msg if msg == taskbar_created && taskbar_created != 0 => {
                    Shell_NotifyIconW(NIM_ADD, &icon);
                }
                _ => {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
        Shell_NotifyIconW(NIM_DELETE, &icon);
        if registered {
            UnregisterHotKey(hwnd, HOTKEY);
        }
        DestroyWindow(hwnd);
        UnregisterClassW(class.as_ptr(), instance);
    }
    Ok(())
}
