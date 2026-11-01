// SPDX-License-Identifier: GPL-3.0-or-later
//! The launcher UI: a libcosmic application with one borderless window
//! modelled on cosmic-launcher (search field, up to [`MAX_ROWS`] result rows
//! with name and description, highlighted selection).
//!
//! Keys: typing searches, Up/Down and Ctrl+P/Ctrl+N (also Tab) move the
//! selection, Return activates, Escape quits. The process quits after an
//! activation and when the window loses key focus.
//!
//! libcosmic's main-window path cannot set a window level, visibility or
//! position, so the app runs with `no_main_window` and opens its own window
//! hidden at winit's floating level; [`appkit::configure_window`] then
//! places, raises and shows it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, Instant};

use cosmic::app::{Core, Task};
use cosmic::iced::alignment::{Horizontal, Vertical};
use cosmic::iced::core::text::{Ellipsize, EllipsizeHeightLimit};
use cosmic::iced::futures::channel::oneshot;
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

use crate::geometry::{self, MAX_ROWS};
use crate::plugin::{Entry, IconSource};
use crate::plugins::{Plugins, apps, windows};
use crate::{appkit, icons};

static INPUT_ID: LazyLock<cosmic::widget::Id> =
    LazyLock::new(|| cosmic::widget::Id::new("launcher-search"));

/// Delay before the first key-focus check after the window is shown.
const INITIAL_FOCUS_CHECK: Duration = Duration::from_millis(300);
/// Delay before the final check after re-activating a window that was not
/// key at the first check.
const FOCUS_RECHECK: Duration = Duration::from_millis(500);
/// Delay between an `Unfocused` event and the check that quits, so a brief
/// focus flutter while the app activates does not close the launcher.
const UNFOCUS_DEBOUNCE: Duration = Duration::from_millis(150);

pub struct Flags {
    /// When `main` started, for the startup-timing log.
    pub started: Instant,
}

#[derive(Clone, Debug)]
pub enum Message {
    WindowOpened(window::Id),
    WindowConfigured(Result<appkit::WindowInfo, String>),
    WindowEvent(window::Id, window::Event),
    FirstFrame,
    CheckFocus {
        retry: bool,
    },
    FocusChecked {
        retry: bool,
        result: Result<bool, String>,
    },
    AppsScanned(Vec<apps::AppEntry>, Duration),
    WindowsLoaded(Result<Vec<WindowState>, String>, Duration),
    /// Rendered (or cached) PNG icons per bundle path; `None` if rendering
    /// failed.
    IconsLoaded(Vec<(PathBuf, Option<Vec<u8>>)>),
    Input(String),
    /// Move the selection by this many rows (wrapping).
    Move(isize),
    /// Return in the search field.
    Submit,
    /// A row was clicked.
    Activate(usize),
    Close,
}

pub struct Launcher {
    core: Core,
    started: Instant,
    window: Option<window::Id>,
    configured: bool,
    has_been_key: bool,
    first_frame_seen: bool,
    closing: bool,
    query: String,
    plugins: Plugins,
    entries: Vec<Entry>,
    selected: usize,
    /// App icons by bundle path; `None` once rendering failed (generic icon).
    icons: HashMap<PathBuf, Option<icon::Handle>>,
    icons_pending: HashSet<PathBuf>,
    icon_cache: Option<PathBuf>,
}

