//! System tray for Windows (notification area).
//!
//! A message-only window receives the tray callbacks; its queue is drained
//! from the GTK main loop with `PeekMessageW`, so everything (including the
//! popup menu) runs on the UI thread and no GObject has to cross threads.

use std::{cell::Cell, cell::RefCell, time::Duration};

use gtk::{glib, prelude::*};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DispatchMessageW, GetCursorPos, LoadIconW, MF_STRING, MSG, PM_REMOVE, PeekMessageW,
                RegisterClassW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
                TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
                WM_COMMAND, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW,
            },
        },
    },
    core::{PCWSTR, w},
};

use crate::{tray_menu, window::Window};

const TRAY_ID: u32 = 1;
const WM_TRAY: u32 = WM_APP + 1;
const MENU_OPEN: usize = tray_menu::OPEN_ID as usize;
const MENU_QUIT: usize = tray_menu::QUIT_ID as usize;

thread_local! {
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
    static TRAY_HWND: Cell<isize> = const { Cell::new(0) };
}

/// Register the tray icon. Fails softly.
pub(crate) fn setup(window: &Window) {
    if let Err(e) = try_setup(window) {
        log::warn!("system tray unavailable: {e}");
    }
}

fn try_setup(window: &Window) -> Result<(), Box<dyn std::error::Error>> {
    WINDOW.with(|slot| slot.replace(Some(window.clone())));

    unsafe {
        let instance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
        let class_name = w!("LanMouseTrayWindow");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&class);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("Lan Mouse"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        TRAY_HWND.with(|h| h.set(hwnd.0 as isize));

        // Resource id 1 is the icon embedded by build.rs (winresource).
        let icon = LoadIconW(Some(instance.into()), PCWSTR(1 as *const u16))?;

        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ID,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: icon,
            ..Default::default()
        };
        let tip = "Lan Mouse";
        for (dst, src) in data
            .szTip
            .iter_mut()
            .zip(tip.encode_utf16().chain(std::iter::once(0)))
        {
            *dst = src;
        }
        if Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
            log::info!("system tray registered");
        } else {
            log::warn!("Shell_NotifyIconW(NIM_ADD) failed");
        }

        // Drain the tray window's queue from the GTK main loop. `PeekMessageW`
        // never blocks; `GetMessageW` would freeze the whole UI.
        glib::timeout_add_local(Duration::from_millis(50), move || {
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            glib::ControlFlow::Continue
        });
    }

    Ok(())
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TRAY => {
            // The low word of lParam carries the mouse message.
            match (lparam.0 & 0xffff) as u32 {
                WM_LBUTTONUP => present_window(),
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match wparam.0 & 0xffff {
                MENU_OPEN => present_window(),
                MENU_QUIT => quit(),
                _ => {}
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn present_window() {
    WINDOW.with(|slot| {
        if let Some(window) = slot.borrow().as_ref() {
            window.set_visible(true);
            window.present();
        }
    });
}

fn quit() {
    let hwnd = TRAY_HWND.with(|h| h.get());
    if hwnd != 0 {
        unsafe {
            let data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: HWND(hwnd as *mut _),
                uID: TRAY_ID,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }
    WINDOW.with(|slot| {
        if let Some(window) = slot.borrow().as_ref() {
            if let Some(app) = window.application() {
                app.activate_action(tray_menu::QUIT_ACTION, None);
            }
        }
    });
}

fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, MENU_OPEN, w!("Open Lan Mouse"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Quit Lan Mouse"));

        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);

        // Required so the menu closes when the user clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
    }
}
