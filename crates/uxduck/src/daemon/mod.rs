//! `uxduck daemon`: start the engine and the listeners, publish
//! `daemon.json`, and run until interrupted.

pub mod agent;
pub mod controllers;
pub mod data_plane;
pub mod engine_task;
#[cfg(all(feature = "gamepad", target_os = "macos"))]
pub mod hid;
#[cfg(feature = "plugins")]
pub mod plugins;
#[cfg(feature = "plugins")]
pub mod script;
mod transcripts;

use std::path::{Path, PathBuf};

use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_engine::runtime::{Clock, Ports};
use uxduck_engine::{Engine, EngineConfig};

use crate::adapters::credentials::FileCredentials;
use crate::adapters::system::{OsRandom, SystemClock};
use crate::api::listeners::{http, local};
use crate::paths::{self, DaemonInfo};
use crate::store;
use engine_task::EngineHandle;

/// The default loopback port.
pub const DEFAULT_PORT: u16 = 47800;

/// Where controller input comes from.
#[derive(Clone, Copy, Debug)]
pub struct ControllerInput {
    /// Read gamepads (off in tests, and in builds without the feature).
    pub gamepads: bool,
    /// A fake controller on this loopback UDP port (tests and development).
    pub fake: Option<u16>,
}

// Each bool is a command-line switch.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug)]
pub struct DaemonOptions {
    pub data_dir: PathBuf,
    /// Test-owned daemons stop when the runner's stdin pipe closes.
    pub exit_on_stdin_close: bool,
    /// 0 picks a free port (tests).
    pub port: u16,
    /// Development: the dev server's origin is allowed and is where
    /// `uxduck open` sends the browser.
    pub dev_origin: Option<String>,
    /// Other sites that may pair and connect with a token: UXDuck on the
    /// web, and in tests a page served elsewhere.
    pub hosted_origins: Vec<String>,
    /// A built `apps/web` to serve on the loopback origin.
    pub ui_dir: Option<PathBuf>,
    /// Open the UI, already paired, once listening.
    pub open: bool,
    /// Write a checkpoint after this many events.
    pub checkpoint_every: u64,
    /// Models offered for threads; the first is the default.
    pub models: Vec<String>,
    /// Heartbeat ages at which a vehicle turns stale and offline.
    pub presence: uxduck_mavlink::session::Config,
    pub agent: agent::ProviderChoice,
    /// What to do when the last window closes (ADR-0001 shutdown policy).
    pub on_last_client_closed: uxduck_engine::OnLastClientClosed,
    /// A headless job (`uxduck forward`): never quits on its own.
    pub headless: bool,
    /// Links to make sure exist at start, such as `uxduck forward`'s.
    pub links: Vec<uxduck_contracts::dto::link::LinkAddParams>,
    /// Check the release channel at start and daily (release builds only).
    pub update_check: bool,
    /// Where analytics go (ADR-0016 O16).
    pub analytics: crate::analytics::Config,
    /// `--no-analytics`: switch usage analytics off before anything is sent.
    pub no_analytics: bool,
    pub controllers: ControllerInput,
    /// The UDP port cameras send video to; `None` for off, 0 for any (tests).
    pub video_port: Option<u16>,
    /// Where looking for vehicles nearby goes (S44).
    pub find: crate::find::Places,
}

/// How long the daemon waits after the last window closes before it quits,
/// so a reload or a dev-server restart does not stop it.
const QUIT_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("cannot watch the parent pipe: {0}")]
    ParentPipe(std::io::Error),
    #[error(transparent)]
    Bind(#[from] local::BindError),
    #[error("cannot listen on 127.0.0.1:{port}: {source}")]
    Port { port: u16, source: std::io::Error },
    #[error("cannot use the data directory {path}: {source}")]
    DataDir {
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error("could not restart: {0}")]
    Restart(std::io::Error),
    #[error("cannot start UXDuck's menu bar: {0}")]
    Tray(std::io::Error),
    #[error("{0}")]
    Terrain(String),
}

/// Why the daemon stopped.
enum Stop {
    Quit,
    Restart,
}

/// Set in the environment of a daemon started by a restart: it waits a
/// moment for the old one's port and lock where they are not handed over.
const RESTARTED: &str = "UXDUCK_RESTARTED";

