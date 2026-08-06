// The tray icon and its menu (plan Task 23).

use crate::autostart;
use cw_core::config::DataPaths;
use cw_store::control::Pause;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::windows::EventLoopBuilderExtWindows;
use tracing::{error, info};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

/// How often the recorded pause is re-read. The tray is not told about a pause the CLI issued, so
/// the icon follows the database rather than the menu it was clicked from.
const POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// Side of the generated icons, in pixels. Windows scales the bitmap to whatever the notification
/// area asks for, and scaling down from 32 looks better than scaling up from 16.
const ICON_SIZE: u32 = 32;

/// Menu item ids. The id is what comes back on the event channel, so these are the whole mapping
/// from a click to an action.
const PAUSE_30M: &str = "pause-30m";
const PAUSE_1H: &str = "pause-1h";
const PAUSE_INDEFINITE: &str = "pause-indefinite";
const RESUME: &str = "resume";
const OPEN_FOLDER: &str = "open-folder";
const AUTOSTART: &str = "autostart";
const QUIT: &str = "quit";

/// Put the tray icon up on its own thread and return at once.
pub fn spawn(paths: DataPaths) {
    let started = std::thread::Builder::new()
        .name("tray".to_owned())
        .spawn(move || {
            if let Err(error) = run(paths) {
                error!("the tray did not come up, so this run has no tray: {error}");
            }
        });
    if let Err(error) = started {
        error!("the tray thread could not be spawned, so this run has no tray: {error}");
    }
}

/// Everything the menu acts on, held for as long as the event loop runs. Dropping [`Self::tray`]
/// takes the icon out of the notification area, so it lives here rather than in a temporary.
struct Tray {
    conn: rusqlite::Connection,
    root: std::path::PathBuf,
    tray: tray_icon::TrayIcon,
    /// The checkable autostart entry, sharing its state with the copy inside the menu.
    autostart_item: CheckMenuItem,
    active_icon: Icon,
    paused_icon: Icon,
    /// Which of the two icons is currently up, so an unchanged pause costs no icon swap.
    showing_paused: bool,
    last_poll: std::time::Instant,
}

/// Build the tray on this thread and hand the thread to the event loop, which never gives it back.
///
/// The event loop is built with the any-thread extension because this is not the main thread; the
/// main one is running the capture loop.
fn run(paths: DataPaths) -> Result<(), Box<dyn std::error::Error>> {
    let conn = cw_store::db::open(&paths.database())?;
    let event_loop = EventLoopBuilder::new().with_any_thread(true).build();

    let autostart_item = CheckMenuItem::with_id(
        AUTOSTART,
        "Autostart",
        true,
        // A registry the tray cannot read leaves the box clear rather than refusing to start: the
        // pause controls are worth more than the checkbox.
        autostart::is_enabled().unwrap_or_else(|error| {
            error!("the autostart registration could not be read: {error}");
            false
        }),
        None,
    );
    let menu = Menu::new();
    menu.append_items(&[
        &MenuItem::with_id(PAUSE_30M, "Pause 30m", true, None),
        &MenuItem::with_id(PAUSE_1H, "Pause 1h", true, None),
        &MenuItem::with_id(PAUSE_INDEFINITE, "Pause until resumed", true, None),
        &MenuItem::with_id(RESUME, "Resume", true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(OPEN_FOLDER, "Open data folder", true, None),
        &autostart_item,
        // Quit ends the collection, so it does not sit against something reached by a slip.
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(QUIT, "Quit", true, None),
    ])?;

    let active_icon = disc([0x1e, 0x88, 0xe5])?;
    let paused_icon = disc([0x9e, 0x9e, 0x9e])?;
    let tray = TrayIconBuilder::new()
        .with_tooltip("ContextWitness")
        .with_menu(Box::new(menu))
        .with_icon(active_icon.clone())
        .build()?;

    let mut state = Tray {
        conn,
        root: paths.root,
        tray,
        autostart_item,
        active_icon,
        paused_icon,
        showing_paused: false,
        last_poll: std::time::Instant::now(),
    };
    // Before the first wait, so a daemon started while a pause is in force comes up gray.
    state.follow_pause();

    event_loop.run(move |_event, _target, control_flow| {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            state.act(&event.id.0);
        }
        // Nothing here acts on clicks, but the channel behind them is unbounded and every mouse
        // move across the icon posts to it, so somebody has to empty it.
        while TrayIconEvent::receiver().try_recv().is_ok() {}

        if state.last_poll.elapsed() >= POLL {
            state.follow_pause();
        }
        *control_flow = ControlFlow::WaitUntil(state.last_poll + POLL);
    })
}

