// SPDX-License-Identifier: GPL-3.0-or-later
//! The launcher UI: a resident libcosmic application whose window is
//! modelled on cosmic-launcher (search field, up to [`MAX_ROWS`] result rows
//! with name and description, highlighted selection).
//!
//! Keys: typing searches, Up/Down and Ctrl+P/Ctrl+N (also Tab) move the
//! selection, Return activates, Escape hides. The launcher also hides after
//! an activation and when its window loses key focus.
//!
//! libcosmic's main-window path cannot set a window level, visibility or
//! position, so the app runs with `no_main_window` and opens its own window
//! hidden at winit's floating level; [`appkit::show_window`] then places,
//! raises and shows it.
//!
//! Resident mode: lines from the control socket ([`install_commands`])
//! toggle, show or hide the launcher. Hiding orders the window out and
//! closes it; showing opens a fresh one. With `no_main_window` libcosmic
//! runs an iced daemon, which neither exits nor drops its wgpu compositor
//! when the last window closes, so fonts, GPU pipelines, the app list and
//! the icon cache stay warm and a show costs one window creation. Keeping
//! a hidden window open instead does not work: iced redraws every window
//! after every update, and a `CAMetalLayer` that is off screen runs out of
//! drawables, blocks a second per frame and retries forever. While hidden
//! only event-driven subscriptions run (the socket, the macOS appearance and
//! COSMIC's Mode config), so the app sleeps.
//!
//! Window ids are unique per show. `window::open` returns the id at once
//! but the window appears later, and tasks for a window iced does not know
//! yet are dropped, so everything that touches the window waits for its
//! `WindowOpened`; a window that is no longer wanted by then is closed.
//! A hidden start opens one window that is never shown (warm-up: the
//! compositor is created with the first window) and closes it after its
//! first frame, unless a `show` arrives first and takes it over.
//!
//! Dark or light follows [`crate::theme`]: re-resolved on every show, on the
//! macOS appearance notification and on COSMIC Mode config changes; a change
//! swaps the libcosmic theme.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use cosmic::app::{Core, Task};
use cosmic::cosmic_theme::{THEME_MODE_ID, Theme as CosmicTheme, ThemeMode};
use cosmic::iced::alignment::{Horizontal, Vertical};
use cosmic::iced::core::text::{Ellipsize, EllipsizeHeightLimit};
use cosmic::iced::futures::channel::{mpsc, oneshot};
use cosmic::iced::futures::stream::{self, BoxStream, StreamExt};
use cosmic::iced::keyboard::key::Named;
use cosmic::iced::keyboard::{self, Key};
use cosmic::iced::{
    self, Alignment, Border, Color, Length, Shadow, Size, Subscription, event, window,
};
use cosmic::theme::{self, Button};
use cosmic::widget::text_input::{self, StyleSheet as _};
use cosmic::widget::{self, button, container, divider, icon};
use cosmic::{Element, executor};
use cosmic_mac_protocol::WindowState;
use cosmic_mac_protocol::launcher::LauncherCommand;
use cosmic_mac_protocol::theme::Mode;

use crate::geometry::{self, MAX_ROWS};
use crate::plugin::{Entry, IconSource};
use crate::plugins::{Plugins, apps, files, windows};
use crate::{appkit, icons, theme as launcher_theme};

static INPUT_ID: LazyLock<cosmic::widget::Id> =
    LazyLock::new(|| cosmic::widget::Id::new("launcher-search"));

/// Delay before the first key-focus check after the window is shown.
const INITIAL_FOCUS_CHECK: Duration = Duration::from_millis(300);
/// Delay before the final check after re-activating a window that was not
/// key at the first check.
const FOCUS_RECHECK: Duration = Duration::from_millis(500);
/// Delay between an `Unfocused` event and the check that hides, so a brief
/// focus flutter while the app activates does not hide the launcher.
const UNFOCUS_DEBOUNCE: Duration = Duration::from_millis(150);
/// Typing pause before a Spotlight file search starts.
const FILE_SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);
/// Longest wait for the warm-up window's first frame.
const WARM_UP_TIMEOUT: Duration = Duration::from_secs(3);
/// When a hidden start gives focus back: winit activates the app when it
/// finishes launching, at a moment iced does not report.
const YIELD_FOCUS_AT: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_millis(400),
    Duration::from_millis(1200),
];

pub struct Flags {
    /// When `main` started, for the timing log.
    pub started: Instant,
    /// Show right away (`--show`) instead of starting hidden.
    pub show: bool,
    /// The mode the initial theme was built for.
    pub theme_mode: Mode,
}

/// Control-socket commands, handed from `main` to the subscription.
static COMMANDS: Mutex<Option<mpsc::UnboundedReceiver<LauncherCommand>>> = Mutex::new(None);

