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
/// - cover image (if `image` feature is enabled)
/// - playback progress bar
/// - playback status (repeat, shuffle, volume, device) below the progress bar
pub fn render_playback_window(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) -> Rect {
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

    let (outer, other_rect) = split_rect_for_playback_window(state, rect);
    let inner = construct_and_render_block("Playback", &ui.theme, Borders::ALL, frame, outer);

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
        // the spectrogram, regardless of `progress_bar_position`. Repeat /
        // shuffle / volume / device occupy the last inner row so the block
        // keeps a solid bottom border.
        let (content, status_rect) = split_status_row(inner);

        #[cfg(feature = "streaming")]
        let (rect, vis_rect, progress_override) = {
            let configs = config::get_config();
            if configs.app_config.enable_audio_visualization {
                let (header, vis, progress) =
                    split_viz_playback_rows(content, playback_format_line_count());
                (header, Some(vis), Some(progress))
            } else {
                (content, None, None)
            }
        };

        #[cfg(not(feature = "streaming"))]
        let (rect, progress_override): (Rect, Option<Rect>) = (content, None);

        // Cover is prepared here but rendered after the visualizer so it can paint
        // over the dead top-right of the spectrogram.
        #[cfg(feature = "image")]
        let (metadata_rect, progress_bar_rect) = {
            let configs = config::get_config();
            #[cfg(feature = "streaming")]
            let (metadata_rect, cover_img_rect, progress_bar_rect) =
                if let (Some(progress), Some(vis_r)) = (progress_override, vis_rect) {
                    let combined = Rect {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: rect.height.saturating_add(vis_r.height),
                    };
                    let (cover, metadata) =
                        split_cover_over_visualizer(combined, rect.height, &ui.picker);
                    (metadata, cover, progress)
                } else {
                    split_cover_with_progress_bar(rect, configs, &ui.picker)
                };

            #[cfg(not(feature = "streaming"))]
            let (metadata_rect, cover_img_rect, progress_bar_rect) =
                split_cover_with_progress_bar(rect, configs, &ui.picker);

            prepare_cover_image(state, ui, &item, cover_img_rect);
            (metadata_rect, progress_bar_rect)
        };

        #[cfg(not(feature = "image"))]
        let (metadata_rect, progress_bar_rect) = {
            if let Some(progress) = progress_override {
                (rect, progress)
            } else {
                split_rect_for_progress_bar(rect)
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

        // Draw the cover last so it occludes the visualizer's top-right corner.
        #[cfg(feature = "image")]
        {
            let area = ui.last_cover_image_render_info.render_area;
            if let Some(cover) = ui.last_cover_image_render_info.state.as_mut() {
                cover.render(frame, area);
            }
        }

        render_playback_progress_bar(frame, ui, progress, duration, progress_bar_rect);
        if let Some(ref buffered) = buffered_playback {
            render_playback_status_row(frame, ui, buffered, status_rect);
        }
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
        .split(inner);
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
            inner,
        );
    }

    other_rect
}

/// `{metadata}` is rendered on its own row below the progress bar, so drop it
/// (and any blank lines that leaves) from the header format string.
fn playback_header_format() -> String {
    collapse_format_newlines(
        &config::get_config()
            .app_config
            .playback_format
            .replace("{metadata}", ""),
    )
}

fn collapse_format_newlines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_nl = false;
    for c in s.chars() {
        if c == '\n' {
            if prev_nl {
                continue;
            }
            prev_nl = true;
            out.push(c);
        } else {
            prev_nl = false;
            out.push(c);
        }
    }
    out.trim_matches('\n').to_string()
}

/// Line count of the header format (one plus the number of `\n` separators).
#[cfg(feature = "streaming")]
fn playback_format_line_count() -> u16 {
    let format_str = playback_header_format();
    if format_str.is_empty() {
        0
    } else {
        format_str.bytes().filter(|&b| b == b'\n').count() as u16 + 1
    }
}

/// Split off the last inner row for repeat/shuffle/volume/device so the block
/// can keep a solid bottom border underneath.
fn split_status_row(rect: Rect) -> (Rect, Rect) {
    let status = Rect {
        x: rect.x,
        y: rect.bottom().saturating_sub(1),
        width: rect.width,
        height: 1.min(rect.height),
    };
    let content = Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height.saturating_sub(status.height),
    };
    (content, status)
}

