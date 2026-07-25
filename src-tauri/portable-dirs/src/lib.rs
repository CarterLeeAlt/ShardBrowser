use std::path::PathBuf;

#[cfg(windows)]
fn executable_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
}

/// Portable replacement for `dirs::config_dir()` on Windows; standard
/// platform location on macOS/Linux.
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        return executable_dir();
    }

    #[cfg(not(windows))]
    {
        dirs_upstream::config_dir()
    }
}

/// Portable replacement for `dirs::data_dir()` on Windows; standard
/// platform location on macOS/Linux.
pub fn data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        return executable_dir();
    }

    #[cfg(not(windows))]
    {
        dirs_upstream::data_dir()
    }
}