/// Runs the daemon until Ctrl-C or SIGTERM.
///
/// # Errors
/// When the data directory, socket or port cannot be used.
#[expect(
    clippy::too_many_lines,
    reason = "composition root orders startup and shutdown"
)]
pub async fn run(options: DaemonOptions) -> Result<(), DaemonError> {
    let parent_closed = crate::parent::stdin_closed(options.exit_on_stdin_close)
        .map_err(DaemonError::ParentPipe)?;
    let data_dir = &options.data_dir;
    // Before anything starts: a development daemon without the key fails here.
    let terrain = crate::terrain::Source::from_env(options.dev_origin.is_some())
        .map_err(DaemonError::Terrain)?;
    create_private_dir(data_dir).map_err(|source| DaemonError::DataDir {
        path: data_dir.display().to_string(),
        source,
    })?;

    let tcp = bind_port(options.port).await?;
    let port = tcp
        .local_addr()
        .map_or(options.port, |address| address.port());
    let ui_url = options.dev_origin.as_ref().map_or_else(
        || format!("http://127.0.0.1:{port}/"),
        |origin| format!("{}/", origin.trim_end_matches('/')),
    );

    // One daemon per data directory: the lock is held until the process ends,
    // and taken before the store is opened so two daemons never share it.
    let lock = std::fs::File::create(data_dir.join("daemon.lock")).map_err(|source| {
        DaemonError::DataDir {
            path: data_dir.display().to_string(),
            source,
        }
    })?;
    if !take_lock(&lock).await {
        return Err(DaemonError::Bind(local::BindError::AlreadyRunning(
            data_dir.display().to_string(),
        )));
    }
    initialize_private_state(data_dir)?;

    // A new data directory is a new install (ADR-0016 O6).
    let fresh = !data_dir.join("uxduck.db").exists();
    let (store, loaded, tray) = open_store_with_tray(&options).await?;
    let engine = start_engine(&options, &ui_url, port, &store, loaded);
    start_analytics(&engine, &options, fresh).await;
    // Recordings are the person's footage, in their videos folder: moved
    // there from the data directory before anything reads them.
    let recordings = crate::paths::move_recordings(data_dir);
    start_online(&engine, data_dir, terrain).await;

    let (socket, accept_local) = local::bind(data_dir, engine.clone()).await?;
    tokio::spawn(accept_local);
    // The tray connects to that socket (S64).
    crate::tray::start(data_dir, engine.clone(), tray);

    start_finding(&engine, &options).await;
    start_remote_view(&engine, data_dir).await;
    // Recordings are the Video storage category (ADR-0007).
    let video = crate::video::start(options.video_port, &engine, recordings).await;
    // Copies of vehicles' files (S121): the Vehicle files category.
    let vehicle_files = crate::vehicle_files::Store::new(data_dir);
    let port_files = vehicle_files.clone();
    let _ = engine.run(move |e| e.set_files(Box::new(port_files))).await;
    let extra_origins: Vec<String> = options.dev_origin.iter().cloned().collect();
    let interfaces = open_interfaces(&options, &engine)?;
    let app = http::router(
        engine.clone(),
        http::Allowlist::new(port, &extra_origins).with_hosted(&options.hosted_origins),
        interfaces,
        video,
        vehicle_files,
    );
    tokio::spawn(async move {
        if let Err(error) = axum::serve(tcp, app).await {
            tracing::error!(%error, code = "listener-stopped", "the loopback listener stopped");
        }
    });

    let info = DaemonInfo {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").into(),
        port,
        socket: socket.display().to_string(),
        ui_url: ui_url.clone(),
        dev: options.dev_origin.is_some(),
        started_us: SystemClock.now_us(),
    };
    paths::write_info(data_dir, &info).map_err(|source| DaemonError::DataDir {
        path: data_dir.display().to_string(),
        source,
    })?;
    tracing::info!(port, socket = %info.socket, data_dir = %data_dir.display(), "uxduck daemon {} listening", info.version);

    if (options.dev_origin.is_some() || options.open)
        && let Ok(token) = engine.create_bootstrap().await
    {
        let url = format!("{ui_url}?bootstrap={token}");
        eprintln!(
            "\n  UXDuck daemon {} is running.\n  Open the app, already paired: {url}\n",
            info.version
        );
        if options.open
            && let Err(error) = crate::adapters::browser::open(&url)
        {
            tracing::warn!(%error, "could not open a browser");
        }
    }

    if options.update_check
        && let Some(key) = crate::update::PUBLIC_KEY
    {
        tokio::spawn(check_for_updates(key, data_dir.clone()));
    }
    watch(&engine, options.headless);
    let stop = run_until_stopped(&engine, &options, parent_closed).await;
    tracing::info!("shutting down");
    // A final checkpoint, so the next start replays nothing (ADR-0007).
    let _ = engine.checkpoint().await;
    store.close();
    drop(lock);
    let _ = std::fs::remove_file(paths::info_path(data_dir));
    #[cfg(unix)]
    let _ = std::fs::remove_file(&socket);
    match stop {
        Stop::Quit => Ok(()),
        Stop::Restart => Err(DaemonError::Restart(restart_process())),
    }
}

