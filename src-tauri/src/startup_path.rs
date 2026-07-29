use std::path::{Path, PathBuf};
use std::io::Write;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
};

const INVALID_PATH_EXIT_CODE: i32 = 2;

/// Validate the directory that contains the launcher before Tauri, WebView2,
/// profile storage, or the browser runtime starts touching the filesystem.
pub(crate) fn enforce_valid_launcher_directory_or_exit() {
    match current_launcher_directory() {
        Ok(directory) if is_valid_launcher_directory(&directory) => {
            if let Err(error) = ensure_portable_storage_writable(&directory) {
                show_unwritable_path_and_exit(&directory, &error);
            }
        }
        Ok(directory) => show_invalid_path_and_exit(Some(&directory), None),
        Err(error) => show_invalid_path_and_exit(None, Some(&error)),
    }
}

/// The launcher is intentionally portable: every persistent byte, including
/// WebView2 state, must live beside the executable. Refuse to start instead of
/// silently falling back to AppData when that location is read-only.
fn ensure_portable_storage_writable(directory: &Path) -> Result<(), String> {
    let root = directory.join("shardx-launcher");
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("无法创建便携数据目录 {}：{error}", root.display()))?;
    let probe = root.join(format!(
        ".write-probe-{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
        output.write_all(b"ShardX portable storage probe")?;
        output.sync_all()?;
        drop(output);
        std::fs::remove_file(&probe)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&probe);
    }
    result.map_err(|error| {
        format!(
            "启动器所在目录不可写，无法使用便携数据目录 {}：{error}",
            root.display()
        )
    })
}

fn current_launcher_directory() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("无法获取启动器路径：{error}"))?;
    executable
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "启动器路径没有父目录".to_string())
}

/// Windows drive/UNC separators are path syntax. Every other character in the
/// launcher directory must be an ASCII letter, digit, space, hyphen, or
/// underscore. The executable file name itself is intentionally excluded
/// because release filenames also contain dots.
fn is_valid_launcher_directory(directory: &Path) -> bool {
    let value = directory.as_os_str().to_string_lossy();
    // `current_exe()` can return the Windows extended-length prefix. It is
    // path syntax, not part of any directory name, so do not reject its `?`.
    let value = value.strip_prefix(r"\\?\").unwrap_or(&value);
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, ' ' | '-' | '_' | ':' | '\\' | '/')
        })
}

fn show_invalid_path_and_exit(directory: Option<&Path>, detail: Option<&str>) -> ! {
    let location = directory
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "无法读取".to_string());
    let detail = detail
        .map(|message| format!("\n\n错误详情：{message}"))
        .unwrap_or_default();
    let message = format!(
        "启动器所在目录的路径不合法。\n\n\
         当前目录：{location}\n\n\
         路径中的每一级目录名称只能包含英文字母（A-Z、a-z）、数字（0-9）、\
         空格、连字符（-）和下划线（_），不能包含中文或其他符号。\n\n\
         软件将彻底退出。请将启动器移动到合法路径（例如 C:\\ShardX 123\\Launcher_x64）后重新启动。\
         {detail}"
    );

    show_error_message(&message, "ShardX Launcher - 路径不合法");
    std::process::exit(INVALID_PATH_EXIT_CODE);
}

fn show_unwritable_path_and_exit(directory: &Path, detail: &str) -> ! {
    let message = format!(
        "启动器所在目录没有写入权限。\n\n\
         当前目录：{}\n\n\
         ShardX Launcher 是纯绿色软件，浏览器 Runtime、Profiles、Cookies、设置和 WebView2 数据都必须保存在启动器旁边，不会写入 AppData 或其他目录。\n\n\
         软件将彻底退出。请把启动器移动到当前用户可读写的目录后重新启动，例如 D:\\ShardX Launcher。\n\n\
         错误详情：{detail}",
        directory.display()
    );
    show_error_message(&message, "ShardX Launcher - 目录不可写");
    std::process::exit(INVALID_PATH_EXIT_CODE);
}

pub(crate) fn show_fatal_startup_error_and_exit(detail: &str) -> ! {
    let message = format!(
        "ShardX Launcher 启动失败，软件将彻底退出。\n\n错误详情：{detail}"
    );
    show_error_message(&message, "ShardX Launcher - 启动失败");
    std::process::exit(1);
}

fn show_error_message(message: &str, title: &str) {
    let message_wide = to_wide(message);
    let title_wide = to_wide(title);
    unsafe {
        MessageBoxW(
            0 as HWND,
            message_wide.as_ptr(),
            title_wide.as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::is_valid_launcher_directory;
    use std::path::Path;

    #[test]
    fn accepts_supported_ascii_characters_in_directory_names() {
        assert!(is_valid_launcher_directory(Path::new(
            r"C:\Users\PZWang\Desktop\ShardX123\Launcher9"
        )));
        assert!(is_valid_launcher_directory(Path::new(
            r"C:\Program Files\ShardX Launcher"
        )));
        assert!(is_valid_launcher_directory(Path::new(
            r"C:\Shard-X\Launcher_x64"
        )));
        assert!(is_valid_launcher_directory(Path::new(r"C:\ShardX123")));
        assert!(is_valid_launcher_directory(Path::new(r"C:\")));
        assert!(is_valid_launcher_directory(Path::new(
            r"\\?\C:\ShardX123\Launcher9"
        )));
    }

    #[test]
    fn rejects_unsupported_directory_characters() {
        for directory in [
            r"C:\Shard.X",
            r"C:\浏览器\ShardX",
            r"C:\ShardX!",
        ] {
            assert!(
                !is_valid_launcher_directory(Path::new(directory)),
                "directory should be rejected: {directory}"
            );
        }
    }
}
