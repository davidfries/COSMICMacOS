// SPDX-License-Identifier: GPL-3.0-or-later
//! cosmicMac panel (Spike A).
//!
//! A minimal libcosmic application proving that libcosmic (iced + winit +
//! wgpu/Metal) builds and renders on macOS: one borderless window, the
//! height of the COSMIC panel, with a label on the left and an `HH:MM` clock
//! on the right, in cosmic's dark theme. Once the window is open its
//! `NSWindow` is turned into a status-level, all-Spaces panel at the top of
//! the main screen (see [`appkit`]). Real panel features (applets, dock,
//! cosmic-panel-config) are milestone 2.

mod appkit;

use std::time::Duration;

use cosmic::app::{Core, Settings, Task};
use cosmic::iced::{self, Alignment, Length, Size, Subscription, window};
use cosmic::{Element, executor, widget};

/// Logical height of the panel bar, matching COSMIC's default panel size.
pub const PANEL_HEIGHT: f64 = 32.0;

/// Initial logical width; [`appkit::configure_panel_window`] stretches the
/// window to the main screen's width (1728 px in the test VM).
const INITIAL_WIDTH: f32 = 1728.0;

/// Horizontal padding between the screen edges and the panel contents.
const EDGE_PADDING: u16 = 12;

fn main() -> iced::Result {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,cosmic_mac_panel=info"),
    )
    .init();

    // `client_decorations` (on by default) makes libcosmic open the window
    // without OS decorations; `resizable(None)` drops winit's resize border.
    let settings = Settings::default()
        .size(Size::new(INITIAL_WIDTH, PANEL_HEIGHT as f32))
        .resizable(None)
        .theme(cosmic::Theme::dark());

    cosmic::app::run::<Panel>(settings, ())
}

#[derive(Clone, Debug)]
pub enum Message {
    /// One-second timer tick; refreshes the clock text.
    Tick,
    /// A window opened; the main window gets the AppKit panel treatment.
    WindowOpened(window::Id),
    /// Result of [`appkit::configure_panel_window`].
    WindowConfigured(Result<appkit::PanelWindow, String>),
}

pub struct Panel {
    core: Core,
    clock: String,
}

impl cosmic::Application for Panel {
    type Executor = executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = "com.cosmicmac.Panel";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(mut core: Core, _flags: Self::Flags) -> (Self, Task<Self::Message>) {
        // Render `view()` directly: no header bar, window border or rounded
        // corners from libcosmic's application template (applets do the same).
        core.window.use_template = false;
        core.window.show_headerbar = false;
        core.window.sharp_corners = true;
        let panel = Panel {
            core,
            clock: clock_text(&jiff::Zoned::now()),
        };
        (panel, Task::none())
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        Subscription::batch([
            iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick),
            window::open_events().map(Message::WindowOpened),
        ])
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::Tick => {
                self.clock = clock_text(&jiff::Zoned::now());
            }
            Message::WindowOpened(id) => {
                log::info!("window opened: {id:?}");
                if self.core.main_window_id() == Some(id) {
                    return window::run_with_handle(id, appkit::configure_panel_window)
                        .map(|result| cosmic::Action::App(Message::WindowConfigured(result)));
                }
            }
            Message::WindowConfigured(Ok(info)) => {
                log::info!(
                    "panel NSWindow configured: windowNumber={} frame=({:.0},{:.0} {:.0}x{:.0}) \
                     level={} styleMask={:#x} collectionBehavior={:#x}",
                    info.window_number,
                    info.frame.0,
                    info.frame.1,
                    info.frame.2,
                    info.frame.3,
                    info.level,
                    info.style_mask,
                    info.collection_behavior,
                );
            }
            Message::WindowConfigured(Err(err)) => {
                log::error!("cannot configure the panel NSWindow: {err}");
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let content = widget::row::with_children(vec![
            widget::text::body("cosmicMac panel").into(),
            widget::space::horizontal().into(),
            widget::text::body(self.clock.as_str()).into(),
        ])
        .align_y(Alignment::Center)
        .padding([0, EDGE_PADDING])
        .width(Length::Fill)
        .height(Length::Fill);

        widget::container(content)
            .class(cosmic::theme::Container::Background)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

/// Formats a local time as the panel clock text (`HH:MM`, 24-hour).
fn clock_text(now: &jiff::Zoned) -> String {
    now.strftime("%H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_text_is_zero_padded_24_hour() {
        let zoned = jiff::civil::date(2026, 10, 31)
            .at(7, 5, 59, 0)
            .in_tz("UTC")
            .unwrap();
        assert_eq!(clock_text(&zoned), "07:05");

        let zoned = jiff::civil::date(2026, 10, 31)
            .at(23, 59, 0, 0)
            .in_tz("UTC")
            .unwrap();
        assert_eq!(clock_text(&zoned), "23:59");
    }
}
