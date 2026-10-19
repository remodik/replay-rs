//! Оверлей на Windows: отдельное layered-окно поверх остальных.
//!
//! Обычное окно с `WS_EX_TOPMOST` — без внедрения в процесс игры и без
//! хуков Direct3D, поэтому античиты на него не реагируют, но и поверх
//! всего оно не гарантировано: поверх игр в окне без рамки (borderless) и
//! игр, которым Windows 10/11 включает «оптимизацию полноэкранного режима»,
//! оно видно; поверх настоящего эксклюзивного полноэкранного режима
//! (старые D3D9/D3D11 без flip-модели) — нет. Окно не берёт фокус
//! (`WS_EX_NOACTIVATE`), не появляется на панели задач (`WS_EX_TOOLWINDOW`),
//! а в режиме «клики насквозь» пропускает мышь (`WS_EX_TRANSPARENT`).
//!
//! Кадр — 32-битный DIB с premultiplied alpha через `UpdateLayeredWindow`:
//! тот же буфер, что рисует `raster`. Масштаб DPI не учитывается (кадр
//! 1:1 в пикселях). На оборудовании не проверялось — только сборка.

use std::cell::RefCell;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MsgWaitForMultipleObjects,
    PeekMessageW, RegisterClassW, ShowWindow, SystemParametersInfoW, TranslateMessage,
    UpdateLayeredWindow, MA_NOACTIVATE, MSG, PM_REMOVE, QS_ALLINPUT, SPI_GETWORKAREA,
    SW_SHOWNOACTIVATE, ULW_ALPHA, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE,
    WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};

use super::{place, Availability, Ticker, TICK};
use crate::config::OverlayConfig;
use crate::recorder::Recorder;
use crate::service::CaptureService;

/// Событие мыши из оконной процедуры: (нажата ли кнопка, x, y), `None` у
/// кнопки — просто движение.
type MouseEvent = (Option<bool>, f32, f32);

thread_local! {
    /// Оконная процедура вызывается в этом же потоке из DispatchMessage.
    static MOUSE: RefCell<Vec<MouseEvent>> = const { RefCell::new(Vec::new()) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let x = (lp & 0xffff) as i16 as f32;
    let y = ((lp >> 16) & 0xffff) as i16 as f32;
    match msg {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        WM_MOUSEMOVE => MOUSE.with(|m| m.borrow_mut().push((None, x, y))),
        WM_LBUTTONDOWN => MOUSE.with(|m| m.borrow_mut().push((Some(true), x, y))),
        WM_LBUTTONUP => MOUSE.with(|m| m.borrow_mut().push((Some(false), x, y))),
        _ => {}
    }
    // SAFETY: стандартная обработка остальных сообщений.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

struct OverlayWindow {
    hwnd: HWND,
    cfg: OverlayConfig,
}

impl OverlayWindow {
    fn create(cfg: &OverlayConfig) -> Result<Self> {
        let class = wide("replay-rs-overlay");
        // SAFETY: строки живут до конца вызовов; класс регистрируется один раз
        // на поток (повторная регистрация вернёт 0, это не ошибка).
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            let mut ex = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
            if !cfg.interactive {
                ex |= WS_EX_TRANSPARENT;
            }
            let title = wide("replay-rs");
            let hwnd = CreateWindowExW(
                ex,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                bail!("CreateWindowExW не создал окно оверлея");
            }
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            Ok(Self {
                hwnd,
                cfg: cfg.clone(),
            })
        }
    }

    /// Выводит кадр в окно и ставит его в угол рабочей области.
    fn present(&self, canvas: &super::raster::Canvas) -> Result<()> {
        let (w, h) = (canvas.width as i32, canvas.height as i32);
        let mut work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        // SAFETY: SPI_GETWORKAREA пишет RECT по указателю.
        unsafe {
            SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work as *mut RECT).cast(), 0);
        }
        let (x, y) = place(
            self.cfg.corner,
            (work.right - work.left, work.bottom - work.top),
            (w as u32, h as u32),
        );
        let dst = POINT {
            x: work.left + x,
            y: work.top + y,
        };
        // SAFETY: DIB-секция живёт до DeleteObject; копируем ровно w*h*4 байт
        // в буфер, который CreateDIBSection выделил под этот размер.
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            let mem = CreateCompatibleDC(screen);
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                // Отрицательная высота — строки сверху вниз, как в буфере.
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(
                mem,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            if dib.is_null() || bits.is_null() {
                DeleteDC(mem);
                ReleaseDC(std::ptr::null_mut(), screen);
                bail!("CreateDIBSection не выделил кадр");
            }
            let bytes = canvas.to_bytes();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), bits.cast::<u8>(), bytes.len());
            let old = SelectObject(mem, dib);
            let size = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let ok = UpdateLayeredWindow(
                self.hwnd, screen, &dst, &size, mem, &src, 0, &blend, ULW_ALPHA,
            );
            SelectObject(mem, old);
            DeleteObject(dib);
            DeleteDC(mem);
            ReleaseDC(std::ptr::null_mut(), screen);
            if ok == 0 {
                bail!("UpdateLayeredWindow не обновил оверлей");
            }
        }
        Ok(())
    }
}

impl Drop for OverlayWindow {
    fn drop(&mut self) {
        // SAFETY: окно создано в этом потоке.
        unsafe {
            DestroyWindow(self.hwnd);
        }
    }
}

/// Разбирает сообщения окна, ожидая их не дольше `timeout_ms`.
fn pump(timeout_ms: u32) {
    // SAFETY: стандартный цикл сообщений потока.
    unsafe {
        MsgWaitForMultipleObjects(0, std::ptr::null(), 0, timeout_ms, QS_ALLINPUT);
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

pub fn run(
    rec: &Arc<Recorder>,
    service: &Arc<CaptureService>,
    wake: &Receiver<()>,
    availability: &Arc<Mutex<Availability>>,
) -> Result<()> {
    *availability.lock().expect("состояние оверлея отравлено") = Availability::Overlay;
    let mut ticker = Ticker::new(rec);
    let mut window: Option<OverlayWindow> = None;
    loop {
        match wake.try_recv() {
            Ok(()) => ticker.invalidate(),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Ok(()),
        }
        let cfg = rec.config().overlay;
        if !cfg.enabled {
            return Ok(());
        }
        if !ticker.visible(rec, &cfg) {
            if window.take().is_some() {
                ticker.invalidate();
            }
        } else if window.as_ref().is_none_or(|w| {
            w.cfg.corner != cfg.corner
                || w.cfg.interactive != cfg.interactive
                || w.cfg.compact != cfg.compact
        }) {
            // Старое окно — до создания нового: два оверлея не мигают рядом.
            drop(window.take());
            window = Some(OverlayWindow::create(&cfg)?);
            ticker.invalidate();
        }
        let events = MOUSE.with(|m| std::mem::take(&mut *m.borrow_mut()));
        let force = !events.is_empty();
        for (button, x, y) in events {
            match button {
                None => ticker.renderer.pointer_moved(x, y),
                Some(pressed) => ticker.renderer.pointer_button(x, y, pressed),
            }
        }
        if let Some(w) = &window {
            if let Some(canvas) = ticker.frame(rec, service, &cfg, 1.0, force) {
                w.present(&canvas)?;
            }
        }
        pump(TICK.as_millis() as u32);
    }
}