impl Tray {
    /// Carry out the menu item with this id. Every failure is reported and survived: the tray is
    /// the daemon's controls, not its collection, and a click that cannot be honoured is not a
    /// reason to stop capturing.
    fn act(&mut self, id: &str) {
        let now = chrono::Utc::now();
        match id {
            PAUSE_30M => self.pause(Pause::Until(now + chrono::TimeDelta::minutes(30)), now),
            PAUSE_1H => self.pause(Pause::Until(now + chrono::TimeDelta::hours(1)), now),
            PAUSE_INDEFINITE => self.pause(Pause::Indefinite, now),
            RESUME => {
                if let Err(error) =
                    cw_store::control::resume(&mut self.conn, ulid::Ulid::generate(), now)
                {
                    error!("the tray could not record the resume: {error}");
                }
                self.follow_pause();
            }
            OPEN_FOLDER => {
                // Not waited on: Explorer hands the window to an already running instance and can
                // outlive this process, and a tray blocked here would stop answering its menu.
                if let Err(error) = std::process::Command::new("explorer.exe")
                    .arg(&self.root)
                    .spawn()
                {
                    error!("the data folder could not be opened: {error}");
                }
            }
            AUTOSTART => self.toggle_autostart(),
            QUIT => {
                info!("quitting on the tray's Quit");
                // Nothing to unwind: every connection is in WAL, where a write cut off mid-way is
                // rolled back by whoever opens the database next.
                std::process::exit(0);
            }
            // Not this build's: the ids above are the whole menu, and the separators send nothing.
            other => error!(id = other, "an unknown tray menu item was activated"),
        }
    }

    /// Record a pause and show it. `set_pause` writes its own audit row.
    fn pause(&mut self, pause: Pause, now: chrono::DateTime<chrono::Utc>) {
        if let Err(error) =
            cw_store::control::set_pause(&mut self.conn, pause, ulid::Ulid::generate(), now)
        {
            error!("the tray could not record the pause: {error}");
        }
        self.follow_pause();
    }

    /// Apply the registration the user just asked for, then set the checkmark to what the registry
    /// answers rather than to what was asked: a write that did not land must not read as done.
    fn toggle_autostart(&self) {
        let result = if self.autostart_item.is_checked() {
            autostart::enable()
        } else {
            autostart::disable()
        };
        if let Err(error) = result {
            error!("the autostart registration could not be changed: {error}");
        }
        match autostart::is_enabled() {
            Ok(enabled) => self.autostart_item.set_checked(enabled),
            Err(error) => error!("the autostart registration could not be read back: {error}"),
        }
    }

    /// Put the icon where the recorded pause says it belongs.
    fn follow_pause(&mut self) {
        self.last_poll = std::time::Instant::now();
        let pause = match cw_store::control::get_pause(&self.conn) {
            Ok(pause) => pause,
            Err(error) => {
                error!("the tray could not read the pause: {error}");
                return;
            }
        };
        // A deadline already past is stored as given, so whether it still stops capture is decided
        // here against one instant, the same way the capture loop decides it.
        let now = chrono::Utc::now();
        let paused = match pause {
            Some(Pause::Indefinite) => true,
            Some(Pause::Until(deadline)) => now < deadline,
            None => false,
        };
        if paused == self.showing_paused {
            return;
        }

        let icon = if paused {
            self.paused_icon.clone()
        } else {
            self.active_icon.clone()
        };
        match self.tray.set_icon(Some(icon)) {
            // Only once the swap took: otherwise the next poll tries again.
            Ok(()) => self.showing_paused = paused,
            Err(error) => error!("the tray icon could not be swapped: {error}"),
        }
    }
}

/// A filled disc in `rgb` on a transparent square, in the 32bpp RGBA rows tray-icon takes. Drawn
/// here rather than loaded, so the binary carries no asset beside it.
fn disc(rgb: [u8; 3]) -> Result<Icon, tray_icon::BadIcon> {
    let radius = ICON_SIZE as f32 / 2.0;
    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            // Pixel centres, so the disc is centred on the square rather than a half pixel off it.
            let dx = x as f32 + 0.5 - radius;
            let dy = y as f32 + 0.5 - radius;
            let inside = dx * dx + dy * dy <= radius * radius;
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], if inside { 0xff } else { 0 }]);
        }
    }

    Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE)
}
