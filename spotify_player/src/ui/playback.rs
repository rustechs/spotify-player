use super::{
    config, utils::construct_and_render_block, Alignment, Borders, Constraint, Frame, Gauge,
    Layout, Line, LineGauge, Modifier, Paragraph, PlaybackMetadata, Rect, SharedState, Span, Style,
    Text, UIStateGuard, Wrap,
};
#[cfg(feature = "image")]
use crate::state::ImageRenderInfo;
use crate::{
    state::Track,
    ui::utils::{format_genres, to_bidi_string},
};
use rspotify::model::Id;

/// Playback fields needed to render the window, cloned under a short `player` read.
struct ActivePlayback {
    item: rspotify::model::PlayableItem,
    buffered_playback: Option<PlaybackMetadata>,
    progress: chrono::Duration,
    duration: chrono::Duration,
}

/// Render a playback window showing information about the current playback, which includes
/// - track title, artists, album
/// - playback metadata (playing state, repeat state, shuffle state, volume, device, etc)
/// - cover image (if `image` feature is enabled)
/// - playback progress bar
pub fn render_playback_window(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) -> Rect {
    let (rect, other_rect) = split_rect_for_playback_window(state, rect);
    let rect = construct_and_render_block("Playback", &ui.theme, Borders::ALL, frame, rect);

    // Snapshot playback under a short `player` read. Holding the RwLock across cover
    // encode / viz / data reads lets a waiting writer (playback refresh) block *new*
    // readers under parking_lot's fair policy — parking every tokio worker on the lock
    // and freezing the TUI + CLI socket.
    let (active, waiting_for_first_fetch) = {
        let player = state.player.read();
        let waiting_for_first_fetch = player.playback_last_updated_time.is_none();
        let active = player
            .playback
            .as_ref()
            .and_then(|p| p.item.clone())
            .and_then(|item| {
                let duration = match &item {
                    rspotify::model::PlayableItem::Track(track) => Some(track.duration),
                    rspotify::model::PlayableItem::Episode(episode) => Some(episode.duration),
                    rspotify::model::PlayableItem::Unknown(unknown) => {
                        log::warn!("Unknown playback item: {unknown:?}");
                        None
                    }
                }?;
                let progress = std::cmp::min(
                    player.playback_progress().expect("non-empty playback"),
                    duration,
                );
                Some(ActivePlayback {
                    item,
                    buffered_playback: player.buffered_playback.clone(),
                    progress,
                    duration,
                })
            });
        (active, waiting_for_first_fetch)
    };

    if let Some(ActivePlayback {
        item,
        buffered_playback,
        progress,
        duration,
    }) = active
    {
        // Carve off the visualization rows here, inside the active-playback
        // branch, so the full rect is used when there is nothing playing.
        // Keep the area reserved while a track is loaded (including pause) so
        // the layout does not jump; bars idle at zero when audio is silent.
        // With visualization enabled the progress bar is always placed below
        // the spectrogram, regardless of `progress_bar_position`.
        #[cfg(feature = "streaming")]
        let (rect, vis_rect, progress_override) = {
            let configs = config::get_config();
            if configs.app_config.enable_audio_visualization {
                let chunks = Layout::vertical([
                    Constraint::Length(playback_format_line_count()),
                    Constraint::Length(super::streaming::VIS_HEIGHT),
                    Constraint::Length(1),
                ])
                .split(rect);
                (chunks[0], Some(chunks[1]), Some(chunks[2]))
            } else {
                (rect, None, None)
            }
        };

        #[cfg(not(feature = "streaming"))]
        let progress_override: Option<Rect> = None;

        let (metadata_rect, progress_bar_rect) = {
            // Render the track's cover image if `image` feature is enabled
            #[cfg(feature = "image")]
            {
                let configs = config::get_config();
                // Split the allocated rectangle into `metadata_rect`, `cover_img_rect` and `progress_bar_rect`
                let (metadata_rect, cover_img_rect, progress_bar_rect) =
                    if let Some(progress) = progress_override {
                        let hor_chunks = split_rect_for_cover_img(rect, &ui.picker);
                        (hor_chunks.1, hor_chunks.0, progress)
                    } else {
                        match configs.app_config.progress_bar_position {
                            config::ProgressBarPosition::Bottom => {
                                let ver_chunks = split_rect_for_progress_bar(rect); // rect, progress_bar_rect
                                let hor_chunks = split_rect_for_cover_img(ver_chunks.0, &ui.picker); // cover_img_rect, metadata_rect
                                (hor_chunks.1, hor_chunks.0, ver_chunks.1)
                            }
                            config::ProgressBarPosition::Right => {
                                let hor_chunks = split_rect_for_cover_img(rect, &ui.picker); // cover_img_rect, rect
                                let ver_chunks = split_rect_for_progress_bar(hor_chunks.1); // metadata_rect, progress_bar_rect
                                (ver_chunks.0, hor_chunks.0, ver_chunks.1)
                            }
                        }
                    };

                let url = match &item {
                    rspotify::model::PlayableItem::Track(track) => {
                        crate::utils::get_track_album_image_url(track).map(String::from)
                    }
                    rspotify::model::PlayableItem::Episode(episode) => {
                        crate::utils::get_episode_show_image_url(episode).map(String::from)
                    }
                    rspotify::model::PlayableItem::Unknown(_) => None,
                };
                if let Some(url) = url {
                    let data = state.data.read();
                    if let Some(img) = data.caches.images.get(&url) {
                        if ui.last_cover_image_render_info.url != url
                            || ui.last_cover_image_render_info.render_area != cover_img_rect
                        {
                            let state = match crate::ui::cover_image::CoverImage::new(
                                &ui.picker,
                                img,
                                cover_img_rect,
                            ) {
                                Ok(cover) => Some(cover),
                                Err(err) => {
                                    tracing::error!("Failed to encode cover image: {err:#}");
                                    None
                                }
                            };
                            ui.last_cover_image_render_info = ImageRenderInfo {
                                url,
                                render_area: cover_img_rect,
                                state,
                            };
                        }
                        let area = ui.last_cover_image_render_info.render_area;
                        if let Some(cover) = ui.last_cover_image_render_info.state.as_mut() {
                            cover.render(frame, area);
                        }
                    }
                }
                (metadata_rect, progress_bar_rect)
            }

            #[cfg(not(feature = "image"))]
            {
                if let Some(progress) = progress_override {
                    (rect, progress)
                } else {
                    let chunks = split_rect_for_progress_bar(rect);
                    (chunks.0, chunks.1)
                }
            }
        };

        if let Some(ref buffered) = buffered_playback {
            let playback_text = construct_playback_text(ui, state, &item, buffered);
            let playback_desc = Paragraph::new(playback_text);
            frame.render_widget(playback_desc, metadata_rect);
        }

        #[cfg(feature = "streaming")]
        if let Some(vis_r) = vis_rect {
            super::streaming::render_audio_visualization(frame, state, &ui.theme, vis_r);
        }
        render_playback_progress_bar(frame, ui, progress, duration, progress_bar_rect);
        return other_rect;
    }

    // Previously rendered image can result in a weird rendering text,
    // clear the previous widget's area before rendering the text.
    #[cfg(feature = "image")]
    {
        ui.last_cover_image_render_info = ImageRenderInfo::default();
    }

    if waiting_for_first_fetch {
        // Still waiting for the first successful playback fetch — show animated loading indicator
        const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame_idx = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            / 100) as usize
            % SPINNER_FRAMES.len();
        let vertical_chunks = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(rect);
        frame.render_widget(
            Paragraph::new(format!("{} Loading...", SPINNER_FRAMES[frame_idx]))
                .style(ui.theme.playback_metadata())
                .alignment(Alignment::Center),
            vertical_chunks[1],
        );
    } else {
        frame.render_widget(
            Paragraph::new(
                "No playback found. Please start a new playback.\n \
                 Make sure there is a running Spotify device and try to connect to one using the `SwitchDevice` command."
            )
            .wrap(Wrap { trim: true }),
            rect,
        );
    }

    other_rect
}

