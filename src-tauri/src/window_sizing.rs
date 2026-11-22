use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use tauri::{
    utils::config::WindowConfig, AppHandle, Manager, RunEvent, WebviewWindow, WindowEvent,
};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MINMAXINFO, SM_CXPADDEDBORDER, SM_CXSIZEFRAME, WM_GETMINMAXINFO, WM_NCDESTROY,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TableLayout {
    sidebar_width: u32,
    minimum_side_padding: u32,
    scrollbar_width: u32,
    table_border: u32,
    column_gap: u32,
    drag_column: u32,
    select_column: u32,
    actions_column: u32,
    table_inline_padding: u32,
    browser: Table,
    proxy: Table,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Table {
    columns: HashMap<String, u32>,
}

impl TableLayout {
    fn minimum_window_width(&self) -> u32 {
        let table_width = |table: &Table| {
            self.drag_column
                + self.select_column
                + table.columns.values().sum::<u32>()
                + self.actions_column
                + 2 * self.table_inline_padding
                + (table.columns.len() as u32 + 2) * self.column_gap
        };
        table_width(&self.browser).max(table_width(&self.proxy))
            + self.sidebar_width
            + self.scrollbar_width
            + 2 * (self.minimum_side_padding + self.table_border)
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
struct SavedSize {
    width: u32,
    height: u32,
}

impl SavedSize {
    fn read() -> Result<Option<Self>> {
        let path = crate::store::config_root()?.join("window-size.json");
        if !path.try_exists()? {
            return Ok(None);
        }
        let size: Self = crate::store::load_json_with_backup(&path)?;
        anyhow::ensure!(
            size.width != 0 && size.height != 0,
            "invalid saved window size"
        );
        Ok(Some(size))
    }

    fn write(self) -> Result<()> {
        let path = crate::store::config_root()?.join("window-size.json");
        crate::store::atomic_write(&path, &serde_json::to_vec(&self)?)
            .with_context(|| format!("write {}", path.display()))
    }

    fn current(window: &WebviewWindow) -> tauri::Result<Self> {
        let size = window
            .inner_size()?
            .to_logical::<u32>(window.scale_factor()?);
        Ok(Self {
            width: size.width,
            height: size.height,
        })
    }
}

pub(crate) fn configure(config: &mut WindowConfig) -> Result<()> {
    let layout: TableLayout = serde_json::from_str(include_str!("../../src/table-layout.json"))?;
    let minimum = f64::from(layout.minimum_window_width());
    config.min_width = Some(minimum);
    config.width = minimum;
    match SavedSize::read() {
        Ok(Some(size)) => {
            config.width = f64::from(size.width).max(minimum);
            config.height = f64::from(size.height).max(config.min_height.unwrap_or(0.0));
        }
        Ok(None) => {}
        Err(error) => eprintln!("[launcher] cannot restore window size: {error:#}"),
    }
    Ok(())
}

pub(crate) fn remember_size() -> impl FnMut(&AppHandle, RunEvent) {
    let mut opened = None;
    let mut latest = None;
    move |app, event| match event {
        RunEvent::Ready => {
            opened = app.get_webview_window("main").and_then(|window| {
                SavedSize::current(&window)
                    .inspect_err(|error| {
                        eprintln!("[launcher] cannot read initial window size: {error}")
                    })
                    .ok()
            });
        }
        RunEvent::WindowEvent {
            label,
            event: WindowEvent::Resized(_),
            ..
        } if label == "main" => {
            if let Some(window) = app.get_webview_window("main") {
                let record = || -> tauri::Result<Option<SavedSize>> {
                    if window.is_minimized()? || window.is_maximized()? {
                        return Ok(None);
                    }
                    SavedSize::current(&window).map(Some)
                };
                match record() {
                    Ok(Some(size)) => latest = Some(size),
                    Ok(None) => {}
                    Err(error) => eprintln!("[launcher] cannot read resized window size: {error}"),
                }
            }
        }
        RunEvent::Exit => {
            if let Some(size) = latest.filter(|size| Some(*size) != opened) {
                if let Err(error) = size.write() {
                    eprintln!("[launcher] cannot save window size: {error:#}");
                }
            }
        }
        _ => {}
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widest_table_controls_window_minimum() {
        let mut layout: TableLayout = serde_json::from_value(serde_json::json!({
            "sidebarWidth": 200, "minimumSidePadding": 10, "scrollbarWidth": 10,
            "tableBorder": 1, "columnGap": 8, "dragColumn": 24, "selectColumn": 28,
            "actionsColumn": 60, "tableInlinePadding": 8,
            "browser": {"columns": {"name": 100, "status": 40}},
            "proxy": {"columns": {"name": 100, "type": 80}}
        }))
        .unwrap();
        // The proxy table needs 340 pixels; window chrome adds 232.
        assert_eq!(layout.minimum_window_width(), 572);
        *layout.proxy.columns.get_mut("type").unwrap() += 20;
        assert_eq!(layout.minimum_window_width(), 592);
        *layout.browser.columns.get_mut("status").unwrap() += 80;
        assert_eq!(layout.minimum_window_width(), 612);
    }

    #[test]
    fn production_layout_reserves_both_fixed_columns_and_symmetric_padding() {
        let layout: TableLayout =
            serde_json::from_str(include_str!("../../src/table-layout.json")).unwrap();
        assert_eq!(layout.minimum_window_width(), 1286);
    }
}
