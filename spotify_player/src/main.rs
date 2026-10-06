mod auth;
mod cli;
mod client;
mod command;
mod config;
#[cfg(target_os = "linux")]
mod desktop_spotify;
mod event;
mod key;
mod log_layer;
#[cfg(feature = "media-control")]
mod media_control;
mod playlist_folders;
mod state;
#[cfg(feature = "streaming")]
mod streaming;
#[cfg(all(feature = "system-audio-visualization", target_os = "linux"))]
mod system_audio;
mod token;
mod ui;
mod utils;
#[cfg(feature = "streaming")]
mod vis;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    io::{IsTerminal as _, Write},
    sync::Arc,
};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::apply_config_override;

/// File name prefix shared by one start's log and backtrace files. Prefixes
/// sort by start time; starts within the same second differ by process id.
fn log_file_prefix(start: chrono::NaiveDateTime, pid: u32) -> String {
    let time = start.format("%y-%m-%d-%H-%M-%S");
    format!("spotify-player-{time}-{pid}")
}

/// Create `path`, failing if it already exists, so a start never overwrites
/// an earlier start's log or backtrace file.
fn create_new_file(path: &std::path::Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))
}

fn init_logging(
    log_folder: &std::path::Path,
    log_buffer: Arc<Mutex<VecDeque<String>>>,
) -> Result<()> {
    if std::env::var_os("RUST_LOG").is_some_and(|x| x == "off") {
        // Don't create log files if logging is disabled.
        return Ok(());
    }

    let log_prefix = log_file_prefix(chrono::Local::now().naive_local(), std::process::id());

    // initialize the application's logging
    if std::env::var("RUST_LOG").is_err() {
        // default to log the current crate and librespot crates
        std::env::set_var("RUST_LOG", "spotify_player=info,librespot=info");
    }
    if !log_folder.exists() {
        std::fs::create_dir_all(log_folder)?;
    }
    let log_file = create_new_file(&log_folder.join(format!("{log_prefix}.log")))?;

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(log_file));

    let buffer_layer = crate::log_layer::BufferLayer::new(log_buffer, 1000);

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(fmt_layer)
        .with(buffer_layer)
        .init();

    // initialize the application's panic backtrace
    let backtrace_file = create_new_file(&log_folder.join(format!("{log_prefix}.backtrace")))?;
    let backtrace_file = std::sync::Mutex::new(backtrace_file);
    std::panic::set_hook(Box::new(move |info| {
        // Also surface panics in the log file and the in-TUI Logs page;
        // the backtrace file alone is easy to miss.
        tracing::error!("Panic: {info}");
        let mut file = backtrace_file.lock().unwrap();
        let backtrace = backtrace::Backtrace::new();
        writeln!(&mut file, "Got a panic: {info:#?}\n").unwrap();
        writeln!(&mut file, "Stack backtrace:\n{backtrace:?}").unwrap();
    }));

    Ok(())
}

