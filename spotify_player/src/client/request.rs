use crate::state::{
    AlbumId, Category, ContextId, Item, ItemId, PlayableId, Playback, PlaylistId, TrackId,
};

#[derive(Clone, Debug)]
/// A request that modifies the player's playback
pub enum PlayerRequest {
    NextTrack,
    PreviousTrack,
    Resume,
    Pause,
    ResumePause,
    SeekTrack(chrono::Duration),
    Repeat,
    Shuffle,
    Volume(u8),
    ToggleMute,
    TransferPlayback(String, bool),
    StartPlayback(Playback, Option<bool>),
}

#[derive(Clone, Debug)]
/// A request to the client
pub enum ClientRequest {
    GetCurrentUser,
    GetDevices,
    GetBrowseCategories,
    GetBrowseCategoryPlaylists(Category),
    GetUserPlaylists,
    GetUserSavedAlbums,
    GetUserSavedShows,
    GetUserFollowedArtists,
    GetContext(ContextId),
    GetCurrentPlayback,
    Search(String),
    AddPlayableToQueue(PlayableId<'static>),
    AddAlbumToQueue(AlbumId<'static>),
    AddPlayableToPlaylist(PlaylistId<'static>, PlayableId<'static>),
    DeleteTrackFromPlaylist(PlaylistId<'static>, TrackId<'static>),
    ReorderPlaylistItems {
        playlist_id: PlaylistId<'static>,
        insert_index: usize,
        range_start: usize,
        range_length: Option<usize>,
        snapshot_id: Option<String>,
    },
    AddToLibrary(Item),
    DeleteFromLibrary(ItemId),
    Player(PlayerRequest),
    GetCurrentUserQueue,
    GetLyrics {
        track_id: TrackId<'static>,
    },
    #[cfg(feature = "streaming")]
    RestartIntegratedClient,
    CreatePlaylist {
        playlist_name: String,
        public: bool,
        collab: bool,
        desc: String,
    },
}

impl PlayerRequest {
    /// What the request does, in the words shown to the user.
    pub fn action(&self) -> &'static str {
        match self {
            Self::NextTrack => "next track",
            Self::PreviousTrack => "previous track",
            Self::Resume | Self::Pause | Self::ResumePause => "play/pause",
            Self::SeekTrack(_) => "seek",
            Self::Repeat => "repeat",
            Self::Shuffle => "shuffle",
            Self::Volume(_) | Self::ToggleMute => "volume",
            Self::TransferPlayback(..) => "device switch",
            Self::StartPlayback(..) => "playback start",
        }
    }

    /// Whether sending the request a second time undoes the first.
    pub fn is_toggle(&self) -> bool {
        matches!(
            self,
            Self::ResumePause | Self::Repeat | Self::Shuffle | Self::ToggleMute
        )
    }
}

impl ClientRequest {
    /// What a request the user made does, in the words shown to them; `None`
    /// for the requests the app makes on its own.
    pub fn action(&self) -> Option<&'static str> {
        match self {
            Self::Player(request) => Some(request.action()),
            _ if self.is_toastable() => Some("the change"),
            _ => None,
        }
    }

    /// Requests whose outcome is shown as a toast: mutating library/queue/
    /// playlist actions, starting playback, and skip next/previous.
    pub fn is_toastable(&self) -> bool {
        self.toast_success_message().is_some()
    }

    /// Success toast text; `None` for requests that never toast.
    pub fn toast_success_message(&self) -> Option<&'static str> {
        match self {
            Self::AddPlayableToQueue(_) | Self::AddAlbumToQueue(_) => Some("Added to queue"),
            Self::AddPlayableToPlaylist(_, _) => Some("Added to playlist"),
            Self::DeleteTrackFromPlaylist(_, _) => Some("Removed from playlist"),
            Self::ReorderPlaylistItems { .. } => Some("Reordered playlist"),
            Self::AddToLibrary(Item::Track(_)) => Some("Liked"),
            Self::AddToLibrary(_) => Some("Added to library"),
            Self::DeleteFromLibrary(ItemId::Track(_)) => Some("Unliked"),
            Self::DeleteFromLibrary(_) => Some("Removed from library"),
            Self::CreatePlaylist { .. } => Some("Created playlist"),
            Self::Player(PlayerRequest::StartPlayback(_, _)) => Some("Playing"),
            Self::Player(PlayerRequest::NextTrack) => Some("Skipped to next"),
            Self::Player(PlayerRequest::PreviousTrack) => Some("Skipped to previous"),
            Self::GetCurrentUser
            | Self::GetDevices
            | Self::GetBrowseCategories
            | Self::GetBrowseCategoryPlaylists(_)
            | Self::GetUserPlaylists
            | Self::GetUserSavedAlbums
            | Self::GetUserSavedShows
            | Self::GetUserFollowedArtists
            | Self::GetContext(_)
            | Self::GetCurrentPlayback
            | Self::Search(_)
            | Self::Player(_)
            | Self::GetCurrentUserQueue
            | Self::GetLyrics { .. } => None,
            #[cfg(feature = "streaming")]
            Self::RestartIntegratedClient => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Artist, ArtistId, Category, ContextId, Item, ItemId, Playback, TracksId};

