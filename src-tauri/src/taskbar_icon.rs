use anyhow::Result;
use std::path::{Path, PathBuf};
use unicode_segmentation::UnicodeSegmentation;

/// The taskbar badge is the last one to four user-perceived characters from NAME.
/// Short names are never padded, so `A`, `AB`, `ABC`, and `ABCD` stay exactly that.
pub(crate) fn badge_label(name: &str) -> String {
    let mut graphemes: Vec<&str> = UnicodeSegmentation::graphemes(name, true)
        .rev()
        .take(4)
        .collect();
    graphemes.reverse();
    graphemes.concat()
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn prepare_profile_binary(
    original: &Path,
    _profile_id: &str,
    _profile_name: &str,
) -> Result<PathBuf> {
    Ok(original.to_path_buf())
}

#[cfg(target_os = "windows")]
pub(crate) fn prepare_profile_binary(
    original: &Path,
    profile_id: &str,
    profile_name: &str,
) -> Result<PathBuf> {
    windows::prepare_profile_binary(original, profile_id, profile_name)
}

pub(crate) fn remove_profile_launchers(profile_id: &str) {
    #[cfg(target_os = "windows")]
    windows::remove_profile_launchers(profile_id);
    #[cfg(not(target_os = "windows"))]
    let _ = profile_id;
}

pub(crate) fn watch_profile_taskbar(pid: u32, profile_id: String, executable: PathBuf) {
    #[cfg(target_os = "windows")]
    windows::watch_profile_taskbar(pid, profile_id, executable);
    #[cfg(not(target_os = "windows"))]
    let _ = (pid, profile_id, executable);
}

#[cfg(target_os = "windows")]
mod windows {
    use super::badge_label;
    use anyhow::{Context, Result};
    use ico::{IconDir, IconDirEntry, IconImage, ResourceType};
    use image::{imageops::FilterType, RgbaImage};
    use std::ffi::{c_void, OsStr, OsString};
    use std::fs;
    use std::io::Cursor;
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::ptr::{copy_nonoverlapping, null_mut};
    use std::slice;
    use std::sync::OnceLock;
    use std::time::{Instant, UNIX_EPOCH};

    const BASE_ICON_PNG: &[u8] = include_bytes!("../icons/shardx-browser-taskbar-base.png");
    const INTER_GDI_FONT_TTF: &[u8] = include_bytes!("../fonts/Inter-Variable-GDI.ttf");
    const ICON_SIZE: i32 = 256;
    const ICON_SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];
    const RT_ICON: *const u16 = 3usize as *const u16;
    const RT_GROUP_ICON: *const u16 = 14usize as *const u16;
    const DIB_RGB_COLORS: u32 = 0;
    const BI_RGB: u32 = 0;
    const TRANSPARENT: i32 = 1;
    const FW_BOLD: i32 = 700;
    const DEFAULT_CHARSET: u32 = 1;
    const OUT_TT_PRECIS: u32 = 4;
    const CLIP_DEFAULT_PRECIS: u32 = 0;
    const ANTIALIASED_QUALITY: u32 = 4;
    const DEFAULT_PITCH: u32 = 0;
    const WM_SETICON: u32 = 0x0080;
    const IMAGE_ICON: u32 = 1;
    const LR_LOADFROMFILE: u32 = 0x0010;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const VT_LPWSTR: u16 = 31;
    const COINIT_MULTITHREADED: u32 = 0;

    // AddFontMemResourceEx keeps the memory font available until it is
    // explicitly removed. The bytes are compiled into the executable and the
    // process-lifetime handle is intentionally retained, so taskbar rendering
    // never depends on Inter being installed in Windows.
    static INTER_GDI_FONT_REGISTRATION: OnceLock<std::result::Result<isize, String>> =
        OnceLock::new();

    #[repr(C)]
    struct BitmapInfoHeader {
        size: u32,
        width: i32,
        height: i32,
        planes: u16,
        bit_count: u16,
        compression: u32,
        size_image: u32,
        x_pels_per_meter: i32,
        y_pels_per_meter: i32,
        clr_used: u32,
        clr_important: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RgbQuad {
        blue: u8,
        green: u8,
        red: u8,
        reserved: u8,
    }

    #[repr(C)]
    struct BitmapInfo {
        header: BitmapInfoHeader,
        colors: [RgbQuad; 1],
    }

    #[repr(C)]
    #[derive(Default)]
    struct Size {
        cx: i32,
        cy: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    #[repr(C)]
    struct PropertyKey {
        format_id: Guid,
        property_id: u32,
    }

    #[repr(C)]
    struct PropVariant {
        variant_type: u16,
        reserved1: u16,
        reserved2: u16,
        reserved3: u16,
        string_value: *const u16,
    }

    #[repr(C)]
    struct PropertyStore {
        vtable: *const PropertyStoreVTable,
    }

    #[repr(C)]
    struct PropertyStoreVTable {
        query_interface: unsafe extern "system" fn(
            *mut PropertyStore,
            *const Guid,
            *mut *mut c_void,
        ) -> i32,
        add_ref: unsafe extern "system" fn(*mut PropertyStore) -> u32,
        release: unsafe extern "system" fn(*mut PropertyStore) -> u32,
        get_count: unsafe extern "system" fn(*mut PropertyStore, *mut u32) -> i32,
        get_at:
            unsafe extern "system" fn(*mut PropertyStore, u32, *mut PropertyKey) -> i32,
        get_value: unsafe extern "system" fn(
            *mut PropertyStore,
            *const PropertyKey,
            *mut PropVariant,
        ) -> i32,
        set_value: unsafe extern "system" fn(
            *mut PropertyStore,
            *const PropertyKey,
            *const PropVariant,
        ) -> i32,
        commit: unsafe extern "system" fn(*mut PropertyStore) -> i32,
    }

    const IID_PROPERTY_STORE: Guid = Guid {
        data1: 0x886d8eeb,
        data2: 0x8cf2,
        data3: 0x4446,
        data4: [0x8d, 0x02, 0xcd, 0xba, 0x1d, 0xbd, 0xcf, 0x99],
    };

    const APP_USER_MODEL_FORMAT: Guid = Guid {
        data1: 0x9f4c2855,
        data2: 0x9f79,
        data3: 0x4b39,
        data4: [0xa8, 0xd0, 0xe1, 0xd4, 0x2d, 0xe1, 0xd5, 0xf3],
    };

    #[link(name = "kernel32")]
    extern "system" {
        fn BeginUpdateResourceW(file_name: *const u16, delete_existing: i32) -> isize;
        fn UpdateResourceW(
            update: isize,
            resource_type: *const u16,
            name: *const u16,
            language: u16,
            data: *const c_void,
            size: u32,
        ) -> i32;
        fn EndUpdateResourceW(update: isize, discard: i32) -> i32;
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
        fn QueryFullProcessImageNameW(
            process: isize,
            flags: u32,
            executable_name: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn CloseHandle(object: isize) -> i32;
    }

    #[link(name = "gdi32")]
    extern "system" {
        fn AddFontMemResourceEx(
            font_data: *const c_void,
            data_size: u32,
            reserved: *mut c_void,
            font_count: *mut u32,
        ) -> isize;
        fn CreateCompatibleDC(dc: isize) -> isize;
        fn DeleteDC(dc: isize) -> i32;
        fn CreateDIBSection(
            dc: isize,
            info: *const BitmapInfo,
            usage: u32,
            bits: *mut *mut c_void,
            section: isize,
            offset: u32,
        ) -> isize;
        fn CreateFontW(
            height: i32,
            width: i32,
            escapement: i32,
            orientation: i32,
            weight: i32,
            italic: u32,
            underline: u32,
            strike_out: u32,
            char_set: u32,
            out_precision: u32,
            clip_precision: u32,
            quality: u32,
            pitch_and_family: u32,
            face_name: *const u16,
        ) -> isize;
        fn SelectObject(dc: isize, object: isize) -> isize;
        fn DeleteObject(object: isize) -> i32;
        fn SetBkMode(dc: isize, mode: i32) -> i32;
        fn SetTextColor(dc: isize, color: u32) -> u32;
        fn GetTextExtentPoint32W(dc: isize, text: *const u16, len: i32, size: *mut Size) -> i32;
        fn TextOutW(dc: isize, x: i32, y: i32, text: *const u16, len: i32) -> i32;
    }

    type EnumWindowsCallback = unsafe extern "system" fn(isize, isize) -> i32;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(callback: Option<EnumWindowsCallback>, param: isize) -> i32;
        fn GetWindowThreadProcessId(window: isize, process_id: *mut u32) -> u32;
        fn LoadImageW(
            instance: isize,
            name: *const u16,
            image_type: u32,
            width: i32,
            height: i32,
            flags: u32,
        ) -> isize;
        fn SendMessageW(window: isize, message: u32, wparam: usize, lparam: isize) -> isize;
        fn DestroyIcon(icon: isize) -> i32;
    }

    #[link(name = "shell32")]
    extern "system" {
        fn SHGetPropertyStoreForWindow(
            window: isize,
            interface_id: *const Guid,
            property_store: *mut *mut PropertyStore,
        ) -> i32;
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, concurrency_model: u32) -> i32;
        fn CoUninitialize();
    }

    pub(super) fn prepare_profile_binary(
        original: &Path,
        profile_id: &str,
        profile_name: &str,
    ) -> Result<PathBuf> {
        if !profile_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            anyhow::bail!("invalid profile id for taskbar launcher");
        }

        let parent = original
            .parent()
            .context("browser executable has no parent directory")?;
        let metadata = fs::metadata(original).context("read browser executable metadata")?;
        let label = badge_label(profile_name);
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let fingerprint = stable_hash(&[
            label.as_bytes(),
            BASE_ICON_PNG,
            b"taskbar-badge-layout-v10-inter-gdi-antialiased",
            &metadata.len().to_le_bytes(),
            &modified.to_le_bytes(),
        ]);
        let file_name = format!("shardx-profile-{profile_id}-{fingerprint:016x}.exe");
        let target = parent.join(&file_name);
        if target.exists() {
            // The sidecar is the live-window icon source. Loading an icon from
            // an EXE with LoadImageW + LR_LOADFROMFILE is unsupported and was
            // the main reason the watcher silently gave up, leaving the shell
            // free to show Chromium's default/cached icon.
            let icon_path = profile_icon_path(&target);
            if !icon_path.exists() {
                let icon = build_badged_icon(&label)?;
                write_icon_sidecar(&icon_path, &icon)?;
            }
            cleanup_stale_launchers(parent, profile_id, Some(&target));
            return Ok(target);
        }

        let temporary = parent.join(format!(
            ".{file_name}.{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let _ = fs::remove_file(&temporary);
        fs::copy(original, &temporary).with_context(|| {
            format!(
                "create portable taskbar launcher {} from {}",
                temporary.display(),
                original.display()
            )
        })?;

        let result = (|| -> Result<()> {
            let icon = build_badged_icon(&label)?;
            patch_main_icon(&temporary, &icon)?;
            match fs::rename(&temporary, &target) {
                Ok(()) => Ok(()),
                Err(_) if target.exists() => {
                    let _ = fs::remove_file(&temporary);
                    Ok(())
                }
                Err(error) => Err(error).context("publish portable taskbar launcher"),
            }?;
            write_icon_sidecar(&profile_icon_path(&target), &icon)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;

        cleanup_stale_launchers(parent, profile_id, Some(&target));
        Ok(target)
    }

    pub(super) fn remove_profile_launchers(profile_id: &str) {
        if !profile_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return;
        }
        let Ok(original) = crate::runtime::binary_path() else {
            return;
        };
        let Some(parent) = original.parent() else {
            return;
        };
        cleanup_stale_launchers(parent, profile_id, None);
    }

    pub(super) fn watch_profile_taskbar(pid: u32, profile_id: String, executable: PathBuf) {
        if pid == 0 {
            return;
        }
        tokio::spawn(async move {
            let icon_path = profile_icon_path(&executable);
            let icons = match LoadedIcons::from_icon_file(&icon_path) {
                Ok(icons) => icons,
                Err(error) => {
                    eprintln!(
                        "[launcher] cannot load taskbar icons for {profile_id}: {error:#}"
                    );
                    return;
                }
            };
            let app_id = profile_app_id(&profile_id, &executable);
            let app_id = wide_null(OsStr::new(&app_id));
            let mut icon_resource = OsString::from(icon_path.as_os_str());
            icon_resource.push(",0");
            let icon_resource = wide_null(&icon_resource);
            let started = Instant::now();

            loop {
                // Match both the initially spawned PID and any top-level
                // window owned by the same per-profile executable. Chromium
                // can hand startup off to another process, so PID-only
                // matching made the icon work on some launches but not others.
                let matched = apply_profile_windows(
                    pid,
                    &executable,
                    &icons,
                    &app_id,
                    &icon_resource,
                );
                let tracked = crate::process::Tracker::shared().is_running_pid(pid);
                if !tracked
                    && matched == 0
                    && started.elapsed() >= std::time::Duration::from_secs(15)
                {
                    break;
                }
                let delay = if started.elapsed() < std::time::Duration::from_secs(5) {
                    std::time::Duration::from_millis(100)
                } else {
                    std::time::Duration::from_secs(1)
                };
                tokio::time::sleep(delay).await;
            }
        });
    }

    struct LoadedIcons {
        large: isize,
        small: isize,
    }

    impl LoadedIcons {
        fn from_icon_file(icon_file: &Path) -> Result<Self> {
            let path = wide_null(icon_file.as_os_str());
            unsafe {
                // Load native taskbar/window sizes from the multi-resolution
                // ICO. Loading 256px and letting Windows shrink it made both
                // the logo and the tiny NAME label visibly soft.
                let large = LoadImageW(0, path.as_ptr(), IMAGE_ICON, 32, 32, LR_LOADFROMFILE);
                let small = LoadImageW(0, path.as_ptr(), IMAGE_ICON, 16, 16, LR_LOADFROMFILE);
                if large == 0 || small == 0 {
                    if large != 0 {
                        DestroyIcon(large);
                    }
                    if small != 0 {
                        DestroyIcon(small);
                    }
                    return Err(std::io::Error::last_os_error())
                        .context("load generated profile taskbar icon");
                }
                Ok(Self { large, small })
            }
        }
    }

    impl Drop for LoadedIcons {
        fn drop(&mut self) {
            unsafe {
                DestroyIcon(self.large);
                DestroyIcon(self.small);
            }
        }
    }

    fn profile_app_id(profile_id: &str, executable: &Path) -> String {
        let sanitized: String = profile_id
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '.'
                }
            })
            .collect();
        let executable_name = executable
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(profile_id);
        let icon_identity = stable_hash(&[executable_name.as_bytes()]);
        format!("ProxyShard.ShardX.{sanitized}.{icon_identity:016x}")
    }

    struct WindowApplyContext {
        pid: u32,
        executable: *const PathBuf,
        large_icon: isize,
        small_icon: isize,
        app_id: *const u16,
        icon_resource: *const u16,
        matched_windows: usize,
    }

    fn apply_profile_windows(
        pid: u32,
        executable: &PathBuf,
        icons: &LoadedIcons,
        app_id: &[u16],
        icon_resource: &[u16],
    ) -> usize {
        let mut context = WindowApplyContext {
            pid,
            executable,
            large_icon: icons.large,
            small_icon: icons.small,
            app_id: app_id.as_ptr(),
            icon_resource: icon_resource.as_ptr(),
            matched_windows: 0,
        };
        unsafe {
            let com_result = CoInitializeEx(null_mut(), COINIT_MULTITHREADED);
            EnumWindows(
                Some(apply_window_callback),
                (&mut context as *mut WindowApplyContext) as isize,
            );
            if com_result >= 0 {
                CoUninitialize();
            }
        }
        context.matched_windows
    }

    unsafe extern "system" fn apply_window_callback(window: isize, param: isize) -> i32 {
        let context = &*(param as *const WindowApplyContext);
        let mut window_pid = 0u32;
        GetWindowThreadProcessId(window, &mut window_pid);
        if window_pid != context.pid
            && !process_uses_executable(window_pid, &*context.executable)
        {
            return 1;
        }

        set_window_taskbar_properties(window, context.app_id, context.icon_resource);
        SendMessageW(window, WM_SETICON, 1, context.large_icon);
        SendMessageW(window, WM_SETICON, 0, context.small_icon);
        (*(param as *mut WindowApplyContext)).matched_windows += 1;
        1
    }

    unsafe fn process_uses_executable(process_id: u32, expected: &Path) -> bool {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if process == 0 {
            return false;
        }
        let mut path = vec![0u16; 32_768];
        let mut length = path.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut length) != 0;
        CloseHandle(process);
        if !ok {
            return false;
        }
        path.truncate(length as usize);
        let actual = PathBuf::from(OsString::from_wide(&path));
        actual
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected.to_string_lossy())
    }

    unsafe fn set_window_taskbar_properties(
        window: isize,
        app_id: *const u16,
        icon_resource: *const u16,
    ) {
        let mut store: *mut PropertyStore = null_mut();
        if SHGetPropertyStoreForWindow(window, &IID_PROPERTY_STORE, &mut store) < 0
            || store.is_null()
        {
            return;
        }

        let id_key = PropertyKey {
            format_id: APP_USER_MODEL_FORMAT,
            property_id: 5,
        };
        let icon_key = PropertyKey {
            format_id: APP_USER_MODEL_FORMAT,
            property_id: 3,
        };
        let id_value = PropVariant {
            variant_type: VT_LPWSTR,
            reserved1: 0,
            reserved2: 0,
            reserved3: 0,
            string_value: app_id,
        };
        let icon_value = PropVariant {
            variant_type: VT_LPWSTR,
            reserved1: 0,
            reserved2: 0,
            reserved3: 0,
            string_value: icon_resource,
        };
        let vtable = &*(*store).vtable;
        (vtable.set_value)(store, &id_key, &id_value);
        (vtable.set_value)(store, &icon_key, &icon_value);
        (vtable.commit)(store);
        (vtable.release)(store);
    }

    fn stable_hash(parts: &[&[u8]]) -> u64 {
        // FNV-1a keeps the generated executable path stable across app restarts.
        let mut hash = 0xcbf29ce484222325u64;
        for part in parts {
            for byte in *part {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
        hash
    }

    fn cleanup_stale_launchers(parent: &Path, profile_id: &str, keep: Option<&Path>) {
        let prefix = format!("shardx-profile-{profile_id}-");
        let keep_icon = keep.map(profile_icon_path);
        let Ok(entries) = fs::read_dir(parent) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if keep.is_some_and(|keep| path.as_path() == keep) {
                continue;
            }
            if keep_icon
                .as_ref()
                .is_some_and(|keep| path.as_path() == keep.as_path())
            {
                continue;
            }
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if name.starts_with(&prefix)
                && (name.ends_with(".exe") || name.ends_with(".ico"))
            {
                // A still-running previous launcher remains locked by Windows and
                // simply survives until the next launch-time cleanup.
                let _ = fs::remove_file(path);
            }
        }
    }

    fn profile_icon_path(executable: &Path) -> PathBuf {
        executable.with_extension("ico")
    }

    fn write_icon_sidecar(icon_path: &Path, icon: &[u8]) -> Result<()> {
        let parent = icon_path.parent().context("profile icon has no parent directory")?;
        let file_name = icon_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("profile icon has an invalid file name")?;
        let temporary = parent.join(format!(
            ".{file_name}.{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        fs::write(&temporary, icon).context("write temporary profile taskbar icon")?;
        match fs::rename(&temporary, icon_path) {
            Ok(()) => Ok(()),
            Err(_) if icon_path.exists() => {
                let _ = fs::remove_file(&temporary);
                Ok(())
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                Err(error).context("publish profile taskbar icon")
            }
        }
    }

    fn build_badged_icon(label: &str) -> Result<Vec<u8>> {
        let base = IconImage::read_png(Cursor::new(BASE_ICON_PNG))
            .context("decode bundled ShardX browser icon")?;
        if base.width() != ICON_SIZE as u32 || base.height() != ICON_SIZE as u32 {
            anyhow::bail!("bundled ShardX browser icon must be 256x256");
        }
        let base = RgbaImage::from_raw(
            ICON_SIZE as u32,
            ICON_SIZE as u32,
            base.into_rgba_data(),
        )
        .context("construct bundled ShardX browser image")?;
        let mut directory = IconDir::new(ResourceType::Icon);
        for size in ICON_SIZES {
            // Render each Windows icon size independently. In particular, the
            // 16/20/24/32px entries get native text pixels instead of a second
            // shell downscale from the 256px source.
            let mut rgba = if size == ICON_SIZE as u32 {
                base.clone().into_raw()
            } else {
                image::imageops::resize(&base, size, size, FilterType::Lanczos3).into_raw()
            };
            if !label.is_empty() {
                render_badge(&mut rgba, label, size as i32)?;
            }
            let image = IconImage::from_rgba_data(size, size, rgba);
            directory.add_entry(IconDirEntry::encode_as_png(&image)?);
        }
        let mut bytes = Cursor::new(Vec::new());
        directory.write(&mut bytes)?;
        Ok(bytes.into_inner())
    }

    fn render_badge(rgba: &mut [u8], label: &str, icon_size: i32) -> Result<()> {
        let text: Vec<u16> = label.encode_utf16().collect();
        if text.is_empty() {
            return Ok(());
        }
        ensure_inter_gdi_font()?;
        // Keep the taskbar badge aligned with the launcher's bundled Inter UI
        // typography. This family comes from INTER_GDI_FONT_TTF above rather
        // than from the host's system font directory.
        let face_name = wide_null(OsStr::new("Inter"));

        unsafe {
            let dc = CreateCompatibleDC(0);
            if dc == 0 {
                return Err(std::io::Error::last_os_error()).context("create badge drawing context");
            }

            // Full-width, borderless label: use every available taskbar pixel
            // for NAME instead of spending space on side margins and a white
            // outline. Inter has taller/wider metrics than the former font, so
            // use one fixed aspect ratio at every icon size. This keeps up to
            // four wide characters inside the badge without per-label spacing
            // compression.
            let badge_height = scaled(132, icon_size);
            let font_height = scaled(80, icon_size).max(5);
            let font_width = scaled(36, icon_size).max(1);
            let radius = scaled(20, icon_size).max(1);
            let left = 0;
            let top = icon_size - badge_height;
            let right = icon_size;
            let bottom = icon_size;
            fill_rounded_rect(
                rgba,
                icon_size,
                left,
                top,
                right,
                bottom,
                radius,
                [3, 8, 15, 252],
            );

            let mut info: BitmapInfo = zeroed();
            info.header.size = size_of::<BitmapInfoHeader>() as u32;
            info.header.width = icon_size;
            info.header.height = -icon_size; // top-down rows
            info.header.planes = 1;
            info.header.bit_count = 32;
            info.header.compression = BI_RGB;
            info.header.size_image = (icon_size * icon_size * 4) as u32;

            let mut bits: *mut c_void = null_mut();
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, 0, 0);
            if bitmap == 0 || bits.is_null() {
                DeleteDC(dc);
                return Err(std::io::Error::last_os_error()).context("create badge bitmap");
            }

            let mut bgra = Vec::with_capacity(rgba.len());
            for pixel in rgba.chunks_exact(4) {
                bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            }
            copy_nonoverlapping(bgra.as_ptr(), bits.cast::<u8>(), bgra.len());

            let old_bitmap = SelectObject(dc, bitmap);
            let font = match create_badge_font(font_height, font_width, &face_name) {
                Ok(font) => font,
                Err(error) => {
                    SelectObject(dc, old_bitmap);
                    DeleteObject(bitmap);
                    DeleteDC(dc);
                    return Err(error);
                }
            };
            let old_font = SelectObject(dc, font);
            let mut final_size = Size::default();
            if GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut final_size) == 0 {
                SelectObject(dc, old_font);
                DeleteObject(font);
                SelectObject(dc, old_bitmap);
                DeleteObject(bitmap);
                DeleteDC(dc);
                return Err(std::io::Error::last_os_error()).context("measure final badge text");
            }
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, 0x00ff_ffff);
            let text_x = (icon_size - final_size.cx) / 2;
            let text_y = top + (badge_height - final_size.cy) / 2;
            let drawn = TextOutW(dc, text_x, text_y, text.as_ptr(), text.len() as i32);

            let rendered = slice::from_raw_parts(bits.cast::<u8>(), rgba.len());
            for (index, pixel) in rendered.chunks_exact(4).enumerate() {
                let offset = index * 4;
                rgba[offset] = pixel[2];
                rgba[offset + 1] = pixel[1];
                rgba[offset + 2] = pixel[0];
                rgba[offset + 3] = pixel[3];
            }
            // GDI does not maintain alpha for glyph pixels. The whole badge is
            // intentionally opaque, so restore alpha for its rounded footprint.
            force_rounded_alpha(
                rgba,
                icon_size,
                left,
                top,
                right,
                bottom,
                radius,
            );

            SelectObject(dc, old_font);
            DeleteObject(font);
            SelectObject(dc, old_bitmap);
            DeleteObject(bitmap);
            DeleteDC(dc);

            if drawn == 0 {
                return Err(std::io::Error::last_os_error()).context("draw taskbar badge text");
            }
        }
        Ok(())
    }

    fn ensure_inter_gdi_font() -> Result<()> {
        let registration = INTER_GDI_FONT_REGISTRATION.get_or_init(|| {
            let data_size = match u32::try_from(INTER_GDI_FONT_TTF.len()) {
                Ok(size) => size,
                Err(_) => return Err("bundled Inter GDI font is too large".to_string()),
            };
            let mut font_count = 0u32;
            let handle = unsafe {
                AddFontMemResourceEx(
                    INTER_GDI_FONT_TTF.as_ptr().cast::<c_void>(),
                    data_size,
                    null_mut(),
                    &mut font_count,
                )
            };
            if handle == 0 || font_count == 0 {
                Err(format!(
                    "register bundled Inter GDI font: {}",
                    std::io::Error::last_os_error()
                ))
            } else {
                Ok(handle)
            }
        });

        match registration {
            Ok(_) => Ok(()),
            Err(message) => anyhow::bail!(message.clone()),
        }
    }

    unsafe fn create_badge_font(height: i32, width: i32, face_name: &[u16]) -> Result<isize> {
        let font = CreateFontW(
            -height,
            width,
            0,
            0,
            FW_BOLD,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_TT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            DEFAULT_PITCH,
            face_name.as_ptr(),
        );
        if font == 0 {
            return Err(std::io::Error::last_os_error()).context("create taskbar badge font");
        }
        Ok(font)
    }

    fn fill_rounded_rect(
        rgba: &mut [u8],
        icon_size: i32,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        radius: i32,
        color: [u8; 4],
    ) {
        for y in top.max(0)..bottom.min(icon_size) {
            for x in left.max(0)..right.min(icon_size) {
                if inside_rounded_rect(x, y, left, top, right, bottom, radius) {
                    let offset = ((y * icon_size + x) * 4) as usize;
                    rgba[offset..offset + 4].copy_from_slice(&color);
                }
            }
        }
    }

    fn force_rounded_alpha(
        rgba: &mut [u8],
        icon_size: i32,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        radius: i32,
    ) {
        for y in top.max(0)..bottom.min(icon_size) {
            for x in left.max(0)..right.min(icon_size) {
                if inside_rounded_rect(x, y, left, top, right, bottom, radius) {
                    let offset = ((y * icon_size + x) * 4 + 3) as usize;
                    rgba[offset] = 255;
                }
            }
        }
    }

    fn inside_rounded_rect(
        x: i32,
        y: i32,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        radius: i32,
    ) -> bool {
        if x < left || x >= right || y < top || y >= bottom {
            return false;
        }
        let nearest_x = x.clamp(left + radius, right - radius - 1);
        let nearest_y = y.clamp(top + radius, bottom - radius - 1);
        let dx = x - nearest_x;
        let dy = y - nearest_y;
        dx * dx + dy * dy <= radius * radius
    }

    fn scaled(value_at_256: i32, icon_size: i32) -> i32 {
        (value_at_256 * icon_size + ICON_SIZE / 2) / ICON_SIZE
    }

    fn patch_main_icon(executable: &Path, icon: &[u8]) -> Result<()> {
        let entries = parse_ico(icon)?;
        let group = build_group_icon(&entries);
        let executable_wide = wide_null(executable.as_os_str());
        let group_name = wide_null(OsStr::new("IDR_MAINFRAME"));

        unsafe {
            let update = BeginUpdateResourceW(executable_wide.as_ptr(), 0);
            if update == 0 {
                return Err(std::io::Error::last_os_error()).context("open profile launcher resources");
            }

            let result = (|| -> Result<()> {
                for language in [0u16, 1033u16] {
                    for entry in &entries {
                        if UpdateResourceW(
                            update,
                            RT_ICON,
                            entry.resource_id as usize as *const u16,
                            language,
                            entry.data.as_ptr().cast(),
                            entry.data.len() as u32,
                        ) == 0
                        {
                            return Err(std::io::Error::last_os_error())
                                .context("write profile launcher icon image");
                        }
                    }
                    if UpdateResourceW(
                        update,
                        RT_GROUP_ICON,
                        group_name.as_ptr(),
                        language,
                        group.as_ptr().cast(),
                        group.len() as u32,
                    ) == 0
                    {
                        return Err(std::io::Error::last_os_error())
                            .context("write profile launcher icon group");
                    }
                }
                Ok(())
            })();

            if result.is_err() {
                EndUpdateResourceW(update, 1);
                return result;
            }
            if EndUpdateResourceW(update, 0) == 0 {
                return Err(std::io::Error::last_os_error()).context("commit profile launcher icon");
            }
        }
        Ok(())
    }

    struct IcoEntry {
        width: u8,
        height: u8,
        color_count: u8,
        reserved: u8,
        planes: u16,
        bit_count: u16,
        resource_id: u16,
        data: Vec<u8>,
    }

    fn parse_ico(bytes: &[u8]) -> Result<Vec<IcoEntry>> {
        if bytes.len() < 6 || read_u16(bytes, 0)? != 0 || read_u16(bytes, 2)? != 1 {
            anyhow::bail!("generated taskbar icon is not a valid ICO file");
        }
        let count = read_u16(bytes, 4)? as usize;
        let directory_end = 6usize
            .checked_add(count.checked_mul(16).context("ICO directory overflow")?)
            .context("ICO directory overflow")?;
        if bytes.len() < directory_end {
            anyhow::bail!("truncated ICO directory");
        }

        let mut entries = Vec::with_capacity(count);
        for index in 0..count {
            let offset = 6 + index * 16;
            let size = read_u32(bytes, offset + 8)? as usize;
            let image_offset = read_u32(bytes, offset + 12)? as usize;
            let end = image_offset.checked_add(size).context("ICO image overflow")?;
            let data = bytes
                .get(image_offset..end)
                .context("truncated ICO image")?
                .to_vec();
            entries.push(IcoEntry {
                width: bytes[offset],
                height: bytes[offset + 1],
                color_count: bytes[offset + 2],
                reserved: bytes[offset + 3],
                planes: read_u16(bytes, offset + 4)?,
                bit_count: read_u16(bytes, offset + 6)?,
                resource_id: 5000 + index as u16,
                data,
            });
        }
        Ok(entries)
    }

    fn build_group_icon(entries: &[IcoEntry]) -> Vec<u8> {
        let mut group = Vec::with_capacity(6 + entries.len() * 14);
        group.extend_from_slice(&0u16.to_le_bytes());
        group.extend_from_slice(&1u16.to_le_bytes());
        group.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            group.push(entry.width);
            group.push(entry.height);
            group.push(entry.color_count);
            group.push(entry.reserved);
            group.extend_from_slice(&entry.planes.to_le_bytes());
            group.extend_from_slice(&entry.bit_count.to_le_bytes());
            group.extend_from_slice(&(entry.data.len() as u32).to_le_bytes());
            group.extend_from_slice(&entry.resource_id.to_le_bytes());
        }
        group
    }

    fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
        let value: [u8; 2] = bytes
            .get(offset..offset + 2)
            .context("truncated ICO integer")?
            .try_into()
            .expect("slice length checked");
        Ok(u16::from_le_bytes(value))
    }

    fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
        let value: [u8; 4] = bytes
            .get(offset..offset + 4)
            .context("truncated ICO integer")?
            .try_into()
            .expect("slice length checked");
        Ok(u32::from_le_bytes(value))
    }

    fn wide_null(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    #[cfg(test)]
    mod tests {
        use super::{build_badged_icon, parse_ico, ICON_SIZES, INTER_GDI_FONT_TTF};

        #[test]
        fn generated_icon_contains_every_native_windows_size() {
            let icon = build_badged_icon("1111").expect("generate test taskbar icon");
            let entries = parse_ico(&icon).expect("parse generated taskbar icon");
            let sizes: Vec<u32> = entries
                .iter()
                .map(|entry| {
                    assert_eq!(entry.width, entry.height);
                    if entry.width == 0 {
                        256
                    } else {
                        u32::from(entry.width)
                    }
                })
                .collect();
            assert_eq!(sizes.as_slice(), ICON_SIZES.as_slice());
        }

        #[test]
        fn bundled_inter_font_is_true_type() {
            assert_eq!(&INTER_GDI_FONT_TTF[..4], &[0, 1, 0, 0]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::badge_label;

    #[test]
    fn badge_uses_up_to_four_trailing_characters_without_padding() {
        assert_eq!(badge_label("A"), "A");
        assert_eq!(badge_label("ABC"), "ABC");
        assert_eq!(badge_label("ABCD"), "ABCD");
        assert_eq!(badge_label("ABCDE"), "BCDE");
    }

    #[test]
    fn badge_truncation_is_unicode_safe() {
        assert_eq!(badge_label("浏览器"), "浏览器");
        assert_eq!(badge_label("一号浏览器"), "号浏览器");
        assert_eq!(badge_label("A🧩环境"), "A🧩环境");
        assert_eq!(badge_label("五e\u{301}六七八"), "e\u{301}六七八");
        assert_eq!(badge_label("X👨‍👩‍👧‍👦YZ"), "X👨‍👩‍👧‍👦YZ");
    }
}