#[tokio::main]
async fn start_app(state: &state::SharedState) -> Result<()> {
    // client channels
    let (client_pub, client_sub) = flume::unbounded::<client::ClientRequest>();

    #[cfg(feature = "pulseaudio-backend")]
    {
        // set environment variables for PulseAudio
        if std::env::var("PULSE_PROP_application.name").is_err() {
            std::env::set_var("PULSE_PROP_application.name", "spotify-player");
        }
        if std::env::var("PULSE_PROP_application.icon_name").is_err() {
            std::env::set_var("PULSE_PROP_application.icon_name", "spotify");
        }
        if std::env::var("PULSE_PROP_stream.description").is_err() {
            let configs = config::get_config();
            std::env::set_var(
                "PULSE_PROP_stream.description",
                format!(
                    "Spotify Connect endpoint ({})",
                    configs.app_config.device.name
                ),
            );
        }
        if std::env::var("PULSE_PROP_media.software").is_err() {
            std::env::set_var("PULSE_PROP_media.software", "Spotify");
        }
        if std::env::var("PULSE_PROP_media.role").is_err() {
            std::env::set_var("PULSE_PROP_media.role", "music");
        }
    }

    // Start local-only helpers before authentication/session setup. Both Spotify
    // desktop startup and Pulse capture can warm up while network work proceeds.
    #[cfg(target_os = "linux")]
    {
        let desktop = config::get_config().app_config.desktop_spotify.clone();
        if let Err(err) = desktop_spotify::launch_early_if_needed(&desktop) {
            tracing::warn!("Failed to start Spotify desktop early: {err:#}");
        }
    }

    #[cfg(all(feature = "system-audio-visualization", target_os = "linux"))]
    system_audio::start(state);

    // A missing credential needs a browser login. Interactively the TUI starts
    // first, so the prompt is a popup instead of text written over the screen.
    let tui_login = login_in_tui(state);
    if tui_login {
        start_tui(state, &client_pub)?;
    }
    let login_prompt = if tui_login {
        auth::LoginPrompt::Tui(state.clone())
    } else {
        auth::LoginPrompt::Stdout
    };

    // create a Spotify API client
    let client = match connect(state, login_prompt).await {
        Ok(client) => client,
        Err(err) => return fail_startup(state, tui_login, err).await,
    };

    // request user data
    client_pub.send(client::ClientRequest::GetCurrentUser)?;
    client_pub.send(client::ClientRequest::GetUserPlaylists)?;
    client_pub.send(client::ClientRequest::GetUserFollowedArtists)?;
    client_pub.send(client::ClientRequest::GetUserSavedAlbums)?;
    client_pub.send(client::ClientRequest::GetContext(state::ContextId::Tracks(
        state::USER_LIKED_TRACKS_ID.to_owned(),
    )))?;
    client_pub.send(client::ClientRequest::GetUserSavedShows)?;

    // client socket task (for handling CLI commands)
    tokio::task::spawn({
        let client = client.clone();
        let state = state.clone();
        async move {
            cli::start_socket(&client, Some(&state), None).await;
        }
    });

    // client event handler task
    tokio::task::spawn({
        let state = state.clone();
        let client = client.clone();
        async move {
            client::start_client_handler(&state, &client, &client_sub).await;
        }
    });

    // background task that detects an invalidated session and reconnects,
    // independent of any incoming client request
    tokio::task::spawn({
        let state = state.clone();
        let client = client.clone();
        async move {
            client::start_session_watcher(state, client).await;
        }
    });

    // player event watcher task
    std::thread::Builder::new()
        .name("player-event-watcher".to_string())
        .spawn({
            let state = state.clone();
            let client_pub = client_pub.clone();
            move || {
                run_supervised("player-event-watcher", || {
                    client::start_player_event_watcher(&state, &client_pub);
                });
            }
        })?;

    if !state.is_daemon && !tui_login {
        start_tui(state, &client_pub)?;
    }

    #[cfg(feature = "media-control")]
    if config::get_config().app_config.enable_media_control {
        // media control task
        std::thread::Builder::new()
            .name("media-control".to_string())
            .spawn({
                let state = state.clone();
                move || {
                    if let Err(err) = media_control::start_event_watcher(&state, client_pub) {
                        tracing::error!(
                            "Failed to start the application's media control event watcher: err={err:#?}"
                        );
                    }
                }
            })?;

        // the winit's event loop must be run in the main thread
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            // Start an event loop that listens to OS window events.
            //
            // MacOS and Windows require an open window to be able to listen to media
            // control events. The below code will create an invisible window on startup
            // to listen to such events.
            let event_loop = winit::event_loop::EventLoop::new()?;
            #[allow(deprecated)]
            event_loop.run(move |_, _| {})?;
        }
    }

    // Keep the runtime alive; the tasks and threads spawned above do the work.
    std::future::pending::<()>().await;
    Ok(())
}