fn initialize_private_state(data_dir: &Path) -> Result<(), DaemonError> {
    FileCredentials::new(data_dir)
        .migrate_account_token()
        .map_err(|error| DaemonError::DataDir {
            path: data_dir.join("credentials.json").display().to_string(),
            source: std::io::Error::other(error),
        })?;
    crate::diagnostics::configure(data_dir).map_err(|source| DaemonError::DataDir {
        path: data_dir.join("diagnostics").display().to_string(),
        source,
    })?;
    Ok(())
}

/// The program again, in this one's place. On macOS and Linux it replaces
/// this process (same pid, so a service manager or a terminal keeps it);
/// descriptors are close-on-exec, so ports and the lock are free for it.
/// On Windows it starts a new process and this one ends.
fn restart_process() -> std::io::Error {
    let exe = match std::env::current_exe() {
        // An update replaced the file on Linux: the new one has the old name.
        Ok(path) => std::path::PathBuf::from(
            path.to_string_lossy()
                .trim_end_matches(" (deleted)")
                .to_owned(),
        ),
        Err(error) => return error,
    };
    let mut command = std::process::Command::new(exe);
    command
        .args(std::env::args_os().skip(1))
        .env(RESTARTED, "1");
    tracing::info!("restarting");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.exec()
    }
    #[cfg(not(unix))]
    {
        match command.spawn() {
            Ok(_) => std::process::exit(0),
            Err(error) => error,
        }
    }
}

async fn open_store_with_tray(
    options: &DaemonOptions,
) -> Result<
    (
        store::Store,
        uxduck_engine::Loaded,
        Option<crate::tray::Tray>,
    ),
    DaemonError,
> {
    let (store, loaded) = store::open(&options.data_dir)?;
    let tray = match crate::tray::preflight(
        options.headless,
        loaded.state.settings.show_in_menu_bar,
    )
    .await
    {
        Ok(tray) => tray,
        Err(error) => {
            store.close();
            return Err(DaemonError::Tray(error));
        }
    };
    Ok((store, loaded, tray))
}

/// How long a restarted daemon waits for its port and lock.
const HANDOVER: std::time::Duration = std::time::Duration::from_secs(10);

async fn bind_port(port: u16) -> Result<tokio::net::TcpListener, DaemonError> {
    let deadline = std::time::Instant::now() + HANDOVER;
    loop {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(tcp) => return Ok(tcp),
            Err(_)
                if std::env::var_os(RESTARTED).is_some()
                    && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(source) => return Err(DaemonError::Port { port, source }),
        }
    }
}