/// Line count implied by `playback_format` (one plus the number of `\n` separators).
#[cfg(feature = "streaming")]
fn playback_format_line_count() -> u16 {
    let format_str = &config::get_config().app_config.playback_format;
    format_str.bytes().filter(|&b| b == b'\n').count() as u16 + 1
}

fn split_rect_for_progress_bar(rect: Rect) -> (Rect, Rect) {
    let chunks = Layout::vertical([Constraint::Fill(0), Constraint::Length(1)]).split(rect);
    (chunks[0], chunks[1])
}

#[cfg(feature = "image")]
fn split_rect_for_cover_img(rect: Rect, picker: &ratatui_image::picker::Picker) -> (Rect, Rect) {
    let configs = config::get_config();
    let hor_chunks = Layout::horizontal([
        Constraint::Length(cover_img_length(configs, picker)),
        Constraint::Fill(0), // metadata_rect
    ])
    .spacing(1)
    .split(rect);
    let ver_chunks = Layout::vertical([
        Constraint::Length(configs.app_config.cover_img_width as u16), // cover_img_rect
    ])
    .split(hor_chunks[0]);

    (ver_chunks[0], hor_chunks[1])
}

/// Determine the cover image box's width in columns.
#[cfg(feature = "image")]
fn cover_img_length(configs: &config::Configs, picker: &ratatui_image::picker::Picker) -> u16 {
    match configs.app_config.cover_img_length {
        // When `cover_img_length` is `0` (the default), derive it from the terminal's cell aspect ratio
        0 => {
            let font_size = picker.font_size();
            let rows = configs.app_config.cover_img_width as u16;
            rows * font_size.height / font_size.width
        }
        length => length as u16,
    }
}

