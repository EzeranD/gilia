use std::sync::Arc;

use lmv_engine::{
    EngineConfig, ExternalEvent, PlaybackMode, PlaybackPhase, PlayerEngine, PlayerEvent,
};
use lmv_iced::widget::PlayerWidget;

pub struct Player {
    engine: PlayerEngine,
}

impl Player {
    pub fn new(
        path: String,
        config: EngineConfig,
        callback: Arc<dyn Fn(ExternalEvent) + Send + Sync>,
    ) -> Self {
        let mut engine = PlayerEngine::new(config);
        engine.set_callback(callback);
        let _ = engine.open(path);
        Self { engine }
    }

    #[cfg(feature = "iced")]
    pub fn view(&self) -> PlayerWidget<'_> {
        PlayerWidget::new(&self.engine)
    }

    pub fn play(&mut self) {
        self.engine.apply_event(PlayerEvent::Play);
    }

    pub fn pause(&mut self) {
        self.engine.apply_event(PlayerEvent::Pause);
    }

    pub fn toggle_playback(&mut self) {
        let state = self.engine.state.load();
        match state.mode {
            PlaybackMode::Playing => {
                self.pause();
            }
            PlaybackMode::Paused => {
                self.play();
            }
            PlaybackMode::Stopped => {}
        }
    }

    pub fn seek_to(&mut self, pos_ms: i64) {
        let state = self.engine.state.load();
        if !matches!(state.phase, PlaybackPhase::Seeking(_)) {
            self.engine.apply_event(PlayerEvent::Seek(pos_ms));
        }
    }

    pub fn seek_rel(&mut self, offset_ms: i64) {
        let state = self.engine.state.load();
        if !matches!(state.phase, PlaybackPhase::Seeking(_)) {
            self.engine.apply_event(PlayerEvent::Seek(
                self.position_ms().unwrap_or(0) + offset_ms,
            ));
        }
    }

    #[allow(dead_code)]
    pub fn get_volumes(&self) -> Vec<f32> {
        let volumes = self.engine.audio_info.volume.lock().unwrap();
        volumes.clone()
    }

    #[allow(dead_code)]
    pub fn set_volumes(&mut self, volumes: Vec<f32>) {
        self.engine.apply_event(PlayerEvent::ChangeVolumes(volumes));
    }

    pub fn modify_volumes(&mut self, f: impl Fn(f32) -> f32) -> f32 {
        let mut volumes = self.engine.audio_info.volume.lock().unwrap();
        for v in volumes.iter_mut() {
            *v = f(*v);
        }
        self.engine
            .apply_event(PlayerEvent::ChangeVolumes(volumes.clone()));
        volumes.first().copied().unwrap_or(0.0)
    }

    pub fn position_ms(&self) -> Option<i64> {
        self.engine.position_ms()
    }
}