async fn take_lock(lock: &std::fs::File) -> bool {
    let deadline = std::time::Instant::now() + HANDOVER;
    loop {
        if lock.try_lock().is_ok() {
            return true;
        }
        if std::env::var_os(RESTARTED).is_none() || std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The engine with its ports, in its task, and the agent host beside it.
fn start_engine(
    options: &DaemonOptions,
    ui_url: &str,
    port: u16,
    store: &store::Store,
    loaded: uxduck_engine::Loaded,
) -> EngineHandle {
    let data_dir = &options.data_dir;
    let credentials = FileCredentials::new(data_dir);
    let (agent_channel, agent_inbox) = agent::channel();
    let (link_channel, link_inbox) = data_plane::channel();
    // SITL builds for simulated vehicles (S17), probed off the engine's thread.
    let simulators = crate::sim::Simulators::new(data_dir);
    {
        let simulators = simulators.clone();
        std::thread::spawn(move || simulators.probe_all());
    }
    // Prebuilt builds to download (S18), fetched when first asked for.
    let catalogue = crate::sim::catalogue::Catalogue::new(
        simulators.clone(),
        tokio::runtime::Handle::current(),
    );
    #[cfg(feature = "plugins")]
    let (plugin_port, plugin_feed, plugin_host) = {
        let (port, frames, host) =
            plugins::channel(&data_dir.join("plugins"), data_dir.join("tlogs"));
        let port: Box<dyn uxduck_engine::runtime::PluginPort> = Box::new(port);
        (port, Some(frames.boxed()), Some(host))
    };
    #[cfg(not(feature = "plugins"))]
    let (plugin_port, plugin_feed): (Box<dyn uxduck_engine::runtime::PluginPort>, _) =
        (Box::new(uxduck_engine::runtime::NoPlugins), None);
    let engine = Engine::new(
        EngineConfig {
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            capabilities: DaemonCapabilities {
                simulator: true,
                ..DaemonCapabilities::default()
            },
            ui_url: ui_url.to_owned(),
            checkpoint_every: options.checkpoint_every,
            models: options.models.clone(),
            agent_provider: match options.agent {
                agent::ProviderChoice::OpenAi { .. } => "openai".into(),
                agent::ProviderChoice::Auto { .. } => "auto".into(),
                agent::ProviderChoice::Hosted => "hosted".into(),
                agent::ProviderChoice::Fake { .. } => "fake".into(),
            },
            about: uxduck_engine::HostAbout {
                commit: crate::build::COMMIT.map(str::to_owned),
                built_at: crate::build::BUILT_AT.map(str::to_owned),
                target: crate::build::TARGET.to_owned(),
                data_dir: Some(data_dir.display().to_string()),
                port: Some(port),
            },
        },
        Box::new(SystemClock),
        Box::new(OsRandom),
        Ports {
            persistence: Box::new(store.handle.clone()),
            agent: Box::new(agent_channel),
            credentials: Box::new(credentials.clone()),
            links: Box::new(link_channel),
            system: Box::new(crate::adapters::host::Host::new(
                &options.data_dir,
                simulators.clone(),
                catalogue,
                crate::desktop_app::DesktopApp::new(
                    &options.data_dir,
                    tokio::runtime::Handle::current(),
                ),
            )),
            plugins: plugin_port,
        },
        loaded,
    );
    let engine = EngineHandle::spawn(engine);
    #[cfg(feature = "plugins")]
    if let Some(host) = plugin_host {
        host.start(engine.clone());
    }
    let inputs = data_plane::inputs(&link_inbox);
    if let Some(port) = options.controllers.fake
        && let Err(error) = controllers::fake(port, inputs.clone())
    {
        tracing::warn!(%error, port, "the fake controller could not listen");
    }
    #[cfg(feature = "gamepad")]
    if options.controllers.gamepads {
        controllers::gamepads(inputs);
    }
    #[cfg(not(feature = "gamepad"))]
    let _ = (inputs, options.controllers.gamepads);
    data_plane::start(
        link_inbox,
        engine.clone(),
        options.presence,
        data_dir.join("tlogs"),
        plugin_feed,
        simulators,
    );
    agent::start(
        agent_inbox,
        engine.clone(),
        options.agent.clone(),
        credentials,
        &options.data_dir,
    );
    engine
}

fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The analytics outbox, told whether the switch is on; `--no-analytics`
/// switches it off first, so a new install with it sends nothing but its
/// update check.
/// What talks to UXDuck's API and R2: feedback, the account, cloud sync
/// and parameter definitions.
async fn start_online(
    engine: &EngineHandle,
    data_dir: &Path,
    terrain: Option<crate::terrain::Source>,
) {
    let feedback = crate::feedback::start(data_dir, FileCredentials::new(data_dir), engine.clone());
    let _ = engine
        .run(move |e| e.set_feedback(Box::new(feedback)))
        .await;
    // The account (S27): signed in by this daemon, never by a page.
    let account = crate::account::start(data_dir, FileCredentials::new(data_dir), engine.clone());
    let _ = engine.run(move |e| e.set_account(Box::new(account))).await;
    // Cloud sync (S16): idle until it is on and UXDuck is signed in.
    let sync = crate::sync::start(data_dir, FileCredentials::new(data_dir), engine.clone());
    let _ = engine.run(move |e| e.set_sync(Box::new(sync))).await;
    // This computer's connection to its account (S78): idle until signed in.
    crate::edge::start(data_dir, FileCredentials::new(data_dir), engine.clone());
    // Flights to the account (S82): the spool cuts, and uploads while signed in.
    crate::uploader::start(data_dir, FileCredentials::new(data_dir), engine.clone());
    // Parameter definitions (S67): the catalogue, fetched when a vehicle
    // first needs it.
    let definitions =
        crate::params::Host::new(data_dir, crate::params::Source::from_env(), engine.clone());
    let _ = engine
        .run(move |e| e.set_definitions(Box::new(definitions)))
        .await;
    // Terrain heights (S23): the manifest is read and fetched at once.
    if let Some(source) = terrain {
        let terrain = crate::terrain::Host::new(data_dir, source.base_url, engine.clone());
        let _ = engine
            .run(move |e| e.set_terrain(Box::new(terrain), source.key))
            .await;
    }
    // People's sets from a URL their CI publishes (S74): fetched again a
    // minute after start, then daily.
    let refresh = engine.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_mins(1)).await;
        loop {
            crate::wakes::count("parameter definitions: refresh");
            let _ = refresh.run(uxduck_engine::Engine::refresh_own_sets).await;
            tokio::time::sleep(std::time::Duration::from_hours(24)).await;
        }
    });
}

