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

impl ClientRequest {
    /// Mutating library/queue/playlist actions that should enqueue a TUI toast.
    #[allow(dead_code)] // Phase 0: used by tests; handlers enqueue after the Phase 0 gate
    pub fn is_toastable(&self) -> bool {
        match self {
            Self::AddPlayableToQueue(_)
            | Self::AddAlbumToQueue(_)
            | Self::AddPlayableToPlaylist(_, _)
            | Self::DeleteTrackFromPlaylist(_, _)
            | Self::ReorderPlaylistItems { .. }
            | Self::AddToLibrary(_)
            | Self::DeleteFromLibrary(_)
            | Self::CreatePlaylist { .. } => true,
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
            | Self::GetLyrics { .. } => false,
            #[cfg(feature = "streaming")]
            Self::RestartIntegratedClient => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Artist, ArtistId, Category, ContextId, Item, ItemId, TracksId};

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
    }
}