/// Installs the receiving end of the control socket's commands; call before
/// the app runs.
pub fn install_commands(receiver: mpsc::UnboundedReceiver<LauncherCommand>) {
    *COMMANDS.lock().unwrap_or_else(PoisonError::into_inner) = Some(receiver);
}

/// The control-socket commands as messages. The subscription is part of
/// every [`Launcher::subscription`], so iced builds this stream once and it
/// takes the receiver once.
fn ipc_commands() -> BoxStream<'static, Message> {
    let receiver = COMMANDS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    match receiver {
        Some(receiver) => receiver.map(Message::Ipc).boxed(),
        None => stream::pending().boxed(),
    }
}

/// macOS appearance changes as messages; like [`ipc_commands`], part of
/// every subscription.
fn appearance_changes() -> BoxStream<'static, Message> {
    launcher_theme::appearance_changes()
        .map(|()| Message::CheckTheme)
        .boxed()
}

#[derive(Clone, Debug)]
pub enum Message {
    /// A line from the control socket.
    Ipc(LauncherCommand),
    /// The macOS appearance or COSMIC's Mode config changed: re-resolve the
    /// mode.
    CheckTheme,
    /// These keys of the current mode's theme config changed (none: the
    /// watch just started).
    ThemeConfigChanged(Vec<&'static str>),
    WindowOpened(window::Id),
    WindowShown(window::Id, Result<appkit::WindowInfo, String>),
    WindowEvent(window::Id, window::Event),
    /// A window is about to draw a frame.
    Frame(window::Id),
    WarmUpTimeout,
    YieldFocus,
    CheckFocus {
        retry: bool,
        generation: u64,
    },
    FocusChecked {
        retry: bool,
        generation: u64,
        result: Result<bool, String>,
    },
    AppsScanned(Vec<apps::AppEntry>, Duration),
    WindowsLoaded(Result<Vec<WindowState>, String>, Duration),
    /// Rendered (or cached) PNG icons per bundle or file path; `None` if
    /// rendering failed.
    IconsLoaded(Vec<(PathBuf, Option<Vec<u8>>)>),
    /// Start the pending Spotlight search, unless newer input came.
    SearchFiles {
        generation: u64,
    },
    /// A finished Spotlight search: the file name and its paths.
    FilesFound(String, Result<Vec<PathBuf>, String>),
    Input(String),
    /// Move the selection by this many rows (wrapping).
    Move(isize),
    /// Return in the search field.
    Submit,
    /// A row was clicked.
    Activate(usize),
    /// Escape.
    Dismiss,
}

/// The launcher window of the current show, or the warm-up window.
#[derive(Clone, Copy, Debug)]
struct LauncherWindow {
    id: window::Id,
    /// `WindowOpened` arrived, so window tasks reach it.
    opened: bool,
    /// [`appkit::show_window`] placed and showed it.
    shown: bool,
    has_been_key: bool,
    /// Opened at a hidden start only to create the renderer; never shown.
    warm_up: bool,
}

impl LauncherWindow {
    fn new(id: window::Id, warm_up: bool) -> Self {
        LauncherWindow {
            id,
            opened: false,
            shown: false,
            has_been_key: false,
            warm_up,
        }
    }
}

pub struct Launcher {
    core: Core,
    started: Instant,
    home: Option<PathBuf>,
    /// `None` while hidden.
    window: Option<LauncherWindow>,
    /// Bumped by every show and hide; delayed focus checks carry it, so a
    /// check scheduled for an earlier show is ignored.
    generation: u64,
    /// When the current show was requested, until its first frame (for the
    /// latency log).
    show_requested: Option<Instant>,
    /// When the current show was requested (kept after the first frame, for
    /// later latency logs such as "became key").
    shown_at: Option<Instant>,
    /// Bumped by every query change; a delayed file search runs only if it
    /// carries the latest value.
    query_generation: u64,
    query: String,
    plugins: Plugins,
    entries: Vec<Entry>,
    selected: usize,
    /// Icons by bundle or file path; `None` once rendering failed (generic
    /// icon).
    icons: HashMap<PathBuf, Option<icon::Handle>>,
    icons_pending: HashSet<PathBuf>,
    /// File (not app) icons in `icons`, dropped on hide so the cache does
    /// not grow with every file ever found.
    transient_icons: HashSet<PathBuf>,
    icon_cache: Option<PathBuf>,
    /// The mode the current libcosmic theme was built for.
    theme_mode: Mode,
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

impl Launcher {
    fn elapsed_ms(&self) -> f64 {
        millis(self.started.elapsed())
    }

