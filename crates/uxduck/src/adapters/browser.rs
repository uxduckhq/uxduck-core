//! Opens a URL in a browser (UXDuck's own pages preferring a Chromium one,
//! `app_browser`), and a folder in the file manager.

use std::process::{Command, Stdio};

/// Best effort: failure is reported, never fatal.
///
/// # Errors
/// When no browser could be started.
pub fn open(url: &str) -> std::io::Result<()> {
    super::app_browser::open(url)
}

/// Shows a folder in Finder, Explorer or the desktop's file manager, with a
/// file in it selected where the platform can.
///
/// # Errors
/// When the file manager cannot be started.
pub fn reveal(folder: &std::path::Path, file: Option<&std::path::Path>) -> std::io::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        match file {
            Some(file) => command.arg("-R").arg(file),
            None => command.arg(folder),
        };
        command
    } else if cfg!(windows) {
        let mut command = Command::new("explorer");
        match file {
            Some(file) => command.arg(format!("/select,{}", file.display())),
            None => command.arg(folder),
        };
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(folder);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}