/// Whether a missing credential's browser login is shown as a popup in the TUI.
///
/// That needs an interactive terminal and a loopback redirect URI: without the
/// callback server the login reads the redirect URL from stdin, which the TUI
/// owns. The daemon and a non-terminal stdout keep the printed prompt.
fn login_in_tui(state: &state::SharedState) -> bool {
    !state.is_daemon
        && std::io::stdout().is_terminal()
        && auth::redirect_uses_callback_server(&config::get_config().app_config.login_redirect_uri)
}

/// Take over the terminal and start the event-handler and UI threads.
fn start_tui(
    state: &state::SharedState,
    client_pub: &flume::Sender<client::ClientRequest>,
) -> Result<()> {
    #[cfg(feature = "image")]
    ui::init_image_picker(state).context("initialize image picker")?;
    let terminal = ui::init_terminal().context("initialize terminal")?;

    // terminal event handler task
    std::thread::Builder::new()
        .name("terminal-event-handler".to_string())
        .spawn({
            let client_pub = client_pub.clone();
            let state = state.clone();
            move || {
                run_supervised("terminal-event-handler", || {
                    event::start_event_handler(&state, &client_pub);
                });
            }
        })?;

    // application UI task
    std::thread::Builder::new().name("ui".to_string()).spawn({
        let state = state.clone();
        move || ui::run(&state, terminal)
    })?;
    Ok(())
}

/// Create the Spotify client and its session, logging in where a credential is missing.
async fn connect(
    state: &state::SharedState,
    login_prompt: auth::LoginPrompt,
) -> Result<client::AppClient> {
    let client = client::AppClient::new(login_prompt)
        .await
        .context("construct app client")?;
    client
        .new_session(Some(state), true)
        .await
        .context("initialize new Spotify session")?;
    Ok(client)
}

/// Report a startup failure. With the TUI up, the UI thread owns the terminal
/// and has to restore it before the error is printed, so the message is handed
/// over and this task waits for the process to exit. Otherwise the error
/// propagates to `main` as before.
async fn fail_startup(
    state: &state::SharedState,
    tui_started: bool,
    err: anyhow::Error,
) -> Result<()> {
    if !tui_started {
        return Err(err);
    }
    tracing::error!("Failed to start the application: {err:#}");
    state.ui.lock().quit_with_message(format!("Error: {err:#}"));
    std::future::pending::<()>().await;
    Ok(())
}