    /// Milliseconds since the current show was requested (since the process
    /// started if none is pending).
    fn since_show_ms(&self) -> f64 {
        self.shown_at
            .map_or_else(|| self.elapsed_ms(), |at| millis(at.elapsed()))
    }

    /// Shown, or opening to be shown (not the warm-up window).
    fn is_visible(&self) -> bool {
        self.window.is_some_and(|window| !window.warm_up)
    }

    /// Re-runs the query and requests icons for the rows now on screen.
    fn refresh(&mut self) -> Task<Message> {
        self.entries = self.plugins.search(&self.query, MAX_ROWS);
        if self.selected >= self.entries.len() {
            self.selected = 0;
        }
        self.request_icons()
    }

    fn request_icons(&mut self) -> Task<Message> {
        let missing: Vec<(PathBuf, bool)> = self
            .entries
            .iter()
            .filter_map(|entry| match &entry.result.icon {
                Some(IconSource::Bundle(path)) => Some((path.clone(), false)),
                Some(IconSource::File(path)) => Some((path.clone(), true)),
                _ => None,
            })
            .filter(|(path, _)| {
                !self.icons.contains_key(path) && !self.icons_pending.contains(path)
            })
            .collect();
        if missing.is_empty() {
            return Task::none();
        }
        for (path, transient) in &missing {
            self.icons_pending.insert(path.clone());
            if *transient {
                self.transient_icons.insert(path.clone());
            }
        }
        let cache = self.icon_cache.clone();
        background(
            move || {
                missing
                    .into_iter()
                    .map(|(path, transient)| {
                        // File icons are not worth a file in the disk cache.
                        let dir = if transient { None } else { cache.as_deref() };
                        let png = icons::load_or_render(&path, dir);
                        (path, png)
                    })
                    .collect()
            },
            Message::IconsLoaded,
        )
    }

    /// Runs `f` with the window's handle once iced knows the window.
    fn with_window<T: Send + 'static>(
        &self,
        f: impl FnOnce(window::raw_window_handle::WindowHandle<'_>) -> T + Send + 'static,
        to_message: impl Fn(T) -> Message + Send + 'static,
    ) -> Task<Message> {
        match self.window {
            Some(LauncherWindow {
                id, opened: true, ..
            }) => window::run_with_handle(id, f)
                .map(move |value| cosmic::Action::App(to_message(value))),
            _ => Task::none(),
        }
    }

    fn activate(&mut self, index: usize) -> Task<Message> {
        let Some(entry) = self.entries.get(index).cloned() else {
            return Task::none();
        };
        let Some(action) = self.plugins.activate(&entry) else {
            return Task::none();
        };
        log::info!("activating {:?}: {action:?}", entry.result.name);
        if let Err(error) = appkit::perform(&action) {
            log::error!("cannot perform {action:?}: {error}");
        }
        self.hide()
    }

    fn toggle(&mut self) -> Task<Message> {
        if self.is_visible() {
            self.hide()
        } else {
            self.show()
        }
    }

    /// Shows the launcher with an empty query, centred on the main screen
    /// and with keyboard focus (also when it is shown already). Reloads the
    /// app and window lists in the background.
    fn show(&mut self) -> Task<Message> {
        self.generation += 1;
        self.show_requested = Some(Instant::now());
        self.shown_at = self.show_requested;
        self.reset_query();
        // A change while hidden (or a notification held back while the app
        // was inactive) is applied before the window draws.
        let theme = self.sync_theme();
        let window = match &mut self.window {
            // Shown, opening, or the warm-up window, which this show takes
            // over (presented now or when it has opened).
            Some(window) => {
                window.warm_up = false;
                self.present()
            }
            None => {
                let (id, open) = window::open(launcher_window_settings());
                self.window = Some(LauncherWindow::new(id, false));
                open.map(|id| cosmic::Action::App(Message::WindowOpened(id)))
            }
        };
        Task::batch([theme, window, self.scan_apps(), fetch_windows()])
    }

    /// Re-resolves dark/light and swaps the libcosmic theme if it changed.
    fn sync_theme(&mut self) -> Task<Message> {
        let mode = launcher_theme::resolve(self.home.as_deref());
        if mode == self.theme_mode {
            return Task::none();
        }
        log::info!("theme: {} -> {mode}", self.theme_mode);
        self.theme_mode = mode;
        self.reload_theme()
    }

    /// Rebuilds the libcosmic theme for the current mode from the config.
    fn reload_theme(&self) -> Task<Message> {
        cosmic::command::set_theme(launcher_theme::cosmic_theme(
            self.theme_mode,
            self.home.as_deref(),
        ))
    }

