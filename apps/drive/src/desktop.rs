//! Drive as a desktop app (feature `desktop`): a menu-bar icon on macOS, a
//! tray icon on Linux (app-shell's tray), with the Drive window a click
//! away. The process stays in the menu bar with no window open, as a
//! cloud drive's does. Quit ends it.
//!
//! - **Open Drive** opens the app's window (the same app the browser
//!   shows, drawn by Blitz).
//! - **Show Drive Folder** opens the Drive tree in the system's file manager:
//!   Finder's Drive (the File Provider, docs/fileprovider.md) on macOS, the
//!   mount (`~/Drive`) on Linux.
//! - **Open at Login** is the platform's own start-at-login (a LaunchAgent,
//!   an XDG autostart entry), which starts the app in the background.
//!
//! On macOS the app also keeps Finder current: it registers the Drive File
//! Provider domain and runs the bundled registrar's `watch`, which signals
//! Finder whenever the tree changes (the extension itself is relaunched by
//! the system too often to watch).

use app_shell_native::tray::{
    self, MenuItem, TrayApp, TrayEvent, TrayIcon, TrayMenu, WindowSpec, autostart,
};
use dioxus::logger::tracing;

/// The app's id: the tray's, the start-at-login entry's, the bundle's.
const ID: &str = "app.fasttrackstudio.drive";

/// Started at login: the tray alone, no window.
const BACKGROUND: &str = "--background";

/// The menu-bar icon (32×32 RGBA, from assets/icons/Drive.webp).
const ICON: &[u8] = include_bytes!("../assets/tray-32.rgba");

fn icon() -> TrayIcon {
    TrayIcon::from_rgba(ICON.to_vec(), 32, 32)
        .unwrap_or_else(|| TrayIcon::disc(32, [0x2f, 0x6f, 0xeb, 0xff]))
}

fn main_window() -> WindowSpec {
    let mut w = WindowSpec::shell("drive", "Drive", crate::App);
    w.size = (1280.0, 820.0);
    w
}

fn menu() -> TrayMenu {
    let at_login = autostart::is_enabled(ID).unwrap_or(false);
    TrayMenu::new()
        .item(MenuItem::new("open", "Open Drive").accelerator("Mod+O"))
        .item(MenuItem::new("folder", "Show Drive Folder"))
        .separator()
        .item(MenuItem::new("login", "Open at Login").checked(at_login))
        .separator()
        .item(MenuItem::new("quit", "Quit Drive").accelerator("Mod+Q"))
}

/// Where the Drive tree is in the system's file manager.
fn drive_folder() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    if cfg!(target_os = "macos") {
        home.join("Library/CloudStorage/Drive-Drive")
    } else {
        home.join("Drive")
    }
}

fn reveal(path: &std::path::Path) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener).arg(path).spawn() {
        tracing::warn!(error = %e, path = %path.display(), "the Drive folder could not be shown");
    }
}

fn on_event(ev: TrayEvent, tray: &tray::Tray) {
    match ev {
        TrayEvent::Menu(id) => match id.as_str() {
            "open" => tray.open_window(main_window()),
            "folder" => reveal(&drive_folder()),
            "login" => {
                let on = !autostart::is_enabled(ID).unwrap_or(false);
                match autostart::Autostart::current(ID, "Drive", &[BACKGROUND])
                    .and_then(|a| autostart::set(&a, on))
                {
                    Ok(()) => tray.update_item("login", |i| i.checked = Some(on)),
                    Err(e) => tracing::warn!(error = %e, "start at login could not be changed"),
                }
            }
            "quit" => {
                file_provider::stop();
                tray.quit();
            }
            _ => {}
        },
        // A click on the icon (Linux, Windows): the window.
        TrayEvent::Activate { .. } => tray.open_window(main_window()),
        TrayEvent::WindowClosed(_) => {}
    }
}

/// Run Drive as a desktop app until Quit.
pub fn run() {
    file_provider::start();
    let background = std::env::args().any(|a| a == BACKGROUND);
    tray::run(TrayApp {
        id: ID.to_owned(),
        name: "Drive".to_owned(),
        icon: icon(),
        menu: menu(),
        start_window: (!background).then(main_window),
        panel: None,
        on_event: Box::new(on_event),
    });
    file_provider::stop();
}

/// The File Provider's registrar, bundled beside this program in Drive.app
/// (`Contents/MacOS/DriveFileProviderDomains`): `sync` registers the
/// domain, `watch` keeps Finder current. Elsewhere: nothing.
mod file_provider {
    use dioxus::logger::tracing;
    use std::process::Child;
    use std::sync::Mutex;

    static WATCH: Mutex<Option<Child>> = Mutex::new(None);

    fn registrar() -> Option<std::path::PathBuf> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let here = std::env::current_exe().ok()?;
        let tool = here.parent()?.join("DriveFileProviderDomains");
        tool.exists().then_some(tool)
    }

    pub fn start() {
        let Some(tool) = registrar() else { return };
        match std::process::Command::new(&tool).arg("sync").status() {
            Ok(s) if s.success() => {}
            Ok(s) => {
                tracing::warn!(status = %s, "the Drive File Provider domain was not registered")
            }
            Err(e) => tracing::warn!(error = %e, "the File Provider registrar did not run"),
        }
        match std::process::Command::new(&tool).arg("watch").spawn() {
            Ok(child) => *WATCH.lock().expect("watch") = Some(child),
            Err(e) => tracing::warn!(error = %e, "Finder will not hear of changes made elsewhere"),
        }
    }

    pub fn stop() {
        if let Some(mut child) = WATCH.lock().expect("watch").take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