fn construct_playback_text(
    ui: &UIStateGuard,
    state: &SharedState,
    playable: &rspotify::model::PlayableItem,
    playback: &PlaybackMetadata,
) -> Text<'static> {
    // Construct a "styled" text (`playback_text`) from playback's data
    // based on a user-configurable format string (app_config.playback_format)
    let configs = config::get_config();
    let format_str = &configs.app_config.playback_format;
    let data = state.data.read();

    let mut playback_text = Text::default();
    let mut spans = vec![];

    // this regex is to handle a format argument or a newline
    let re = regex::Regex::new(r"\{.*?\}|\n").unwrap();

    let mut ptr = 0;
    for m in re.find_iter(format_str) {
        let s = m.start();
        let e = m.end();
        if ptr < s {
            spans.push(Span::raw(format_str[ptr..s].to_string()));
        }
        ptr = e;

        let (text, style) = match m.as_str() {
            // upon encountering a newline, create a new `Spans`
            "\n" => {
                let mut tmp = vec![];
                std::mem::swap(&mut tmp, &mut spans);
                playback_text.lines.push(Line::from(tmp));
                continue;
            }
            "{status}" => (
                if playback.is_playing {
                    &configs.app_config.play_icon
                } else {
                    &configs.app_config.pause_icon
                }
                .to_owned(),
                ui.theme.playback_status(),
            ),
            "{liked}" => match playable {
                rspotify::model::PlayableItem::Track(track) => match &track.id {
                    Some(id) => {
                        if data.user_data.saved_tracks.contains_key(&id.uri()) {
                            (configs.app_config.liked_icon.clone(), ui.theme.like())
                        } else {
                            continue;
                        }
                    }
                    None => continue,
                },
                rspotify::model::PlayableItem::Episode(_)
                | rspotify::model::PlayableItem::Unknown(_) => continue,
            },
            "{track}" => match playable {
                rspotify::model::PlayableItem::Track(track) => (
                    {
                        let display = Track::try_from_full_track(track.clone()).map_or_else(
                            || "Unknown Track".to_string(),
                            |t| to_bidi_string(&t.display_name()),
                        );
                        display
                    },
                    ui.theme.playback_track(),
                ),
                rspotify::model::PlayableItem::Episode(episode) => (
                    {
                        let bidi_string = to_bidi_string(&episode.name);
                        if episode.explicit {
                            format!("{bidi_string} (E)")
                        } else {
                            bidi_string
                        }
                    },
                    ui.theme.playback_track(),
                ),
                rspotify::model::PlayableItem::Unknown(_) => {
                    continue;
                }
            },
            "{track_number}" => match playable {
                rspotify::model::PlayableItem::Track(track) => (
                    { to_bidi_string(&track.track_number.to_string()) },
                    ui.theme.playback_track(),
                ),
                rspotify::model::PlayableItem::Episode(_)
                | rspotify::model::PlayableItem::Unknown(_) => {
                    continue;
                }
            },
            "{artists}" => match playable {
                rspotify::model::PlayableItem::Track(track) => (
                    to_bidi_string(&crate::utils::map_join(&track.artists, |a| &a.name, ", ")),
                    ui.theme.playback_artists(),
                ),
                rspotify::model::PlayableItem::Episode(episode) => {
                    (episode.show.publisher.clone(), ui.theme.playback_artists())
                }
                rspotify::model::PlayableItem::Unknown(_) => {
                    continue;
                }
            },
            "{album}" => match playable {
                rspotify::model::PlayableItem::Track(track) => {
                    (to_bidi_string(&track.album.name), ui.theme.playback_album())
                }
                rspotify::model::PlayableItem::Episode(episode) => (
                    to_bidi_string(&episode.show.name),
                    ui.theme.playback_album(),
                ),
                rspotify::model::PlayableItem::Unknown(_) => {
                    continue;
                }
            },
            "{genres}" => match playable {
                rspotify::model::PlayableItem::Track(full_track) => {
                    let genre = match data.caches.genres.get(&full_track.artists[0].name) {
                        Some(genres) => &format_genres(genres, configs.app_config.genre_num),
                        None => "no genre",
                    };
                    (to_bidi_string(genre), ui.theme.playback_genres())
                }
                rspotify::model::PlayableItem::Episode(_) => {
                    (to_bidi_string("no genre"), ui.theme.playback_genres())
                }
                rspotify::model::PlayableItem::Unknown(_) => {
                    continue;
                }
            },
            "{metadata}" => {
                let repeat_value = <&'static str>::from(playback.repeat_state).to_string();

                let volume_value = if let Some(volume) = playback.mute_state {
                    format!("{volume}% (muted)")
                } else {
                    format!("{}%", playback.volume.unwrap_or_default())
                };

                let mut parts = vec![];

                for field in &configs.app_config.playback_metadata_fields {
                    match field.as_str() {
                        "repeat" => parts.push(format!("repeat: {repeat_value}")),
                        "shuffle" => parts.push(format!("shuffle: {}", playback.shuffle_state)),
                        "volume" => parts.push(format!("volume: {volume_value}")),
                        "device" => parts.push(format!("device: {}", playback.device_name)),
                        _ => {}
                    }
                }

                let metadata_str = parts.join(" | ");
                (metadata_str, ui.theme.playback_metadata())
            }
            _ => continue,
        };

        spans.push(Span::styled(text, style));
    }
    if ptr < format_str.len() {
        spans.push(Span::raw(format_str[ptr..].to_string()));
    }
    if !spans.is_empty() {
        playback_text.lines.push(Line::from(spans));
    }

    playback_text
}