    /// Places, shows and focuses the window if it has opened (otherwise
    /// `WindowOpened` does this).
    fn present(&self) -> Task<Message> {
        let Some(LauncherWindow {
            id, opened: true, ..
        }) = self.window
        else {
            return Task::none();
        };
        Task::batch([
            window::run_with_handle(id, appkit::show_window)
                .map(move |result| cosmic::Action::App(Message::WindowShown(id, result))),
            text_input::focus(INPUT_ID.clone()),
        ])
    }

    /// Hides the launcher: orders the window out (giving focus back) and
    /// closes it. The warm-up window is left alone.
    fn hide(&mut self) -> Task<Message> {
        let Some(window) = self.window.filter(|window| !window.warm_up) else {
            return Task::none();
        };
        self.window = None;
        self.generation += 1;
        self.show_requested = None;
        self.reset_query();
        self.drop_transient_icons();
        log::info!("hiding the launcher");
        if !window.opened {
            // Closed when its `WindowOpened` arrives.
            return Task::none();
        }
        let id = window.id;
        window::run_with_handle(id, |handle| {
            if let Err(error) = appkit::hide_window(handle) {
                log::warn!("cannot hide the window: {error}");
            }
        })
        .discard()
        .chain(window::close(id))
    }

    fn reset_query(&mut self) {
        self.query.clear();
        self.entries.clear();
        self.selected = 0;
        self.query_generation += 1;
        self.plugins.files.clear();
    }

    fn drop_transient_icons(&mut self) {
        let icons = &mut self.icons;
        let pending = &self.icons_pending;
        // Icons still rendering stay listed and are dropped on arrival.
        self.transient_icons.retain(|path| {
            icons.remove(path);
            pending.contains(path)
        });
    }

    /// Opens the never-shown window that creates the renderer at a hidden
    /// start.
    fn warm_up(&mut self) -> Task<Message> {
        let (id, open) = window::open(launcher_window_settings());
        self.window = Some(LauncherWindow::new(id, true));
        Task::batch([
            open.map(|id| cosmic::Action::App(Message::WindowOpened(id))),
            delay(WARM_UP_TIMEOUT, || Message::WarmUpTimeout),
        ])
    }

    /// Closes the warm-up window (if a show has not taken it over).
    fn finish_warm_up(&mut self, reason: &str) -> Task<Message> {
        let Some(window) = self.window.filter(|window| window.warm_up) else {
            return Task::none();
        };
        self.window = None;
        log::info!("warmed up at +{:.0} ms ({reason})", self.elapsed_ms());
        self.yield_focus();
        if window.opened {
            window::close(window.id)
        } else {
            // Closed when its `WindowOpened` arrives.
            Task::none()
        }
    }

    /// Gives activation back while nothing is shown (hidden start).
    fn yield_focus(&self) {
        if self.is_visible() {
            return;
        }
        match appkit::yield_activation() {
            Ok(true) => log::info!("gave focus back at +{:.0} ms", self.elapsed_ms()),
            Ok(false) => {}
            Err(error) => log::warn!("cannot give focus back: {error}"),
        }
    }

    fn scan_apps(&self) -> Task<Message> {
        let roots = apps::default_roots(self.home.as_deref());
        background(
            move || {
                let started = Instant::now();
                let entries = apps::scan(&roots);
                (entries, started.elapsed())
            },
            |(entries, took)| Message::AppsScanned(entries, took),
        )
    }

    /// After the query or the file results changed: if the files plugin
    /// waits for a search, starts it after a typing pause. Invalidates the
    /// searches scheduled before.
    fn schedule_file_search(&mut self) -> Task<Message> {
        self.query_generation += 1;
        if files::parse(&self.query).is_none() || !self.plugins.files.has_query() {
            return Task::none();
        }
        let generation = self.query_generation;
        delay(FILE_SEARCH_DEBOUNCE, move || Message::SearchFiles {
            generation,
        })
    }

    fn focus_check(&self, retry: bool, after: Duration) -> Task<Message> {
        let generation = self.generation;
        delay(after, move || Message::CheckFocus { retry, generation })
    }
}

fn fetch_windows() -> Task<Message> {
    background(
        || {
            let started = Instant::now();
            let windows = windows::default_client(windows::LIST_TIMEOUT)
                .and_then(|client| windows::fetch_windows(&client));
            (windows, started.elapsed())
        },
        |(windows, took)| Message::WindowsLoaded(windows, took),
    )
}

/// Runs blocking `work` on its own thread and delivers its result as a
/// message, so file-system scans and IPC never block the UI thread.
fn background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    to_message: impl FnOnce(T) -> Message + Send + 'static,
) -> Task<Message> {
    let (sender, receiver) = oneshot::channel();
    thread::spawn(move || {
        let _ = sender.send(work());
    });
    Task::future(async move {
        match receiver.await {
            Ok(value) => cosmic::Action::App(to_message(value)),
            Err(_) => cosmic::Action::None,
        }
    })
}

/// Delivers `message()` after `duration`.
fn delay(duration: Duration, message: impl FnOnce() -> Message + Send + 'static) -> Task<Message> {
    background(move || thread::sleep(duration), move |()| message())
}

fn launcher_window_settings() -> window::Settings {
    window::Settings {
        size: Size::new(geometry::WINDOW_WIDTH, geometry::window_height()),
        position: window::Position::Centered,
        // Shown by `appkit::show_window` once placed and raised.
        visible: false,
        resizable: false,
        minimizable: false,
        decorations: false,
        transparent: true,
        // Never at level 0, where the WM would consider managing it.
        level: window::Level::AlwaysOnTop,
        // Closing hides the launcher; the process stays.
        exit_on_close_request: false,
        ..window::Settings::default()
    }
}

impl cosmic::Application for Launcher {
    type Executor = executor::Default;
    type Flags = Flags;
    type Message = Message;