/// Run a long-lived thread body, restarting it after a panic.
///
/// `parking_lot` locks do not poison, so the shared state stays usable; a panic
/// on one tick must not silently disable playback refreshes or keyboard input.
fn run_supervised(name: &str, mut body: impl FnMut()) {
    loop {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut body)).is_ok() {
            return;
        }
        tracing::error!("Thread `{name}` panicked; restarting it in 1s");
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn main() -> Result<()> {
    // librespot depends on hyper-rustls which requires a crypto provider to be set up.
    // TODO: see if this can be fixed upstream
    rustls::crypto::ring::default_provider()
        .install_default()
        .unwrap();

    // parse command line arguments
    let args = cli::init_cli()?.get_matches();

    // initialize the application's cache and config folders
    let config_folder: std::path::PathBuf = args
        .get_one::<String>("config-folder")
        .expect("config-folder should have default value")
        .into();
    if !config_folder.exists() {
        std::fs::create_dir_all(&config_folder)?;
    }

    let cache_folder: std::path::PathBuf = args
        .get_one::<String>("cache-folder")
        .expect("cache-folder should have a default value")
        .into();
    // The cache folder holds the Web API tokens and librespot credentials, so it
    // is created private. A folder that already exists is the user's to manage.
    utils::create_private_dir_all(&cache_folder)
        .with_context(|| format!("create cache folder {}", cache_folder.display()))?;
    let cache_audio_folder = cache_folder.join("audio");
    if !cache_audio_folder.exists() {
        std::fs::create_dir_all(&cache_audio_folder)?;
    }
    let cache_image_folder = cache_folder.join("image");
    if !cache_image_folder.exists() {
        std::fs::create_dir_all(&cache_image_folder)?;
    }

    // initialize the application configs
    {
        let mut configs = config::Configs::new(&config_folder, &cache_folder)?;
        if configs.app_config.log_folder.is_none() {
            // set the log folder to be the cache folder if it is not set
            configs.app_config.log_folder = Some(cache_folder);
        }
        if let Some(overrides) = args.get_many::<String>("config-override") {
            for override_str in overrides {
                let (key, value) = override_str.split_once('=').context(format!(
                    "Invalid override format: '{override_str}'. Expected KEY=VALUE"
                ))?;

                apply_config_override(&mut configs.app_config, key, value)?;
            }
        }
        config::set_config(configs);
    }

    match args.subcommand() {
        None => {
            // initialize the application's log
            let log_folder = config::get_config()
                .app_config
                .log_folder
                .as_deref()
                .expect("log_folder is set");

            let log_buffer: Arc<Mutex<VecDeque<String>>> =
                Arc::new(Mutex::new(VecDeque::with_capacity(1000)));

            init_logging(log_folder, log_buffer.clone())
                .context("failed to initialize application's logging")?;

            // Older versions left credential files with default permissions. Run
            // once logging is up so a failure is recorded.
            auth::restrict_cached_credentials(&config::get_config().cache_folder);

            // log the application's configurations
            tracing::info!("Configurations: {:?}", config::get_config());

            let is_daemon;

            #[cfg(feature = "daemon")]
            {
                is_daemon = args.get_flag("daemon");
                if is_daemon {
                    if cfg!(any(target_os = "macos", target_os = "windows"))
                        && cfg!(feature = "media-control")
                    {
                        eprintln!("Running the application as a daemon on windows/macos with `media-control` feature enabled is not supported!");
                        std::process::exit(1);
                    }

                    tracing::info!("Starting the application as a daemon...");
                    let daemonize = daemonize::Daemonize::new();
                    daemonize.start()?;
                }
            }

            #[cfg(not(feature = "daemon"))]
            {
                is_daemon = false;
            }

            let state = std::sync::Arc::new(state::State::new(is_daemon, log_buffer));
            start_app(&state)
        }
        Some((cmd, args)) => cli::handle_cli_subcommand(cmd, args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: u32, min: u32, sec: u32, milli: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 5)
            .unwrap()
            .and_hms_milli_opt(hour, min, sec, milli)
            .unwrap()
    }

    #[test]
    fn starts_in_the_same_minute_get_different_log_file_names() {
        let first = log_file_prefix(at(14, 46, 7, 0), 4242);
        assert_eq!(first, "spotify-player-26-10-05-14-46-07-4242");

        // Quit, then respawned by a wrapper half a second later.
        let respawned = log_file_prefix(at(14, 46, 7, 500), 4250);
        assert_ne!(first, respawned);

        // A later second differs even if the process id were reused.
        let later = log_file_prefix(at(14, 46, 52, 0), 4242);
        assert_ne!(first, later);
    }

    #[test]
    fn log_file_names_sort_by_start_time() {
        // The process id comes after the time, so a longer one does not
        // move an earlier start behind a later one.
        assert!(
            log_file_prefix(at(14, 46, 59, 0), 123_456) < log_file_prefix(at(14, 47, 0, 0), 99)
        );
        assert!(log_file_prefix(at(9, 59, 59, 0), 1) < log_file_prefix(at(10, 0, 0, 0), 1));
    }

    #[test]
    fn create_new_file_never_overwrites_an_existing_file() {
        let folder = crate::utils::test_scratch_dir("log-files");
        let path = folder.join("spotify-player-26-10-05-14-46-07-4242.log");
        std::fs::write(&path, "earlier run").unwrap();

        let err = create_new_file(&path).unwrap_err();
        assert_eq!(
            err.downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "earlier run");

        let fresh = folder.join("spotify-player-26-10-05-14-46-07-4250.log");
        create_new_file(&fresh).unwrap();
        assert!(fresh.exists());

        std::fs::remove_dir_all(&folder).unwrap();
    }
}
