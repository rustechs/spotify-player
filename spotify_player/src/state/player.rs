use super::model::{
    AlbumId, ArtistId, ContextId, Device, PlaybackMetadata, PlaylistId, ShowId, TracksId,
};
use super::queue::CustomQueue;

/// Player state
#[derive(Default, Debug)]
pub struct PlayerState {
    pub devices: Vec<Device>,

    pub playback: Option<rspotify::model::CurrentPlaybackContext>,
    pub playback_last_updated_time: Option<std::time::Instant>,
    /// A buffered state to speedup the feedback of playback metadata update to user
    // Related issue: https://github.com/aome510/spotify-player/issues/109
    pub buffered_playback: Option<PlaybackMetadata>,

    pub queue: Option<rspotify::model::CurrentUserQueue>,

    /// The currently playing Tracks context (for contexts not tracked by Spotify's playback, e.g. liked/top tracks)
    pub currently_playing_tracks_id: Option<TracksId>,

    /// App-managed custom queue for full playlist/album playback.
    /// Active when the integrated librespot player is streaming and the user
    /// started playback from a track-table context.
    pub custom_queue: Option<CustomQueue>,
}

impl PlayerState {
    /// Get the current playback
    ///
    /// # Note
    /// Because playback metadata stored inside the player state is buffered,
    /// the returned playback is estimated based on the available data.
    pub fn current_playback(&self) -> Option<rspotify::model::CurrentPlaybackContext> {
        let mut playback = self.playback.clone()?;

        // update the playback's progress based on the `playback_last_updated_time`
        playback.progress = estimate_progress(
            playback.progress,
            playback.is_playing,
            self.playback_last_updated_time,
        );

        // update the playback's metadata based on the `buffered_playback` metadata
        if let Some(ref p) = self.buffered_playback {
            playback.device.name.clone_from(&p.device_name);
            playback.device.id.clone_from(&p.device_id);
            playback.is_playing = p.is_playing;
            playback.device.volume_percent = p.volume;
            playback.repeat_state = p.repeat_state;
            playback.shuffle_state = p.shuffle_state;
        }

        Some(playback)
    }

    /// The playback metadata that player commands act on: the buffered copy,
    /// or the playback itself while that copy is missing.
    ///
    /// A new connection clears the buffered copy and the next playback fetch
    /// refills it, which a rate limit can hold back for a minute. Commands
    /// sent in that gap used to fail with "no playback found".
    pub fn playback_metadata(&self) -> Option<PlaybackMetadata> {
        self.buffered_playback
            .clone()
            .or_else(|| self.playback.as_ref().map(PlaybackMetadata::from_playback))
    }

    pub fn currently_playing(&self) -> Option<&rspotify::model::PlayableItem> {
        self.playback.as_ref().and_then(|p| p.item.as_ref())
    }

    pub fn playback_progress(&self) -> Option<chrono::Duration> {
        let playback = self.playback.as_ref()?;
        estimate_progress(
            playback.progress,
            playback.is_playing,
            self.playback_last_updated_time,
        )
    }

    pub fn playing_context_id(&self) -> Option<ContextId> {
        match self.playback {
            Some(ref playback) => match playback.context {
                Some(ref context) => {
                    let uri = crate::utils::parse_uri(&context.uri);
                    match context._type {
                        rspotify::model::Type::Playlist => Some(ContextId::Playlist(
                            PlaylistId::from_uri(&uri).ok()?.into_static(),
                        )),
                        rspotify::model::Type::Album => Some(ContextId::Album(
                            AlbumId::from_uri(&uri).ok()?.into_static(),
                        )),
                        rspotify::model::Type::Artist => Some(ContextId::Artist(
                            ArtistId::from_uri(&uri).ok()?.into_static(),
                        )),
                        rspotify::model::Type::Show => {
                            Some(ContextId::Show(ShowId::from_uri(&uri).ok()?.into_static()))
                        }
                        _ => None,
                    }
                }
                None => self
                    .custom_queue
                    .as_ref()
                    .and_then(|q| q.source_context().cloned())
                    .or_else(|| {
                        self.currently_playing_tracks_id
                            .clone()
                            .map(ContextId::Tracks)
                    }),
            },
            None => None,
        }
    }
}

/// Playback progress advanced by the wall-clock time since the last fetch.
///
/// `progress` is `None` when Spotify reports no `progress_ms` (the Web API
/// documents it as nullable); callers must treat that as unknown, not panic.
fn estimate_progress(
    progress: Option<chrono::Duration>,
    is_playing: bool,
    last_updated: Option<std::time::Instant>,
) -> Option<chrono::Duration> {
    let progress = progress?;
    if !is_playing {
        return Some(progress);
    }
    let elapsed = last_updated
        .and_then(|t| chrono::Duration::from_std(t.elapsed()).ok())
        .unwrap_or_else(chrono::Duration::zero);
    Some(progress + elapsed)
}

#[cfg(test)]
mod tests {
    use super::{estimate_progress, PlayerState};

    /// A paused session on a Connect device, as `/v1/me/player` returns it.
    const PLAYBACK: &str = r#"{
        "device": {
            "id": "0000000000000000000000000000000000000000",
            "is_active": true, "is_private_session": false, "is_restricted": false,
            "name": "test-device", "type": "Computer", "volume_percent": 70
        },
        "repeat_state": "context", "shuffle_state": true, "context": null,
        "timestamp": 1700000000000, "progress_ms": 61000, "is_playing": false,
        "item": null, "currently_playing_type": "track",
        "actions": {"disallows": {}}
    }"#;

    #[test]
    fn playback_metadata_falls_back_to_the_playback_itself() {
        let mut player = PlayerState::default();
        assert!(player.playback_metadata().is_none(), "nothing is playing");

        player.playback = Some(serde_json::from_str(PLAYBACK).unwrap());
        let derived = player
            .playback_metadata()
            .expect("derived from the playback");
        assert_eq!(derived.device_name, "test-device");
        assert_eq!(derived.volume, Some(70));
        assert!(!derived.is_playing);
        assert!(derived.shuffle_state);

        // Once the buffered copy exists it wins: it carries the optimistic
        // result of the last command.
        let mut buffered = derived;
        buffered.is_playing = true;
        player.buffered_playback = Some(buffered);
        assert!(player.playback_metadata().unwrap().is_playing);
    }

    #[test]
    fn estimate_progress_handles_null_progress_and_missing_timestamp() {
        let now = std::time::Instant::now();
        assert_eq!(estimate_progress(None, true, Some(now)), None);
        let paused = chrono::Duration::seconds(30);
        assert_eq!(estimate_progress(Some(paused), false, None), Some(paused));
        assert_eq!(estimate_progress(Some(paused), true, None), Some(paused));
        let earlier = now - std::time::Duration::from_secs(2);
        let playing = estimate_progress(Some(paused), true, Some(earlier)).unwrap();
        assert!(playing >= paused + chrono::Duration::seconds(2));
    }
}
