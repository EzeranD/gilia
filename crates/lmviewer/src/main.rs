use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use clap::Parser;
use iced::{
    Background, Color, Element, Length, Task, Theme,
    futures::{self, SinkExt},
    keyboard::{
        self,
        Key::{self, Character, Named},
        Modifiers,
        key::Named::{ArrowDown, ArrowLeft, ArrowRight, ArrowUp, Space},
    },
    mouse::ScrollDelta,
    widget::{container, mouse_area, stack, text},
    window::{self, Settings},
};
use lmv::{
    engine::{
        EngineConfig,
        ExternalEvent::{self, VolumesChanged},
    },
    player::Player,
};
use time::{UtcOffset, macros::format_description};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, fmt::time::OffsetTime};

#[derive(Debug, Parser)]
#[command(version)]
struct Args {
    path: String,
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

    ffmpeg_next::init().unwrap();
    ffmpeg_next::log::set_level(ffmpeg_next::log::Level::Quiet);
    pipewire::init();
    iced::daemon(App::boot, App::update, App::view)
        .subscription(App::subscription)
        .theme(Theme::GruvboxDark)
        .run()
}

#[derive(Debug, Clone)]
pub enum Message {
    WindowClosed(window::Id),
    VideoHovered(bool),
    KeyPressed(Key, keyboard::Modifiers),
    Scroll(ScrollDelta),
    CallbackEvent(ExternalEvent),
    ClearText(u32),
    GotMode(window::Mode),
}

enum WindowKind {
    Main,
}

