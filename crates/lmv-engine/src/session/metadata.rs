use ffmpeg_next::{Rational, Stream, codec::Parameters, format::context::Input, media};

pub type TrackId = usize;

#[derive(Debug, Clone)]
pub struct PlayerMeta {
    pub duration: i64,
    pub title: Option<String>,
    pub tracks: Vec<TrackMeta>,
}

#[derive(Clone)]
pub struct TrackMeta {
    pub id: TrackId,
    pub kind: TrackKind,
    pub codec: String,
    pub title: Option<String>,
    pub lang: Option<String>,
    pub params: Parameters,
    pub time_base: Rational,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackKind {
    Audio,
    Video,
    Subtitle,
}

impl PlayerMeta {
    pub(super) fn new(ictx: &Input) -> Self {
        let duration = ictx.duration();
        let title = ictx.metadata().get("title").map(|t| t.to_string());
        let mut tracks = Vec::new();
        for stream in ictx.streams() {
            let Some(track) = TrackMeta::new(&stream) else {
                continue;
            };
            tracks.push(track);
        }
        Self {
            duration,
            title,
            tracks,
        }
    }

    #[must_use]
    pub fn get_track(&self, id: TrackId) -> Option<&TrackMeta> {
        self.tracks.iter().find(|t| t.id == id)
    }

    #[must_use]
    pub fn next_track(&self, kind: TrackKind, current_id: Option<TrackId>) -> Option<&TrackMeta> {
        let tracks: Vec<_> = self.tracks(kind).collect();
        if tracks.is_empty() {
            return None;
        }
        let current_pos = current_id
            .and_then(|id| tracks.iter().position(|t| t.id == id))
            .unwrap_or(0);
        let next_pos = (current_pos + 1) % tracks.len();
        Some(tracks[next_pos])
    }

    pub fn tracks(&self, kind: TrackKind) -> impl Iterator<Item = &TrackMeta> + '_ {
        self.tracks.iter().filter(move |t| t.kind == kind)
    }
}

impl TrackMeta {
    fn new(stream: &Stream) -> Option<Self> {
        let id = stream.index();
        let kind = TrackKind::try_from(stream.parameters().medium()).ok()?;
        let codec = stream.parameters().id().name().to_string();
        let title = stream.metadata().get("title").map(|t| t.to_string());
        let lang = stream.metadata().get("language").map(|l| l.to_string());
        let params = stream.parameters().clone();
        let time_base = stream.time_base();
        Some(Self {
            id,
            kind,
            codec,
            title,
            lang,
            params,
            time_base,
        })
    }
}

impl std::fmt::Debug for TrackMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackMeta")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("codec", &self.codec)
            .field("title", &self.title)
            .field("lang", &self.lang)
            .field("params", &self.params.id())
            .field("time_base", &self.time_base)
            .finish()
    }
}

impl TryFrom<media::Type> for TrackKind {
    type Error = ();

    fn try_from(value: media::Type) -> Result<Self, Self::Error> {
        match value {
            media::Type::Audio => Ok(TrackKind::Audio),
            media::Type::Video => Ok(TrackKind::Video),
            media::Type::Subtitle => Ok(TrackKind::Subtitle),
            _ => Err(()),
        }
    }
}