    const APP_ID: &'static str = "com.cosmicmac.Launcher";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, flags: Self::Flags) -> (Self, Task<Self::Message>) {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from);
        let mut launcher = Launcher {
            core,
            started: flags.started,
            home: home.clone(),
            window: None,
            generation: 0,
            show_requested: None,
            shown_at: None,
            query_generation: 0,
            query: String::new(),
            plugins: Plugins::new(home.clone()),
            entries: Vec::new(),
            selected: 0,
            icons: HashMap::new(),
            icons_pending: HashSet::new(),
            transient_icons: HashSet::new(),
            icon_cache: home.as_deref().map(icons::cache_dir),
            theme_mode: flags.theme_mode,
        };
        let task = if flags.show {
            launcher.show()
        } else {
            let mut tasks = vec![launcher.warm_up(), launcher.scan_apps()];
            tasks.extend(
                YIELD_FOCUS_AT
                    .iter()
                    .map(|&at| delay(at, || Message::YieldFocus)),
            );
            Task::batch(tasks)
        };
        (launcher, task)
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        // The socket, the macOS appearance and COSMIC's Mode and theme
        // configs are event-driven and stay subscribed while hidden.
        let mut subscriptions = vec![
            Subscription::run(ipc_commands),
            Subscription::run(appearance_changes),
            self.core
                .watch_config::<ThemeMode>(THEME_MODE_ID)
                .map(|_| Message::CheckTheme),
            self.core
                .watch_config::<CosmicTheme>(self.theme_mode.theme_id())
                .map(|update| Message::ThemeConfigChanged(update.keys)),
        ];
        let frames = || window::frames().map(|(id, _)| Message::Frame(id));
        match self.window {
            // Hidden: nothing else, so the app sleeps.
            None => {}
            Some(window) if window.warm_up => subscriptions.push(frames()),
            Some(_) => {
                subscriptions
                    .push(window::events().map(|(id, event)| Message::WindowEvent(id, event)));
                subscriptions.push(event::listen_with(key_message));
                if self.show_requested.is_some() {
                    subscriptions.push(frames());
                }
            }
        }
        Subscription::batch(subscriptions)
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::Ipc(command) => {
                log::info!("socket command {command}");
                return match command {
                    LauncherCommand::Toggle => self.toggle(),
                    LauncherCommand::Show => self.show(),
                    LauncherCommand::Hide => self.hide(),
                };
            }
            Message::CheckTheme => return self.sync_theme(),
            Message::ThemeConfigChanged(keys) => {
                if keys.is_empty() {
                    return Task::none();
                }
                log::info!("{} theme config changed: {keys:?}", self.theme_mode);
                return self.reload_theme();
            }
            Message::WindowOpened(id) => {
                let Some(window) = self.window.as_mut().filter(|window| window.id == id) else {
                    log::debug!("closing window {id:?}: no longer wanted");
                    return window::close(id);
                };
                window.opened = true;
                if window.warm_up {
                    log::info!("warm-up window opened at +{:.0} ms", self.elapsed_ms());
                    return Task::none();
                }
                log::info!(
                    "window opened {:.0} ms after the show request",
                    self.since_show_ms()
                );
                return self.present();
            }
            Message::WindowShown(id, result) => {
                if !self
                    .window
                    .is_some_and(|window| window.id == id && !window.warm_up)
                {
                    return Task::none();
                }
                let info = match result {
                    Ok(info) => info,
                    Err(error) => {
                        log::error!("cannot show the launcher window: {error}");
                        return self.hide();
                    }
                };
                if let Some(window) = self.window.as_mut() {
                    window.shown = true;
                }
                log::info!(
                    "window visible {:.0} ms after the show request (+{:.0} ms): windowNumber={} \
                     frame=({:.0},{:.0} {:.0}x{:.0}) in visibleFrame=({:.0},{:.0} {:.0}x{:.0}) \
                     level={} policy={}→{} bundleId={:?} key={} active={}",
                    self.since_show_ms(),
                    self.elapsed_ms(),
                    info.window_number,
                    info.frame.x,
                    info.frame.y,
                    info.frame.width,
                    info.frame.height,
                    info.visible_frame.x,
                    info.visible_frame.y,
                    info.visible_frame.width,
                    info.visible_frame.height,
                    info.level,
                    info.initial_policy,
                    info.policy,
                    info.bundle_id,
                    info.is_key,
                    info.is_active,
                );
                return self.focus_check(true, INITIAL_FOCUS_CHECK);
            }
            Message::WindowEvent(id, event) => {
                let Some(window) = self
                    .window
                    .filter(|window| window.id == id && !window.warm_up)
                else {
                    return Task::none();
                };
                match event {
                    window::Event::Focused => {
                        if !window.has_been_key {
                            log::info!(
                                "window became key {:.0} ms after the show request",
                                self.since_show_ms()
                            );
                        }
                        if let Some(window) = self.window.as_mut() {
                            window.has_been_key = true;
                        }
                    }
                    window::Event::Unfocused if window.shown && window.has_been_key => {
                        return self.focus_check(false, UNFOCUS_DEBOUNCE);
                    }
                    window::Event::CloseRequested => return self.hide(),
                    _ => {}
                }
            }
            Message::Frame(id) => {
                let Some(window) = self.window.filter(|window| window.id == id) else {
                    return Task::none();
                };
                if window.warm_up {
                    return self.finish_warm_up("first frame drawn");
                }
                // Frames drawn while still hidden do not count.
                if window.shown
                    && let Some(requested) = self.show_requested.take()
                {
                    log::info!(
                        "first frame {:.0} ms after the show request (+{:.0} ms)",
                        millis(requested.elapsed()),
                        self.elapsed_ms()
                    );
                }
            }
            Message::WarmUpTimeout => return self.finish_warm_up("no frame in time"),
            Message::YieldFocus => self.yield_focus(),
            Message::CheckFocus { retry, generation } => {
                if generation != self.generation {
                    return Task::none();
                }
                return self.with_window(
                    move |handle| appkit::check_key(handle, retry),
                    move |result| Message::FocusChecked {
                        retry,
                        generation,
                        result,
                    },
                );
            }
            Message::FocusChecked { generation, .. } if generation != self.generation => {}
            Message::FocusChecked {
                result: Ok(true), ..
            } => {
                if let Some(window) = self.window.as_mut() {
                    window.has_been_key = true;
                }
            }
            Message::FocusChecked {
                retry: true,
                result: Ok(false),
                ..
            } => {
                log::warn!("launcher window is not key; activating again");
                return self.focus_check(false, FOCUS_RECHECK);
            }
            Message::FocusChecked {
                retry: false,
                result: Ok(false),
                ..
            } => {
                if self.window.is_some_and(|window| window.has_been_key) {
                    log::info!("lost key focus");
                } else {
                    log::warn!("launcher window never became key (activation refused?)");
                }
                return self.hide();
            }
            Message::FocusChecked {
                result: Err(error), ..
            } => {
                log::error!("cannot check key focus: {error}");
            }
            Message::AppsScanned(entries, took) => {
                log::info!(
                    "scanned {} apps in {:.0} ms (at +{:.0} ms)",
                    entries.len(),
                    millis(took),
                    self.elapsed_ms()
                );
                self.plugins.set_apps(entries);
                return self.refresh();
            }
            Message::WindowsLoaded(Ok(windows), took) => {
                log::info!(
                    "window manager lists {} windows ({:.0} ms)",
                    windows.len(),
                    millis(took)
                );
                self.plugins.set_windows(windows);
                return self.refresh();
            }
            Message::WindowsLoaded(Err(error), _) => {
                log::info!("no window list from the window manager: {error}");
            }
            Message::IconsLoaded(icons) => {
                let visible = self.is_visible();
                for (path, png) in icons {
                    if png.is_none() {
                        log::debug!("no icon for {}", path.display());
                    }
                    self.icons_pending.remove(&path);
                    // A file icon that arrives after the hide is not kept.
                    if !visible && self.transient_icons.remove(&path) {
                        continue;
                    }
                    self.icons.insert(path, png.map(icon::from_raster_bytes));
                }
            }
            Message::SearchFiles { generation } => {
                if generation != self.query_generation || !self.is_visible() {
                    return Task::none();
                }
                if let Some(query) = self.plugins.files.take_query() {
                    log::debug!("searching files named like {:?}", query.name);
                    return background(
                        move || query.run(),
                        |(name, result)| Message::FilesFound(name, result),
                    );
                }
            }
            Message::FilesFound(name, result) => {
                if !self.is_visible() {
                    return Task::none();
                }
                match &result {
                    Ok(paths) => log::debug!("{} files named like {name:?}", paths.len()),
                    Err(error) => log::warn!("file search for {name:?} failed: {error}"),
                }
                self.plugins.files.set_results(name, result);
                let refresh = self.refresh();
                return Task::batch([refresh, self.schedule_file_search()]);
            }
            Message::Input(query) => {
                self.query = query;
                self.selected = 0;
                let refresh = self.refresh();
                return Task::batch([refresh, self.schedule_file_search()]);
            }
            Message::Move(delta) => {
                let count = self.entries.len() as isize;
                if count > 0 {
                    self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
                }
            }
            Message::Submit => return self.activate(self.selected),
            Message::Activate(index) => return self.activate(index),
            Message::Dismiss => return self.hide(),
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Self::Message> {
        // There is no main window (`no_main_window`); see `view_window`.
        widget::space::horizontal().into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Self::Message> {
        let search = text_input::search_input(
            "Apps and windows · = math · g web · find files · run command",
            &self.query,
        )
        .on_input(Message::Input)
        .on_paste(Message::Input)
        .on_submit(|_| Message::Submit)
        // Capture Tab so it cycles results instead of moving widget focus.
        .on_tab(Message::Move(1))
        .style(theme::TextInput::Custom {
            active: Box::new(|theme| theme.focused(&theme::TextInput::Search)),
            error: Box::new(|theme| theme.focused(&theme::TextInput::Search)),
            hovered: Box::new(|theme| theme.focused(&theme::TextInput::Search)),
            focused: Box::new(|theme| theme.focused(&theme::TextInput::Search)),
            disabled: Box::new(|theme| theme.disabled(&theme::TextInput::Search)),
        })
        .width(Length::Fixed(geometry::CONTENT_WIDTH))
        .id(INPUT_ID.clone())
        .always_active();

        let mut content = widget::column::with_capacity(2)
            .spacing(geometry::LIST_SPACING)
            .width(Length::Fixed(geometry::CONTENT_WIDTH))
            .push(container(search).height(Length::Fixed(geometry::SEARCH_HEIGHT)));

        if !self.entries.is_empty() {
            let mut list = widget::column::with_capacity(self.entries.len() * 2);
            for (index, entry) in self.entries.iter().enumerate() {
                if index > 0 {
                    list = list.push(
                        container(divider::horizontal::light())
                            .height(Length::Fixed(geometry::DIVIDER_HEIGHT)),
                    );
                }
                list = list.push(result_row(
                    index,
                    entry,
                    index == self.selected,
                    &self.icons,
                ));
            }
            content = content.push(list);
        }

        let panel = container(content)
            .padding([geometry::PADDING_Y as u16, geometry::PADDING_X as u16])
            .width(Length::Fixed(geometry::WINDOW_WIDTH))
            .height(Length::Fixed(geometry::box_height(self.entries.len())))
            .class(theme::Container::custom(|theme| {
                let cosmic = theme.cosmic();
                let radii = cosmic.radius_s().map(|x| if x < 4.0 { x } else { x + 4.0 });
                container::Style {
                    text_color: Some(cosmic.on_bg_color().into()),
                    icon_color: Some(cosmic.on_bg_color().into()),
                    background: Some(Color::from(cosmic.background(theme.transparent).base).into()),
                    border: Border {
                        radius: radii.into(),
                        width: 1.0,
                        color: cosmic.bg_divider().into(),
                    },
                    shadow: Shadow::default(),
                    snap: true,
                }
            }));

        // The window is as tall as a full list; the rest stays transparent.
        container(panel)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_y(Vertical::Top)
            .into()
    }
}

/// Keyboard shortcuts handled regardless of the search field's focus.
fn key_message(event: iced::Event, _status: event::Status, _window: window::Id) -> Option<Message> {
    let iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key,
        physical_key,
        modifiers,
        ..
    }) = event
    else {
        return None;
    };
    match key.as_ref() {
        Key::Named(Named::ArrowUp) => Some(Message::Move(-1)),
        Key::Named(Named::ArrowDown) => Some(Message::Move(1)),
        Key::Named(Named::Escape) => Some(Message::Dismiss),
        _ if modifiers.control() => match key.to_latin(physical_key) {
            Some('p') => Some(Message::Move(-1)),
            Some('n') => Some(Message::Move(1)),
            _ => None,
        },
        _ => None,
    }
}

