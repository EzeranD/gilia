// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;

use clap::Parser;
use gilia::engine::SessionConfig;
use iced::{
    Length, Task, Theme, Widget,
    widget::{container, mouse_area, text},
    window::{self, Settings, raw_window_handle::RawDisplayHandle},
};
use time::{UtcOffset, macros::format_description};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, fmt::time::OffsetTime};

mod playback;
mod widgets;

#[derive(Debug, Parser)]
#[command(version)]
struct Args {
    path: Option<String>,
}

struct App {
    window: window::Id,
    playback: playback::Playback,
    fullscreen: bool,
    drop_supported: bool,
    picking: bool,
}

#[derive(Debug, Clone)]
enum Message {
    Playback(playback::Message),
    DropSupportKnown(bool),
    OpenFile,
    FilePicked(Option<PathBuf>),
    FileDropped(window::Id, PathBuf),
    ToggleFullscreen,
    GotMode(window::Mode),
    WindowClosed(window::Id),
    Exit,
}

fn main() -> iced::Result {
    let timer = OffsetTime::new(
        UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC),
        format_description!(
            "[year]-[month padding:zero]-[day padding:zero] [hour]:[minute]:[second]"
        ),
    );

    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy()
        .add_directive("wgpu_hal=error".parse().unwrap())
        .add_directive("iced_wgpu=error".parse().unwrap())
        .add_directive("iced_winit=error".parse().unwrap());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(timer)
        .with_target(false)
        .with_file(true)
        .with_line_number(true)
        .init();

    iced::daemon(App::boot, App::update, App::view)
        .subscription(App::subscription)
        .theme(Theme::GruvboxDark)
        .run()
}

fn supports_file_drop(window: &dyn window::Window) -> bool {
    window
        .display_handle()
        .is_ok_and(|handle| !matches!(handle.as_raw(), RawDisplayHandle::Wayland(_)))
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        let (mut playback, playback_task) = playback::Playback::new(SessionConfig { hw_dec: true });
        if let Some(path) = Args::parse().path {
            playback.open(path);
        }
        let (id, open) = window::open(Settings::default());
        let open = open.then(|id| {
            Task::batch([
                window::gain_focus(id),
                window::run(id, supports_file_drop).map(Message::DropSupportKnown),
            ])
        });

        (
            Self {
                window: id,
                playback,
                fullscreen: false,
                drop_supported: false,
                picking: false,
            },
            Task::batch([open, playback_task]),
        )
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        iced::Subscription::batch([
            window::close_events().map(Message::WindowClosed),
            window::events().filter_map(|(id, event)| match event {
                window::Event::FileDropped(path) => Some(Message::FileDropped(id, path)),
                _ => None,
            }),
            self.playback.subscription(),
        ])
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Playback(message) => self.playback.update(message),
            Message::DropSupportKnown(supported) => {
                self.drop_supported = supported;
                Task::none()
            }
            Message::OpenFile if self.playback.unopened() && !self.picking => {
                self.picking = true;
                Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .pick_file()
                            .await
                            .map(|file| file.path().to_path_buf())
                    },
                    Message::FilePicked,
                )
            }
            Message::FilePicked(path) if self.picking => {
                self.picking = false;
                if let Some(path) = path {
                    self.playback.open(path.to_string_lossy().to_string());
                }
                Task::none()
            }
            Message::FileDropped(id, path)
                if id == self.window && self.playback.unopened() && !self.picking =>
            {
                self.playback.open(path.to_string_lossy().to_string());
                Task::none()
            }
            Message::OpenFile | Message::FilePicked(_) | Message::FileDropped(..) => Task::none(),
            Message::ToggleFullscreen => window::mode(self.window).map(Message::GotMode),
            Message::GotMode(mode) => match mode {
                window::Mode::Windowed => {
                    self.fullscreen = true;
                    window::set_mode(self.window, window::Mode::Fullscreen)
                }
                window::Mode::Fullscreen => {
                    self.fullscreen = false;
                    window::set_mode(self.window, window::Mode::Windowed)
                }
                window::Mode::Hidden => Task::none(),
            },
            Message::WindowClosed(id) => {
                if id == self.window {
                    Task::done(Message::Exit)
                } else {
                    Task::none()
                }
            }
            Message::Exit => iced::exit(),
        }
    }

    fn view(&self, _window: window::Id) -> impl Widget<Message> {
        if !self.playback.unopened() {
            return self.playback.view(self.fullscreen).boxed();
        }

        let label = if self.picking {
            "Choosing a file…"
        } else if self.drop_supported {
            "Drag a file here or click to open"
        } else {
            "Click to open a file"
        };

        let area = container(text(label))
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(|_| container::Style {
                background: Some(iced::Background::Color(iced::Color::BLACK)),
                ..Default::default()
            });

        if self.picking {
            area.boxed()
        } else {
            mouse_area(area)
                .on_press(Message::OpenFile)
                .interaction(iced::mouse::Interaction::Pointer)
                .boxed()
        }
    }
}