struct App {
    windows: HashMap<window::Id, WindowKind>,
    player: Player,
    video_hovered: bool,
    first_volume_change: bool,
    text_id: u32,
    text: Option<String>,
    last_seek_time: Instant,
    eof_loop: bool,
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        let path = Args::parse().path;
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let cb = move |event| {
            event_tx.send(event).ok();
        };
        let player = Player::new(path, EngineConfig { hw_dec: true }, cb);
        let event_task = Task::stream(iced::stream::channel(
            100,
            move |mut sender: futures::channel::mpsc::Sender<Message>| async move {
                while let Some(event) = event_rx.recv().await {
                    sender.send(Message::CallbackEvent(event)).await.ok();
                }
            },
        ));
        let (id, open) = window::open(Settings::default());
        let open = open.discard();
        let mut windows = HashMap::new();
        windows.insert(id, WindowKind::Main);
        (
            Self {
                windows,
                player,
                video_hovered: false,
                first_volume_change: true,
                text_id: 0,
                text: None,
                last_seek_time: Instant::now(),
                eof_loop: false,
            },
            Task::batch([open, event_task]),
        )
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        iced::Subscription::batch([
            window::close_events().map(Message::WindowClosed),
            keyboard::listen().filter_map(|e| {
                if let keyboard::Event::KeyPressed { key, modifiers, .. } = e {
                    Some(Message::KeyPressed(key, modifiers))
                } else {
                    None
                }
            }),
        ])
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowClosed(id) => {
                if Some(id) == self.main_id() {
                    iced::exit()
                } else {
                    Task::none()
                }
            }
            Message::VideoHovered(bool) => {
                self.video_hovered = bool;
                Task::none()
            }
            Message::KeyPressed(key, modifiers) => self.key_pressed(key, modifiers),
            Message::Scroll(delta) => match delta {
                ScrollDelta::Lines { x: _, y } => {
                    if y > 0.0 {
                        return self.adjust_volume(0.02);
                    } else if y < 0.0 {
                        return self.adjust_volume(-0.02);
                    }
                    Task::none()
                }
                ScrollDelta::Pixels { x: _, y: _ } => Task::none(),
            },
            Message::CallbackEvent(VolumesChanged(volumes)) => {
                if self.first_volume_change {
                    self.first_volume_change = false;
                    return Task::none();
                }
                let channel1_vol = volumes.first().copied().unwrap_or(0.0);
                let text = format!("Volume {}%", (channel1_vol * 100.0).round() as u32);
                self.set_text(text, Duration::from_secs(2))
            }
            Message::CallbackEvent(ExternalEvent::Eof) => {
                if self.eof_loop {
                    self.player.seek_to(0);
                    self.player.play();
                }
                Task::none()
            }
            Message::CallbackEvent(ExternalEvent::NewFrame | ExternalEvent::Error(_)) => {
                Task::none()
            }
            Message::ClearText(id) => {
                if self.text_id == id {
                    self.text = None;
                }
                Task::none()
            }
            Message::GotMode(mode) => {
                let Some(main_id) = self.main_id() else {
                    return Task::none();
                };
                match mode {
                    window::Mode::Windowed => window::set_mode(main_id, window::Mode::Fullscreen),
                    window::Mode::Fullscreen => window::set_mode(main_id, window::Mode::Windowed),
                    window::Mode::Hidden => Task::none(),
                }
            }
        }
    }

    fn set_text(&mut self, text: String, duration: Duration) -> Task<Message> {
        let text_id = self.text_id + 1;
        self.text_id = text_id;
        self.text = Some(text);
        Task::perform(tokio::time::sleep(duration), move |_| {
            Message::ClearText(text_id)
        })
    }

    fn key_pressed(&mut self, key: Key, modifiers: Modifiers) -> Task<Message> {
        if self.video_hovered {
            match key.as_ref() {
                Named(Space) | Character("k") => {
                    self.player.toggle_playback();
                    Task::none()
                }
                Named(ArrowUp) => self.adjust_volume(0.02),
                Named(ArrowDown) => self.adjust_volume(-0.02),
                Named(ArrowLeft) => {
                    if self.last_seek_time.elapsed() > Duration::from_millis(100) {
                        self.last_seek_time = Instant::now();
                        self.player.seek_rel(-5000);
                    }
                    Task::none()
                }
                Named(ArrowRight) => {
                    if self.last_seek_time.elapsed() > Duration::from_millis(100) {
                        self.last_seek_time = Instant::now();
                        self.player.seek_rel(5000);
                    }
                    Task::none()
                }
                Character("f") => {
                    let Some(main_id) = self.main_id() else {
                        return Task::none();
                    };
                    window::mode(main_id).map(Message::GotMode)
                }
                Character("j") => {
                    if self.last_seek_time.elapsed() > Duration::from_millis(100) {
                        self.last_seek_time = Instant::now();
                        self.player.seek_rel(-10000);
                    }
                    Task::none()
                }
                Character("l") => {
                    if self.last_seek_time.elapsed() > Duration::from_millis(100) {
                        self.last_seek_time = Instant::now();
                        self.player.seek_rel(10000);
                    }
                    Task::none()
                }
                Character("r") => {
                    if modifiers.shift() {
                        self.eof_loop = !self.eof_loop;
                        self.set_text(format!("Loop: {}", self.eof_loop), Duration::from_secs(2))
                    } else {
                        // TODO handle ab looping
                        Task::none()
                    }
                }
                Character("t") => {
                    if let Some(ms) = self.player.position_ms() {
                        let total_second = ms / 1000;
                        let second = total_second % 60;
                        let minute = (total_second / 60) % 60;
                        let hour = total_second / 3600;
                        let text = format!("{hour:02}:{minute:02}:{second:02}");
                        self.set_text(text, Duration::from_secs(2))
                    } else {
                        Task::none()
                    }
                }
                _ => Task::none(),
            }
        } else {
            Task::none()
        }
    }

    fn adjust_volume(&mut self, change: f32) -> Task<Message> {
        let channel1_vol = self.player.modify_volumes(|v| (v + change).clamp(0.0, 1.5));
        let text = format!("Volume {}%", (channel1_vol * 100.0).round() as u32);
        self.set_text(text, Duration::from_secs(2))
    }

    fn view(&self, window: window::Id) -> Element<'_, Message> {
        if Some(window) == self.main_id() {
            let player = container(self.player.view()).style(|_| container::Style {
                background: Some(Background::Color(Color::BLACK)),
                ..Default::default()
            });
            let player_area = mouse_area(player)
                .on_enter(Message::VideoHovered(true))
                .on_exit(Message::VideoHovered(false))
                .on_scroll(Message::Scroll)
                .on_press(Message::KeyPressed(
                    Key::Named(iced::keyboard::key::Named::Space),
                    Modifiers::NONE,
                )); // Not ideal but I don't want to create another message when keypressed space is fine

            if let Some(top_text) = &self.text {
                let message = container(text(top_text).size(25))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .padding(10);
                stack![player_area, message].into()
            } else {
                stack![player_area].into()
            }
        } else {
            iced::widget::space().into()
        }
    }

    fn main_id(&self) -> Option<window::Id> {
        self.windows
            .iter()
            .find(|(_, kind)| matches!(kind, WindowKind::Main))
            .map(|(id, _)| *id)
    }
}