impl Launcher {
    fn elapsed_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
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
        let missing: Vec<PathBuf> = self
            .entries
            .iter()
            .filter_map(|entry| match &entry.result.icon {
                Some(IconSource::Bundle(path)) => Some(path.clone()),
                _ => None,
            })
            .filter(|path| !self.icons.contains_key(path) && !self.icons_pending.contains(path))
            .collect();
        if missing.is_empty() {
            return Task::none();
        }
        self.icons_pending.extend(missing.iter().cloned());
        let cache = self.icon_cache.clone();
        background(
            move || {
                missing
                    .into_iter()
                    .map(|path| {
                        let png = icons::load_or_render(&path, cache.as_deref());
                        (path, png)
                    })
                    .collect()
            },
            Message::IconsLoaded,
        )
    }

    fn with_window<T: Send + 'static>(
        &self,
        f: impl FnOnce(window::raw_window_handle::WindowHandle<'_>) -> T + Send + 'static,
        to_message: impl Fn(T) -> Message + Send + 'static,
    ) -> Task<Message> {
        match self.window {
            Some(id) => window::run_with_handle(id, f)
                .map(move |value| cosmic::Action::App(to_message(value))),
            None => Task::none(),
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
        self.close()
    }

    /// Hides the window, then exits the event loop.
    fn close(&mut self) -> Task<Message> {
        if self.closing {
            return Task::none();
        }
        self.closing = true;
        let hide = match self.window {
            Some(id) => window::run_with_handle(id, |handle| {
                if let Err(error) = appkit::hide_window(handle) {
                    log::warn!("cannot hide the window: {error}");
                }
            })
            .discard(),
            None => Task::none(),
        };
        hide.chain(iced::exit())
    }

    fn focus_check(retry: bool, delay: Duration) -> Task<Message> {
        background(
            move || thread::sleep(delay),
            move |()| Message::CheckFocus { retry },
        )
    }
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

fn launcher_window_settings() -> window::Settings {
    window::Settings {
        size: Size::new(geometry::WINDOW_WIDTH, geometry::window_height()),
        position: window::Position::Centered,
        // Shown by `appkit::configure_window` once placed and raised.
        visible: false,
        resizable: false,
        minimizable: false,
        decorations: false,
        transparent: true,
        // Never at level 0, where the WM would consider managing it.
        level: window::Level::AlwaysOnTop,
        exit_on_close_request: true,
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
        let launcher = Launcher {
            core,
            started: flags.started,
            window: None,
            configured: false,
            has_been_key: false,
            first_frame_seen: false,
            closing: false,
            query: String::new(),
            plugins: Plugins::new(home.clone()),
            entries: Vec::new(),
            selected: 0,
            icons: HashMap::new(),
            icons_pending: HashSet::new(),
            icon_cache: home.as_deref().map(icons::cache_dir),
        };

        let (_, open) = window::open(launcher_window_settings());
        let roots = apps::default_roots(home.as_deref());
        let scan = background(
            move || {
                let started = Instant::now();
                let entries = apps::scan(&roots);
                (entries, started.elapsed())
            },
            |(entries, took)| Message::AppsScanned(entries, took),
        );
        let wm = background(
            || {
                let started = Instant::now();
                let windows = windows::default_client(windows::LIST_TIMEOUT)
                    .and_then(|client| windows::fetch_windows(&client));
                (windows, started.elapsed())
            },
            |(windows, took)| Message::WindowsLoaded(windows, took),
        );
        let open = open.map(|id| cosmic::Action::App(Message::WindowOpened(id)));
        (launcher, Task::batch([open, scan, wm]))
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let mut subscriptions = vec![
            window::events().map(|(id, event)| Message::WindowEvent(id, event)),
            event::listen_with(key_message),
        ];
        if !self.first_frame_seen {
            subscriptions.push(window::frames().map(|_| Message::FirstFrame));
        }
        Subscription::batch(subscriptions)
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::WindowOpened(id) => {
                log::info!("window opened at +{:.0} ms", self.elapsed_ms());
                self.window = Some(id);
                return Task::batch([
                    self.with_window(appkit::configure_window, Message::WindowConfigured),
                    text_input::focus(INPUT_ID.clone()),
                ]);
            }
            Message::WindowConfigured(Ok(info)) => {
                self.configured = true;
                log::info!(
                    "window visible at +{:.0} ms: windowNumber={} frame=({:.0},{:.0} {:.0}x{:.0}) \
                     in visibleFrame=({:.0},{:.0} {:.0}x{:.0}) level={} policy={}→{} \
                     bundleId={:?} key={} active={}",
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
                return Self::focus_check(true, INITIAL_FOCUS_CHECK);
            }
            Message::WindowConfigured(Err(error)) => {
                log::error!("cannot configure the launcher window: {error}");
                return self.close();
            }
            Message::WindowEvent(id, event) if Some(id) == self.window => match event {
                window::Event::Focused => {
                    if !self.has_been_key {
                        log::info!("window became key at +{:.0} ms", self.elapsed_ms());
                    }
                    self.has_been_key = true;
                }
                window::Event::Unfocused if self.configured && self.has_been_key => {
                    return Self::focus_check(false, UNFOCUS_DEBOUNCE);
                }
                window::Event::CloseRequested => return self.close(),
                _ => {}
            },
            Message::WindowEvent(..) => {}
            Message::FirstFrame => {
                if !self.first_frame_seen {
                    self.first_frame_seen = true;
                    log::info!(
                        "first frame at +{:.0} ms (window {})",
                        self.elapsed_ms(),
                        if self.configured {
                            "visible"
                        } else {
                            "still hidden"
                        }
                    );
                }
            }
            Message::CheckFocus { retry } => {
                if self.closing {
                    return Task::none();
                }
                return self.with_window(
                    move |handle| appkit::check_key(handle, retry),
                    move |result| Message::FocusChecked { retry, result },
                );
            }
            Message::FocusChecked {
                result: Ok(true), ..
            } => {
                if !self.has_been_key {
                    log::info!("window is key at +{:.0} ms", self.elapsed_ms());
                }
                self.has_been_key = true;
            }
            Message::FocusChecked {
                retry: true,
                result: Ok(false),
            } => {
                log::warn!("launcher window is not key; activating again");
                return Self::focus_check(false, FOCUS_RECHECK);
            }
            Message::FocusChecked {
                retry: false,
                result: Ok(false),
            } => {
                if self.has_been_key {
                    log::info!("lost key focus; quitting");
                } else {
                    log::warn!("launcher window never became key (activation refused?); quitting");
                }
                return self.close();
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
                    took.as_secs_f64() * 1000.0,
                    self.elapsed_ms()
                );
                self.plugins.set_apps(entries);
                return self.refresh();
            }
            Message::WindowsLoaded(Ok(windows), took) => {
                log::info!(
                    "window manager lists {} windows ({:.0} ms)",
                    windows.len(),
                    took.as_secs_f64() * 1000.0
                );
                self.plugins.set_windows(windows);
                return self.refresh();
            }
            Message::WindowsLoaded(Err(error), _) => {
                log::info!("no window list from the window manager: {error}");
            }
            Message::IconsLoaded(icons) => {
                for (path, png) in icons {
                    if png.is_none() {
                        log::debug!("no icon for {}", path.display());
                    }
                    self.icons_pending.remove(&path);
                    self.icons.insert(path, png.map(icon::from_raster_bytes));
                }
            }
            Message::Input(query) => {
                self.query = query;
                self.selected = 0;
                return self.refresh();
            }
            Message::Move(delta) => {
                let count = self.entries.len() as isize;
                if count > 0 {
                    self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
                }
            }
            Message::Submit => return self.activate(self.selected),
            Message::Activate(index) => return self.activate(index),
            Message::Close => return self.close(),
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Self::Message> {
        // There is no main window (`no_main_window`); see `view_window`.
        widget::space::horizontal().into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Self::Message> {
        let search = text_input::search_input(
            "Search apps and windows, = to calculate, g or ddg to search the web",
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
        Key::Named(Named::Escape) => Some(Message::Close),
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
            Some("Close")
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
