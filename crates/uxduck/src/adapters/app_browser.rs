//! Which browser opens UXDuck's own pages (ADR-0025 D2). The app is built and
//! tested for Chromium browsers (Chrome, Edge, Brave, Arc and the like), so
//! when the default browser is another one (Safari, Firefox) and a Chromium
//! browser is installed, UXDuck's pages open there. Links to anything else
//! keep the default browser. `uxduck` and `uxduck-tray` both include this
//! file by path, so it uses only std and `serde_json`.
//!
//! - macOS: the default when it is Chromium, else the first of `CHROMIUM`
//!   installed.
//! - Windows: the default when it is Chromium, else Edge, which Windows
//!   always has.
//! - Linux: the default when it is Chromium, else the first of `CHROMIUM` on
//!   `PATH`.

use std::process::{Command, Stdio};

/// A Chromium browser UXDuck's pages open in, and its name for the page
/// that offers "Open in Chrome".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chromium {
    pub name: String,
    open: How,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum How {
    /// It is the default browser.
    Default,
    /// macOS: `open -b <bundle id>`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Bundle(String),
    /// Windows: the `microsoft-edge:` protocol.
    #[cfg_attr(not(windows), allow(dead_code))]
    Edge,
    /// Linux: run it with the address.
    #[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
    Program(String),
}

/// Chromium browsers on macOS, in the order one is chosen when the default
/// browser is not one: bundle identifier, app, name.
#[cfg(target_os = "macos")]
const CHROMIUM: &[(&str, &str, &str)] = &[
    ("com.google.Chrome", "Google Chrome.app", "Chrome"),
    ("com.brave.Browser", "Brave Browser.app", "Brave"),
    ("com.microsoft.edgemac", "Microsoft Edge.app", "Edge"),
    ("company.thebrowser.Browser", "Arc.app", "Arc"),
    ("com.vivaldi.Vivaldi", "Vivaldi.app", "Vivaldi"),
    ("org.chromium.Chromium", "Chromium.app", "Chromium"),
    ("com.google.Chrome.beta", "Google Chrome Beta.app", "Chrome"),
    ("com.google.Chrome.dev", "Google Chrome Dev.app", "Chrome"),
    (
        "com.google.Chrome.canary",
        "Google Chrome Canary.app",
        "Chrome",
    ),
    ("com.brave.Browser.beta", "Brave Browser Beta.app", "Brave"),
    (
        "com.brave.Browser.nightly",
        "Brave Browser Nightly.app",
        "Brave",
    ),
    (
        "com.microsoft.edgemac.Beta",
        "Microsoft Edge Beta.app",
        "Edge",
    ),
    (
        "com.microsoft.edgemac.Dev",
        "Microsoft Edge Dev.app",
        "Edge",
    ),
];

/// Chromium browsers on Linux, in the order one is chosen: program, name.
#[cfg(all(unix, not(target_os = "macos")))]
const CHROMIUM: &[(&str, &str)] = &[
    ("google-chrome", "Chrome"),
    ("brave-browser", "Brave"),
    ("microsoft-edge", "Edge"),
    ("chromium", "Chromium"),
    ("chromium-browser", "Chromium"),
];

/// A Chromium browser's name from its bundle identifier, `ProgId` or
/// desktop file, including ones not listed (Opera, Dia, other channels);
/// `None` for any other browser.
#[cfg_attr(not(any(target_os = "macos", windows, unix)), allow(dead_code))]
fn chromium_name(id: &str) -> Option<&'static str> {
    let id = id.to_ascii_lowercase();
    [
        ("brave", "Brave"),
        ("edge", "Edge"),
        ("msedge", "Edge"),
        ("thebrowser", "Arc"),
        ("vivaldi", "Vivaldi"),
        ("opera", "Opera"),
        ("chromium", "Chromium"),
        ("chrome", "Chrome"),
    ]
    .iter()
    .find(|(part, _)| id.contains(part))
    .map(|(_, name)| *name)
}

/// The default browser's bundle identifier, from Launch Services' per-user
/// handlers; `None` when none was ever chosen, which means Safari.
#[cfg(target_os = "macos")]
fn default_browser() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let plist = std::path::Path::new(&home)
        .join("Library/Preferences/com.apple.LaunchServices/com.apple.launchservices.secure.plist");
    let output = Command::new("plutil")
        .args(["-extract", "LSHandlers", "json", "-o", "-"])
        .arg(plist)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let handlers: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    ["https", "http"].iter().find_map(|scheme| {
        handlers.as_array()?.iter().find_map(|handler| {
            (handler["LSHandlerURLScheme"].as_str() == Some(*scheme))
                .then(|| handler["LSHandlerRoleAll"].as_str().map(str::to_owned))
                .flatten()
        })
    })
}

/// The default browser's `ProgId`, such as `ChromeHTML`, `MSEdgeHTM` or
/// `FirefoxURL-308046B0AF4A39CB`.
#[cfg(windows)]
fn default_browser() -> Option<String> {
    use std::os::windows::process::CommandExt as _;
    let output = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https\UserChoice",
            "/v",
            "ProgId",
        ])
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("ProgId"))
        .and_then(|rest| rest.trim().strip_prefix("REG_SZ"))
        .map(|id| id.trim().to_owned())
}