/// Split the visualization playback inner area: header, chart, progress flush
/// to the bottom (immediately above the status row).
#[cfg(feature = "streaming")]
fn split_viz_playback_rows(rect: Rect, header_h: u16) -> (Rect, Rect, Rect) {
    let progress = Rect {
        x: rect.x,
        y: rect.bottom().saturating_sub(1),
        width: rect.width,
        height: 1.min(rect.height),
    };
    let header = Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: header_h.min(progress.y.saturating_sub(rect.y)),
    };
    let vis = Rect {
        x: rect.x,
        y: header.bottom(),
        width: rect.width,
        height: progress.y.saturating_sub(header.bottom()),
    };
    (header, vis, progress)
}

fn split_rect_for_progress_bar(rect: Rect) -> (Rect, Rect) {
    let chunks = Layout::vertical([Constraint::Fill(0), Constraint::Length(1)]).split(rect);
    (chunks[0], chunks[1])
}

/// Split `rect` for cover + metadata + progress using `progress_bar_position`.
///
/// Returns `(metadata, cover, progress)`.
#[cfg(feature = "image")]
fn split_cover_with_progress_bar(
    rect: Rect,
    configs: &config::Configs,
    picker: &ratatui_image::picker::Picker,
) -> (Rect, Rect, Rect) {
    match configs.app_config.progress_bar_position {
        config::ProgressBarPosition::Bottom => {
            let (above, progress) = split_rect_for_progress_bar(rect);
            let (cover, metadata) = split_rect_for_cover_img(above, picker);
            (metadata, cover, progress)
        }
        config::ProgressBarPosition::Right => {
            let (cover, rest) = split_rect_for_cover_img(rect, picker);
            let (metadata, progress) = split_rect_for_progress_bar(rest);
            (metadata, cover, progress)
        }
    }
}

/// Place the cover in the top-right of `combined` (metadata strip + visualizer).
///
/// Returns `(cover, metadata)` where metadata is only the left side of the top
/// `metadata_height` rows — the cover may hang down into the visualizer.
#[cfg(all(feature = "image", feature = "streaming"))]
fn split_cover_over_visualizer(
    combined: Rect,
    metadata_height: u16,
    picker: &ratatui_image::picker::Picker,
) -> (Rect, Rect) {
    let configs = config::get_config();
    let cover_rows = (configs.app_config.cover_img_width as u16).min(combined.height);
    // One column left of the chart's vertical axis (the axis sits at
    // `Y_AXIS_WIDTH`); also reserve gap + right margin for cover.
    let left_inset = super::streaming::Y_AXIS_WIDTH.saturating_sub(1);
    let max_cols = combined.width.saturating_sub(left_inset + 2);
    let cover_cols = cover_img_length_for_rows(configs, picker, cover_rows)
        .min(max_cols)
        .max(1);

    let cover = Rect {
        x: combined.right().saturating_sub(1 + cover_cols),
        y: combined.y,
        width: cover_cols,
        height: cover_rows,
    };

    let meta_x = combined.x + left_inset;
    let meta_right = cover.x.saturating_sub(1);
    let metadata = Rect {
        x: meta_x,
        y: combined.y,
        width: meta_right.saturating_sub(meta_x),
        height: metadata_height.min(combined.height),
    };

    (cover, metadata)
}

#[cfg(feature = "image")]
fn split_rect_for_cover_img(rect: Rect, picker: &ratatui_image::picker::Picker) -> (Rect, Rect) {
    let configs = config::get_config();
    // Use the height we can actually fill so column sizing matches the rendered box.
    let cover_rows = (configs.app_config.cover_img_width as u16).min(rect.height);
    let cover_cols = cover_img_length_for_rows(configs, picker, cover_rows);
    // Metadata on the left, cover top-right; 1-column margins on the outer edges
    // and between the two so spacing stays symmetric.
    let hor_chunks = Layout::horizontal([
        Constraint::Length(1),
        Constraint::Fill(0), // metadata_rect
        Constraint::Length(1),
        Constraint::Length(cover_cols),
        Constraint::Length(1),
    ])
    .split(rect);
    let ver_chunks = Layout::vertical([Constraint::Length(cover_rows)]).split(hor_chunks[3]);

    (ver_chunks[0], hor_chunks[1])
}

/// Determine the cover image box's width in columns for a given row height.
#[cfg(feature = "image")]
fn cover_img_length_for_rows(
    configs: &config::Configs,
    picker: &ratatui_image::picker::Picker,
    rows: u16,
) -> u16 {
    match configs.app_config.cover_img_length {
        // When `cover_img_length` is `0` (the default), derive it from the terminal's cell aspect ratio
        0 => {
            let font_size = picker.font_size();
            let width = font_size.width.max(1);
            (rows * font_size.height / width).max(1)
        }
        length => length as u16,
    }
}