    fn track_id() -> TrackId<'static> {
        TrackId::from_id("3n3Ppam7vgaVa1iaRUc9Lp")
            .unwrap()
            .into_static()
    }

    fn playlist_id() -> PlaylistId<'static> {
        PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M")
            .unwrap()
            .into_static()
    }

    fn album_id() -> AlbumId<'static> {
        AlbumId::from_id("4aawyAB9vmqN3uQ7FjRGTy")
            .unwrap()
            .into_static()
    }

    #[test]
    fn requests_the_user_made_have_a_name_and_background_ones_do_not() {
        assert_eq!(
            ClientRequest::Player(PlayerRequest::ResumePause).action(),
            Some("play/pause")
        );
        assert_eq!(
            ClientRequest::Player(PlayerRequest::Volume(40)).action(),
            Some("volume")
        );
        assert_eq!(
            ClientRequest::AddPlayableToQueue(PlayableId::Track(track_id())).action(),
            Some("the change")
        );
        assert_eq!(ClientRequest::GetCurrentPlayback.action(), None);
        assert_eq!(ClientRequest::GetUserPlaylists.action(), None);
    }

    #[test]
    fn only_requests_that_undo_themselves_are_toggles() {
        assert!(PlayerRequest::ResumePause.is_toggle());
        assert!(PlayerRequest::Shuffle.is_toggle());
        assert!(PlayerRequest::Repeat.is_toggle());
        assert!(PlayerRequest::ToggleMute.is_toggle());
        assert!(!PlayerRequest::Pause.is_toggle());
        assert!(!PlayerRequest::NextTrack.is_toggle());
        assert!(!PlayerRequest::Volume(40).is_toggle());
    }

    #[test]
    fn client_request_is_toastable() {
        let tid = track_id();
        let pid = playlist_id();
        let aid = album_id();
        let playable = PlayableId::Track(tid.clone());

        let toastable = [
            ClientRequest::AddPlayableToQueue(playable.clone()),
            ClientRequest::AddAlbumToQueue(aid.clone()),
            ClientRequest::AddPlayableToPlaylist(pid.clone(), playable.clone()),
            ClientRequest::DeleteTrackFromPlaylist(pid.clone(), tid.clone()),
            ClientRequest::ReorderPlaylistItems {
                playlist_id: pid.clone(),
                insert_index: 0,
                range_start: 1,
                range_length: None,
                snapshot_id: None,
            },
            ClientRequest::AddToLibrary(Item::Artist(Artist {
                id: ArtistId::from_id("0OdUWJ0sBjDrqHygGUXeCF")
                    .unwrap()
                    .into_static(),
                name: "a".into(),
            })),
            ClientRequest::DeleteFromLibrary(ItemId::Track(tid.clone())),
            ClientRequest::CreatePlaylist {
                playlist_name: "p".into(),
                public: false,
                collab: false,
                desc: String::new(),
            },
            ClientRequest::Player(PlayerRequest::StartPlayback(
                Playback::URIs(vec![track_id().into()], None),
                None,
            )),
            ClientRequest::Player(PlayerRequest::NextTrack),
            ClientRequest::Player(PlayerRequest::PreviousTrack),
        ];
        for req in &toastable {
            assert!(req.is_toastable(), "expected toastable: {req:?}");
        }

        let silent = [
            ClientRequest::GetCurrentUser,
            ClientRequest::GetDevices,
            ClientRequest::GetBrowseCategories,
            ClientRequest::GetBrowseCategoryPlaylists(Category {
                id: "pop".into(),
                name: "Pop".into(),
            }),
            ClientRequest::GetUserPlaylists,
            ClientRequest::GetUserSavedAlbums,
            ClientRequest::GetUserSavedShows,
            ClientRequest::GetUserFollowedArtists,
            ClientRequest::GetContext(ContextId::Tracks(TracksId {
                uri: "spotify:user:me:collection".into(),
                kind: "collection".into(),
            })),
            ClientRequest::GetCurrentPlayback,
            ClientRequest::Search("q".into()),
            ClientRequest::Player(PlayerRequest::Volume(10)),
            ClientRequest::GetCurrentUserQueue,
            ClientRequest::GetLyrics {
                track_id: tid.clone(),
            },
        ];
        for req in &silent {
            assert!(!req.is_toastable(), "expected silent: {req:?}");
        }

        #[cfg(feature = "streaming")]
        assert!(!ClientRequest::RestartIntegratedClient.is_toastable());

        assert_eq!(
            ClientRequest::Player(PlayerRequest::StartPlayback(
                Playback::URIs(vec![track_id().into()], None),
                None,
            ))
            .toast_success_message(),
            Some("Playing")
        );
        assert_eq!(
            ClientRequest::Player(PlayerRequest::NextTrack).toast_success_message(),
            Some("Skipped to next")
        );
        assert_eq!(
            ClientRequest::Player(PlayerRequest::PreviousTrack).toast_success_message(),
            Some("Skipped to previous")
        );
        assert_eq!(
            ClientRequest::AddPlayableToQueue(playable).toast_success_message(),
            Some("Added to queue")
        );
    }
}
