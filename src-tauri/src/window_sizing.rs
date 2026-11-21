use std::io;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MINMAXINFO, SM_CXPADDEDBORDER, SM_CXSIZEFRAME, WM_GETMINMAXINFO, WM_NCDESTROY,
};

pub(crate) fn honor_client_minimum(window: HWND) -> io::Result<()> {
    // The main window is created and subclassed on the same UI thread.
    if unsafe { SetWindowSubclass(window, Some(window_proc), 1, 0) } == 0 {
        return Err(io::Error::other(
            "cannot install the main window sizing handler",
        ));
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        unsafe { RemoveWindowSubclass(window, Some(window_proc), id) };
    }
    let result = unsafe { DefSubclassProc(window, message, wparam, lparam) };
    if message == WM_GETMINMAXINFO && lparam != 0 {
        let dpi = unsafe { GetDpiForWindow(window) };
        if dpi != 0 {
            // Tao supplies the client minimum but excludes the shadow insets
            // from its tracking size for an undecorated resizable window.
            let inset = unsafe { GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) }
                + unsafe { GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) };
            let bounds = unsafe { &mut *(lparam as *mut MINMAXINFO) };
            bounds.ptMinTrackSize.x += 2 * inset;
        }
    }
    result
}