/// Finding vehicles (S44): probes while someone looks, and USB plugs.
async fn start_finding(engine: &EngineHandle, options: &DaemonOptions) {
    let finder = crate::find::Finder::new(engine.clone(), options.find.clone());
    let _ = engine.run(move |e| e.set_discovery(Box::new(finder))).await;
    crate::find::watch_usb(engine.clone());
}

/// Remote vehicles in the app (S86): viewers on the relay while a window
/// watches another of the account's computers.
async fn start_remote_view(engine: &EngineHandle, data_dir: &std::path::Path) {
    let port = crate::remote_view::start(data_dir.to_path_buf(), engine.clone());
    let _ = engine.run(move |e| e.set_remote_view(Box::new(port))).await;
}

async fn start_analytics(engine: &EngineHandle, options: &DaemonOptions, fresh: bool) {
    let channel = crate::analytics::start(&options.data_dir, options.analytics.clone());
    let off = options.no_analytics;
    let _ = engine
        .run(move |e| {
            if off {
                e.turn_analytics_off();
            }
            e.set_analytics(Box::new(channel));
            if fresh {
                e.track(crate::analytics::install_completed());
            }
        })
        .await;
}

/// Makes sure the command line's links exist, applies the shutdown policy,
/// and waits for Ctrl-C, SIGTERM, the last window to close, or a restart.
async fn run_until_stopped(
    engine: &EngineHandle,
    options: &DaemonOptions,
    parent_closed: impl std::future::Future<Output = ()>,
) -> Stop {
    for params in options.links.clone() {
        let kind = format!("{:?}", params.kind);
        match engine.run(move |e| e.ensure_link(params)).await {
            Ok(Ok(id)) => tracing::info!(link = %id, %kind, "link ready"),
            Ok(Err(error)) => {
                tracing::error!(%kind, error = %error.message, "link refused");
                eprintln!("uxduck: {kind}: {}", error.message);
            }
            Err(_) => {}
        }
    }
    let policy = options.on_last_client_closed;
    let headless = options.headless;
    let _ = engine.run(move |e| e.set_shutdown(policy, headless)).await;

    tokio::select! {
        () = parent_closed => Stop::Quit,
        () = shutdown_signal() => Stop::Quit,
        () = last_window_closed(engine.clone()) => {
            tracing::info!("the last window closed; quitting (on-last-client-closed = quit)");
            Stop::Quit
        }
        () = restart_requested(engine.clone()) => Stop::Restart,
        () = stop_requested(engine.clone()) => {
            tracing::info!("stopping, as a client asked");
            Stop::Quit
        }
        () = panic_requested(engine.clone()) => {
            // The panic hook records it; it is sent after the next start (S32).
            panic!("uxduck debug panic: a test of error reporting");
        }
    }
}