fn row_icon(
    entry: &Entry,
    icons: &HashMap<PathBuf, Option<icon::Handle>>,
) -> Element<'static, Message> {
    let generic = || {
        icon::from_name("application-x-executable")
            .size(32)
            .handle()
    };
    let handle = match &entry.result.icon {
        Some(IconSource::Name(name)) => icon::from_name(*name).size(32).handle(),
        // Rendered asynchronously; the generic icon shows until it arrives.
        Some(IconSource::Bundle(path)) => match icons.get(path) {
            Some(Some(handle)) => handle.clone(),
            _ => generic(),
        },
        Some(IconSource::File(path)) => match icons.get(path) {
            Some(Some(handle)) => handle.clone(),
            _ => icon::from_name("text-x-generic-symbolic").size(32).handle(),
        },
        None if entry.result.window.is_some() => {
            icon::from_name("focus-windows-symbolic").size(32).handle()
        }
        None => generic(),
    };
    icon(handle)
        .width(Length::Fixed(32.0))
        .height(Length::Fixed(32.0))
        .into()
}

fn result_row<'a>(
    index: usize,
    entry: &'a Entry,
    selected: bool,
    icons: &HashMap<PathBuf, Option<icon::Handle>>,
) -> Element<'a, Message> {
    // Like cosmic-launcher, window results show the app line first.
    let (title, subtitle) = if entry.result.window.is_some() {
        (&entry.result.description, &entry.result.name)
    } else {
        (&entry.result.name, &entry.result.description)
    };
    let text_style = || {
        theme::Text::Custom(|theme| {
            let cosmic = theme.cosmic();
            iced::widget::text::Style {
                color: Some(cosmic.on_bg_color().into()),
                selected_fill: cosmic.accent_color().into(),
                selected_text_color: Some(cosmic.on_accent_color().into()),
            }
        })
    };
    let lines = widget::column::with_capacity(2)
        .push(
            widget::text::body(title.as_str())
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                .align_x(Horizontal::Left)
                .class(text_style()),
        )
        .push(
            widget::text::caption(subtitle.as_str())
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                .align_x(Horizontal::Left)
                .class(text_style()),
        )
        .width(Length::Fill);

    let row = widget::row::with_capacity(2)
        .push(row_icon(entry, icons))
        .push(lines)
        .spacing(12)
        .align_y(Alignment::Center);

    let style = move |hovered: bool| {
        move |focused: bool, theme: &cosmic::Theme| {
            let highlighted = selected || focused || hovered;
            let base = if highlighted {
                button::Catalog::hovered(theme, highlighted, highlighted, &Button::Text)
            } else {
                button::Catalog::active(theme, false, false, &Button::Text)
            };
            button::Style {
                border_radius: theme.cosmic().corner_radii.radius_s.into(),
                outline_width: 0.0,
                ..base
            }
        }
    };
    button::custom(row)
        .on_press(Message::Activate(index))
        .padding([8, 24])
        .width(Length::Fill)
        .height(Length::Fixed(geometry::ROW_HEIGHT))
        .class(Button::Custom {
            active: Box::new(style(false)),
            hovered: Box::new(style(true)),
            pressed: Box::new(style(true)),
            disabled: Box::new(|theme| {
                let base = button::Catalog::disabled(theme, &Button::Text);
                button::Style {
                    border_radius: theme.cosmic().corner_radii.radius_s.into(),
                    outline_width: 0.0,
                    ..base
                }
            }),
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::keyboard::{Location, Modifiers, key};

    fn press(key: Key, physical: key::Code, modifiers: Modifiers) -> iced::Event {
        iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: key::Physical::Code(physical),
            location: Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        })
    }

    fn message(event: iced::Event) -> Option<String> {
        key_message(event, event::Status::Ignored, window::Id::NONE).map(|m| format!("{m:?}"))
    }

    #[test]
    fn navigation_keys() {
        let none = Modifiers::empty();
        assert_eq!(
            message(press(
                Key::Named(Named::ArrowDown),
                key::Code::ArrowDown,
                none
            ))
            .as_deref(),
            Some("Move(1)")
        );
        assert_eq!(
            message(press(Key::Named(Named::ArrowUp), key::Code::ArrowUp, none)).as_deref(),
            Some("Move(-1)")
        );
        assert_eq!(
            message(press(Key::Named(Named::Escape), key::Code::Escape, none)).as_deref(),
            Some("Dismiss")
        );
        let ctrl = Modifiers::CTRL;
        assert_eq!(
            message(press(Key::Character("n".into()), key::Code::KeyN, ctrl)).as_deref(),
            Some("Move(1)")
        );
        assert_eq!(
            message(press(Key::Character("p".into()), key::Code::KeyP, ctrl)).as_deref(),
            Some("Move(-1)")
        );
        // Plain letters are for the search field.
        assert_eq!(
            message(press(Key::Character("n".into()), key::Code::KeyN, none)),
            None
        );
    }
}