fn render_playback_progress_bar(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    progress: chrono::Duration,
    duration: chrono::Duration,
    rect: Rect,
) {
    // Negative numbers can sometimes appear from progress.num_seconds() so this stops
    // them coming through into the ratios
    let ratio = (progress.num_seconds() as f64 / duration.num_seconds() as f64).clamp(0.0, 1.0);

    match config::get_config().app_config.progress_bar_type {
        config::ProgressBarType::Line => frame.render_widget(
            LineGauge::default()
                .filled_style(ui.theme.playback_progress_bar())
                .unfilled_style(ui.theme.playback_progress_bar_unfilled())
                .ratio(ratio)
                .label(Span::styled(
                    format!(
                        "{}/{}",
                        crate::utils::format_duration(&progress),
                        crate::utils::format_duration(&duration),
                    ),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
            rect,
        ),
        config::ProgressBarType::Rectangle => frame.render_widget(
            Gauge::default()
                .gauge_style(ui.theme.playback_progress_bar())
                .ratio(ratio)
                .label(Span::styled(
                    format!(
                        "{}/{}",
                        crate::utils::format_duration(&progress),
                        crate::utils::format_duration(&duration),
                    ),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
            rect,
        ),
    }

    // update the progress bar's position stored inside the UI state
    ui.playback_progress_bar_rect = rect;
}

/// Split the given area into two, the first one for the playback window
/// and the second one for the main application's layout (popup, page, etc).
#[allow(unused_variables)]
fn split_rect_for_playback_window(state: &SharedState, rect: Rect) -> (Rect, Rect) {
    let configs = config::get_config();
    // When visualization is enabled, size the window to fit metadata, progress, and
    // the chart tightly instead of inheriting slack from `playback_window_height`.
    #[cfg(feature = "streaming")]
    let playback_width = if configs.app_config.enable_audio_visualization
        && state.player.read().currently_playing().is_some()
    {
        playback_format_line_count() as usize + super::streaming::VIS_HEIGHT as usize + 1
    } else {
        configs.app_config.layout.playback_window_height
    };

    #[cfg(not(feature = "streaming"))]
    let playback_width = configs.app_config.layout.playback_window_height;

    // the playback window's width should not be smaller than the cover image's width + 1
    #[cfg(feature = "image")]
    let playback_width = std::cmp::max(configs.app_config.cover_img_width + 1, playback_width);

    // add lines for top/bottom borders depending on the progress bar's position
    #[cfg(feature = "streaming")]
    let num_lines = if configs.app_config.enable_audio_visualization
        && state.player.read().currently_playing().is_some()
    {
        2
    } else {
        match configs.app_config.progress_bar_position {
            config::ProgressBarPosition::Bottom => 2,
            config::ProgressBarPosition::Right => 1,
        }
    };

    #[cfg(not(feature = "streaming"))]
    let num_lines = match configs.app_config.progress_bar_position {
        config::ProgressBarPosition::Bottom => 2,
        config::ProgressBarPosition::Right => 1,
    };
    let playback_width = (playback_width + num_lines) as u16;

    match configs.app_config.layout.playback_window_position {
        config::Position::Top => {
            let chunks =
                Layout::vertical([Constraint::Length(playback_width), Constraint::Fill(0)])
                    .split(rect);

            (chunks[0], chunks[1])
        }
        config::Position::Bottom => {
            let chunks =
                Layout::vertical([Constraint::Fill(0), Constraint::Length(playback_width)])
                    .split(rect);

            (chunks[1], chunks[0])
        }
    }
}