/// The default browser's desktop file, such as `google-chrome.desktop`.
#[cfg(all(unix, not(target_os = "macos")))]
fn default_browser() -> Option<String> {
    let output = Command::new("xdg-settings")
        .args(["get", "default-web-browser"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned()).filter(|id| !id.is_empty())
}

/// Whether an app is installed where apps are kept.
#[cfg(target_os = "macos")]
fn installed(app: &str) -> bool {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    std::iter::once(std::path::PathBuf::from("/Applications"))
        .chain(home.map(|home| home.join("Applications")))
        .any(|dir| dir.join(app).is_dir())
}

/// Whether a program is on `PATH`.
#[cfg(all(unix, not(target_os = "macos")))]
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// The Chromium browser UXDuck's pages open in, if there is one. On
/// Windows there always is: Edge.
#[must_use]
#[cfg_attr(windows, allow(clippy::unnecessary_wraps))]
pub fn chromium() -> Option<Chromium> {
    if let Some(name) = default_browser().as_deref().and_then(chromium_name) {
        return Some(Chromium {
            name: name.into(),
            open: How::Default,
        });
    }
    #[cfg(target_os = "macos")]
    {
        CHROMIUM
            .iter()
            .find(|(_, app, _)| installed(app))
            .map(|(id, _, name)| Chromium {
                name: (*name).into(),
                open: How::Bundle((*id).into()),
            })
    }
    #[cfg(windows)]
    {
        Some(Chromium {
            name: "Edge".into(),
            open: How::Edge,
        })
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        CHROMIUM
            .iter()
            .find(|(program, _)| on_path(program))
            .map(|(program, name)| Chromium {
                name: (*name).into(),
                open: How::Program((*program).into()),
            })
    }
}

impl Chromium {
    /// Opens a URL in this browser.
    ///
    /// # Errors
    /// When the browser could not be started.
    pub fn open(&self, url: &str) -> std::io::Result<()> {
        match &self.open {
            How::Default => default(url),
            How::Bundle(id) => {
                let status = Command::new("open")
                    .args(["-b", id, url])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()?;
                if status.success() {
                    Ok(())
                } else {
                    Err(std::io::Error::other(format!("{} did not open", self.name)))
                }
            }
            How::Edge => default(&format!("microsoft-edge:{url}")),
            How::Program(program) => Command::new(program)
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map(drop),
        }
    }
}

/// Opens a URL. One of UXDuck's own pages opens in the Chromium browser
/// `chromium` finds, else in the default browser, whose page then says
/// which browsers UXDuck works in. Any other URL opens in the default
/// browser.
///
/// # Errors
/// When no browser could be started.
pub fn open(url: &str) -> std::io::Result<()> {
    if is_own(url)
        && let Some(browser) = chromium()
        && browser.open(url).is_ok()
    {
        return Ok(());
    }
    default(url)
}

/// The platform's default browser.
fn default(url: &str) -> std::io::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(url);
        command
    } else if cfg!(windows) {
        // `start` would split the URL at `&`.
        let mut command = Command::new("rundll32");
        command.args(["url.dll,FileProtocolHandler", url]);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}

/// Whether a URL is one of UXDuck's own pages: served on this computer.
fn is_own(url: &str) -> bool {
    ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"]
        .iter()
        .any(|prefix| url.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_pages_on_this_computer_are_uxduck_s_own() {
        assert!(is_own("http://127.0.0.1:45000/?bootstrap=x"));
        assert!(is_own("http://localhost:5173/home"));
        assert!(!is_own("https://uxduck.com/docs?from=tray"));
        assert!(!is_own("http://127.0.0.1.example.com/"));
    }

    #[test]
    fn chromium_browsers_are_told_by_their_identifier() {
        for (id, name) in [
            ("com.google.Chrome", "Chrome"),
            ("com.google.chrome.canary", "Chrome"),
            ("com.brave.Browser", "Brave"),
            ("com.microsoft.edgemac", "Edge"),
            ("company.thebrowser.Browser", "Arc"),
            ("com.operasoftware.Opera", "Opera"),
            ("org.chromium.Chromium", "Chromium"),
            ("ChromeHTML", "Chrome"),
            ("MSEdgeHTM", "Edge"),
            ("BraveHTML", "Brave"),
            ("google-chrome.desktop", "Chrome"),
            ("chromium-browser.desktop", "Chromium"),
            ("microsoft-edge.desktop", "Edge"),
        ] {
            assert_eq!(chromium_name(id), Some(name), "{id}");
        }
        for other in [
            "com.apple.Safari",
            "org.mozilla.firefox",
            "FirefoxURL-308046B0AF4A39CB",
            "firefox.desktop",
            "org.gnome.Epiphany.desktop",
        ] {
            assert_eq!(chromium_name(other), None, "{other}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn every_listed_mac_browser_is_chromium() {
        for (id, _, name) in CHROMIUM {
            assert_eq!(chromium_name(id), Some(*name), "{id}");
        }
    }
}