/// Encode / cache the cover for `cover_img_rect` without drawing it yet.
#[cfg(feature = "image")]
fn prepare_cover_image(
    state: &SharedState,
    ui: &mut UIStateGuard,
    item: &rspotify::model::PlayableItem,
    cover_img_rect: Rect,
) {
    let url = match item {
        rspotify::model::PlayableItem::Track(track) => {
            crate::utils::get_track_album_image_url(track).map(String::from)
        }
        rspotify::model::PlayableItem::Episode(episode) => {
            crate::utils::get_episode_show_image_url(episode).map(String::from)
        }
        rspotify::model::PlayableItem::Unknown(_) => None,
    };
    let Some(url) = url else {
        // Avoid drawing the previous track's cover after the visualizer.
        ui.last_cover_image_render_info = ImageRenderInfo::default();
        return;
    };

    let data = state.data.read();
    let Some(img) = data.caches.images.get(&url) else {
        // Image not cached yet — clear so post-viz render does not keep the old cover.
        ui.last_cover_image_render_info = ImageRenderInfo::default();
        return;
    };

    if ui.last_cover_image_render_info.url != url
        || ui.last_cover_image_render_info.render_area != cover_img_rect
    {
        let cover_state =
            match crate::ui::cover_image::CoverImage::new(&ui.picker, img, cover_img_rect) {
                Ok(cover) => Some(cover),
                Err(err) => {
                    tracing::error!("Failed to encode cover image: {err:#}");
                    None
                }
            };
        ui.last_cover_image_render_info = ImageRenderInfo {
            url,
            render_area: cover_img_rect,
            state: cover_state,
        };
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
    let format_str = playback_header_format();
    let data = state.data.read();

    let mut playback_text = Text::default();
    let mut spans = vec![];

    // this regex is to handle a format argument or a newline
    let re = regex::Regex::new(r"\{.*?\}|\n").unwrap();

    let mut ptr = 0;
    for m in re.find_iter(&format_str) {
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

fn playback_status_field_texts(playback: &PlaybackMetadata) -> Vec<String> {
    let configs = config::get_config();
    let repeat_value = <&'static str>::from(playback.repeat_state);
    let volume_value = if let Some(volume) = playback.mute_state {
        format!("{volume}% (muted)")
    } else {
        format!("{}%", playback.volume.unwrap_or_default())
    };

    configs
        .app_config
        .playback_metadata_fields
        .iter()
        .filter_map(|field| match field.as_str() {
            "repeat" => Some(format!("repeat: {repeat_value}")),
            "shuffle" => Some(format!("shuffle: {}", playback.shuffle_state)),
            "volume" => Some(format!("volume: {volume_value}")),
            "device" => Some(format!("device: {}", playback.device_name)),
            _ => None,
        })
        .collect()
}

/// Repeat/shuffle/volume/device spread across the last inner row, with side
/// padding equal to half the gap between fields.
fn render_playback_status_row(
    frame: &mut Frame,
    ui: &UIStateGuard,
    playback: &PlaybackMetadata,
    rect: Rect,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }

    let fields = playback_status_field_texts(playback);
    if fields.is_empty() {
        return;
    }

    let style = ui.theme.playback_metadata();
    let text_width: usize = fields.iter().map(|s| s.chars().count()).sum();
    let leftover = (rect.width as usize).saturating_sub(text_width);
    let (left, gaps, right) = status_row_spacing(fields.len(), leftover);

    let mut spans = Vec::with_capacity(fields.len() * 2 + 2);
    if left > 0 {
        spans.push(Span::raw(" ".repeat(left)));
    }
    for (i, field) in fields.iter().enumerate() {
        spans.push(Span::styled(field.clone(), style));
        if i < gaps.len() {
            spans.push(Span::raw(" ".repeat(gaps[i])));
        }
    }
    if right > 0 {
        spans.push(Span::raw(" ".repeat(right)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
}

/// CSS-style `space-around`: leftover columns are split into `2n` half-gaps so
/// each side pad is half the space between neighboring fields.
fn status_row_spacing(field_count: usize, leftover: usize) -> (usize, Vec<usize>, usize) {
    match field_count {
        0 => (0, Vec::new(), leftover),
        1 => {
            let left = leftover / 2;
            (left, Vec::new(), leftover - left)
        }
        n => {
            let half = leftover / (2 * n);
            let extra = leftover % (2 * n);
            let mut gaps = vec![2 * half; n - 1];
            for i in 0..extra {
                gaps[i % (n - 1)] += 1;
            }
            (half, gaps, half)
        }
    }
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

    // When visualization is enabled, size the window to fit metadata, progress,
    // status, and the chart tightly. The cover overlaps the visualizer, so it
    // does not add height.
    #[cfg(feature = "streaming")]
    let playback_width = if configs.app_config.enable_audio_visualization
        && state.player.read().currently_playing().is_some()
    {
        playback_format_line_count() as usize + super::streaming::VIS_HEIGHT as usize + 2
    } else {
        configs.app_config.layout.playback_window_height
    };

    #[cfg(not(feature = "streaming"))]
    let playback_width = configs.app_config.layout.playback_window_height;

    // Without visualization, the playback window must be tall enough for the cover.
    #[cfg(all(feature = "image", feature = "streaming"))]
    let playback_width = if configs.app_config.enable_audio_visualization
        && state.player.read().currently_playing().is_some()
    {
        playback_width
    } else {
        std::cmp::max(configs.app_config.cover_img_width + 1, playback_width)
    };

    #[cfg(all(feature = "image", not(feature = "streaming")))]
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

#[cfg(test)]
mod tests {
    use super::{collapse_format_newlines, status_row_spacing};

    #[test]
    fn collapse_format_newlines_strips_trailing_blank_line() {
        let s =
            collapse_format_newlines("{status} {track} • {artists} {liked}\n{album} • {genres}\n");
        assert_eq!(
            s,
            "{status} {track} • {artists} {liked}\n{album} • {genres}"
        );
    }

    #[test]
    fn collapse_format_newlines_collapses_double_newlines() {
        let s = collapse_format_newlines("{track}\n\n{album}");
        assert_eq!(s, "{track}\n{album}");
    }

    #[test]
    fn status_row_spacing_sides_are_half_the_gap() {
        let (left, gaps, right) = status_row_spacing(4, 40);
        assert_eq!((left, right), (5, 5));
        assert_eq!(gaps, vec![10, 10, 10]);
    }

    #[test]
    fn status_row_spacing_uses_all_leftover() {
        let (left, gaps, right) = status_row_spacing(4, 41);
        assert_eq!(left + right + gaps.iter().sum::<usize>(), 41);
        assert_eq!((left, right), (5, 5));
        assert!(gaps.iter().all(|&g| g == 10 || g == 11));
        assert!(gaps.iter().any(|&g| g == 11));
    }

    #[test]
    fn status_row_spacing_centers_a_single_field() {
        let (left, gaps, right) = status_row_spacing(1, 9);
        assert!(gaps.is_empty());
        assert_eq!((left, right), (4, 5));
    }

    #[test]
    fn split_status_row_takes_the_last_inner_row() {
        let rect = super::Rect::new(0, 0, 80, 12);
        let (content, status) = super::split_status_row(rect);
        assert_eq!(status.y, 11);
        assert_eq!(status.height, 1);
        assert_eq!(content.height, 11);
        assert_eq!(content.bottom(), status.y);
    }

    #[cfg(feature = "streaming")]
    #[test]
    fn split_viz_playback_rows_progress_flush_with_bottom() {
        let rect = super::Rect::new(0, 0, 80, 12);
        let (header, vis, progress) = super::split_viz_playback_rows(rect, 2);
        assert_eq!(header.height, 2);
        assert_eq!(progress.bottom(), rect.bottom());
        assert_eq!(vis.bottom(), progress.y);
        assert_eq!(progress.height, 1);
    }

    #[cfg(feature = "streaming")]
    #[test]
    fn viz_inner_stack_is_header_vis_progress_status_flush() {
        let inner = super::Rect::new(1, 1, 80, 14);
        let (content, status) = super::split_status_row(inner);
        let (header, vis, progress) = super::split_viz_playback_rows(content, 2);
        assert_eq!(header.y, inner.y);
        assert_eq!(vis.y, header.bottom());
        assert_eq!(progress.y, vis.bottom());
        assert_eq!(status.y, progress.bottom());
        assert_eq!(status.bottom(), inner.bottom());
        assert_eq!(status.height, 1);
        assert_eq!(progress.height, 1);
        assert_eq!(header.height, 2);
    }
}