/// Resolves when a client has asked for a restart (and been answered).
async fn restart_requested(engine: EngineHandle) {
    engine.requested(|r| r.restart).await;
    // Let the answer leave before the connections close.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

/// Resolves when a client has asked a development build to panic (and been
/// answered).
async fn panic_requested(engine: EngineHandle) {
    engine.requested(|r| r.panic).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

/// Resolves when a client has asked the daemon to stop (and been answered).
async fn stop_requested(engine: EngineHandle) {
    engine.requested(|r| r.stop).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

/// Free space and the storage policy: shortly after start, then every
/// minute.
/// The interface (S109): downloaded and kept, or the copy built in.
fn open_interfaces(
    options: &DaemonOptions,
    engine: &EngineHandle,
) -> Result<std::sync::Arc<crate::interface::Interfaces>, DaemonError> {
    let interfaces = std::sync::Arc::new(
        crate::interface::Interfaces::open(
            &options.data_dir,
            options.ui_dir.clone(),
            crate::interface::Config::from_env(options.dev_origin.clone()),
        )
        .map_err(|source| DaemonError::DataDir {
            path: options.data_dir.join("interface").display().to_string(),
            source,
        })?,
    );
    interfaces.start(engine);
    Ok(interfaces)
}

/// The engine's chores: approvals that expire, storage, and on a computer
/// with a person at it, Open at login.
fn watch(engine: &EngineHandle, headless: bool) {
    tokio::spawn(expire_approvals(engine.clone()));
    tokio::spawn(check_storage(engine.clone()));
    if !headless {
        tokio::spawn(check_login_item(engine.clone()));
    }
}

/// What the system says of Open at login (S108): at start, then every
/// minute, so a switch turned off in System Settings or Task Manager shows
/// off in UXDuck too.
async fn check_login_item(engine: EngineHandle) {
    loop {
        let Ok(item) = tokio::task::spawn_blocking(crate::adapters::login_item::status).await
        else {
            break;
        };
        if engine
            .run(move |e| e.login_item_changed(item))
            .await
            .is_err()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        crate::wakes::count("open at login: check (1/min)");
    }
}

async fn check_storage(engine: EngineHandle) {
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    loop {
        if engine.run(Engine::check_storage).await.is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

/// Checks the release channel shortly after start, then just after each
/// UTC midnight, and logs what it finds (ADR-0008). Never downloads
/// anything.
async fn check_for_updates(key: &'static str, data_dir: PathBuf) {
    use uxduck_engine::runtime::Clock;
    let url =
        std::env::var("UXDUCK_UPDATE_URL").unwrap_or_else(|_| crate::update::DEFAULT_URL.into());
    // Spread over the first hour of the day, the same for this run.
    let spread = {
        let mut byte = [0u8; 2];
        let _ = getrandom::fill(&mut byte);
        std::time::Duration::from_secs(u64::from(u16::from_le_bytes(byte)) % 3600)
    };
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    loop {
        match crate::update::check(&url, Some(key), &data_dir).await {
            Ok(report) => tracing::info!("{report}"),
            Err(error) => tracing::debug!(%error, "update check"),
        }
        let wait = crate::update::until_next_day(SystemClock.now_us(), spread);
        tokio::time::sleep(wait).await;
    }
}

/// Applies the safe default to approvals nobody answered in time.
async fn expire_approvals(engine: EngineHandle) {
    use uxduck_engine::runtime::Clock;
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        tick.tick().await;
        crate::wakes::count("approvals: expiry (1 Hz)");
        let now = SystemClock.now_us();
        // And who supervises each vehicle (S81).
        if engine
            .run(move |e| {
                e.expire_approvals(now);
                e.expire_pairings();
                e.supervise(now);
                // Downloads waiting for a vehicle that is back (S121).
                e.resume_waiting_files(now);
            })
            .await
            .is_err()
        {
            break;
        }
    }
}

/// Resolves once the engine has wanted to quit for the whole grace period.
async fn last_window_closed(engine: EngineHandle) {
    let step = std::time::Duration::from_secs(1);
    let mut quiet = std::time::Duration::ZERO;
    loop {
        tokio::time::sleep(step).await;
        crate::wakes::count("last window closed: check (1 Hz)");
        if engine.run(|e| e.should_quit()).await.unwrap_or(false) {
            quiet += step;
            if quiet >= QUIT_GRACE {
                return;
            }
        } else {
            quiet = std::time::Duration::ZERO;
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = async { match terminate.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending().await } } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
