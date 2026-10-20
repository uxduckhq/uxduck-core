//! The command line (ADR-0001, Deployable shape).

pub mod client;
pub mod find;
pub mod link_spec;
pub mod mission;
mod new_plugin;
pub mod params;
pub mod plan;
pub mod practice;
mod practice_replay;
pub mod terrain;
pub mod vehicle;
pub mod video;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::Value;

use crate::daemon::{
    self, DEFAULT_PORT, DEV_FIND_UDP_PORTS, DEV_ORIGIN, DEV_PORT, DEV_VIDEO_PORT, DaemonOptions,
};
use client::{CliError, LocalClient};

/// Ground control daemon and command-line interface for MAVLink vehicles.
#[derive(Debug, Parser)]
#[command(name = "uxduck", version, long_version = crate::build::long_version(), about)]
pub struct Cli {
    /// Use the development data directory (and, for `daemon`, allow the dev server).
    #[arg(long, global = true)]
    pub dev: bool,
    /// Data directory; defaults to `UXDUCK_DATA_DIR`, then the platform's.
    #[arg(long, global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// Stop when the owning process closes its stdin pipe (test helpers).
    #[arg(long, global = true, hide = true, env = "UXDUCK_EXIT_ON_STDIN_CLOSE")]
    exit_on_stdin_close: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon in the foreground.
    Daemon(DaemonArgs),
    /// Forward headless: a daemon with these links and no need for a window,
    /// such as on a Raspberry Pi beside a radio. It never quits on its own.
    /// `uxduck forward --in serial:/dev/ttyAMA0@57600 --out udp:192.168.1.20:15010`
    Forward(ForwardArgs),
    /// A Model Context Protocol server on stdio, for coding agents: add
    /// `uxduck mcp` to Claude Code or Codex. Reading vehicles needs nothing;
    /// commands wait until an operator allows them. Start UXDuck yourself
    /// before connecting; MCP never starts it automatically.
    Mcp {
        /// Advertise every tool schema up front instead of finding tools on demand.
        #[arg(long)]
        all_tools: bool,
    },
    /// Plugins: install one from a directory, list them, or test one
    /// against a recorded tlog without a daemon.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Start UXDuck at login, or stop it doing so: `uxduck service install`
    /// registers it as a user service (a launchd agent, a Task Scheduler
    /// task, the systemd user unit), as Settings does; `uninstall` removes it.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Approvals waiting for an operator: list them, or answer one.
    Approvals {
        #[command(subcommand)]
        command: Option<ApprovalsCommand>,
    },
    /// Updates: `uxduck update check` asks the release channel for a newer
    /// version. Nothing is downloaded or installed.
    Update {
        #[command(subcommand)]
        command: UpdateCommand,
    },
    /// The ports the daemon uses, and who holds them.
    Ports {
        #[command(subcommand)]
        command: Option<PortsCommand>,
    },
    /// Tools for development and tests.
    #[command(hide = true)]
    Dev {
        #[command(subcommand)]
        command: DevCommand,
    },
    /// Sign in to your UXDuck account with a code sent to your email.
    /// Works with no browser, as on a Raspberry Pi.
    Login {
        /// Skip the prompt.
        #[arg(long)]
        email: Option<String>,
        /// Join the product updates list.
        #[arg(long)]
        product_updates: bool,
    },
    /// Sign out; everything on this computer keeps working.
    Logout,
    /// Your UXDuck account.
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Simulated vehicles: start and stop them, and list the simulators
    /// UXDuck can run.
    Sim {
        #[command(subcommand)]
        command: SimCommand,
    },
    /// A vehicle's history: `uxduck vehicle history` and `uxduck vehicle
    /// note "replaced motor 3" --when yesterday`.
    Vehicle {
        #[command(subcommand)]
        command: vehicle::VehicleCommand,
    },
    /// Practice exercises on a simulated vehicle (S100): `uxduck practice
    /// list`, `start <exercise> --vehicle <name>`, `status`, `stop` and `guide`.
    Practice {
        #[command(subcommand)]
        command: practice::PracticeCommand,
    },
    /// A vehicle's missions over time (S134): `uxduck mission history Alpha`.
    Mission {
        #[command(subcommand)]
        command: mission::MissionCommand,
    },
    /// Saved plans (S125): `uxduck plan import mission.plan`.
    Plan {
        #[command(subcommand)]
        command: plan::PlanCommand,
    },
    /// Terrain heights (S23): `uxduck terrain at --lat 53.07 --lon -4.08`.
    Terrain {
        #[command(subcommand)]
        command: terrain::TerrainCommand,
    },
    /// Looks for vehicles plugged in or on this computer, and says why
    /// anything found is not one.
    Find {
        /// Search this computer's network too.
        #[arg(long)]
        network: bool,
        /// Look at serial ports the known-id table does not know too.
        #[arg(long)]
        all_serial: bool,
        /// The answer as JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Camera video: `uxduck video list`, and `uxduck video probe --port
    /// 5600` to see what is arriving and why it does or does not play.
    Video {
        #[command(subcommand)]
        command: video::VideoCommand,
    },
    /// Parameters: `uxduck params definitions` lists, adds, generates or
    /// removes the sets that say what a fork's parameters mean.
    Params {
        #[command(subcommand)]
        command: params::ParamsCommand,
    },
    /// Daemon settings and secrets.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Send one command to a vehicle and print its receipt:
    /// `uxduck command arm`. With one vehicle connected, `--vehicle` can be
    /// left out.
    #[command(name = "command")]
    Send {
        #[command(subcommand)]
        action: VehicleAction,
        /// The vehicle's id, from `uxduck rpc call vehicle/list`.
        #[arg(long, global = true)]
        vehicle: Option<String>,
        /// Send it while the vehicle is not online.
        #[arg(long, global = true)]
        confirm: bool,
        /// Don't ask first. In a terminal, a command the app would confirm
        /// (such as arm, or disarm in flight) asks; a script is not asked,
        /// but a dangerous command needs this.
        #[arg(long, short = 'y', global = true)]
        yes: bool,
    },
    /// Call one API method and print the result as JSON.
    Rpc {
        #[command(subcommand)]
        command: RpcCommand,
    },
    /// Print every frame in and out of the daemon as NDJSON, until interrupted.
    Trace,
    /// Development builds only: tests of error reporting.
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
    /// Which of the daemon's tasks wake, and how often: counted over a few
    /// seconds, most first. An idle daemon should show almost nothing.
    Wakeups {
        /// How long to count for.
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
    /// Where the data directory is, and whether a daemon is running there.
    Status {
        /// As JSON, for the desktop shell and scripts.
        #[arg(long)]
        json: bool,
    },
    /// Open UXDuck using its saved launch preference. Starts the background
    /// daemon first when needed, as opening UXDuck.app does.
    Open(OpenArgs),
}

#[derive(Debug, clap::Args)]
pub struct OpenArgs {
    /// Print the address instead of opening it.
    #[arg(long)]
    print: bool,
    /// Open in a browser without changing the saved launch preference.
    #[arg(long, conflicts_with_all = ["window", "print"])]
    browser: bool,
    /// Open a desktop window without changing the saved launch preference.
    #[arg(long, conflicts_with = "print")]
    window: bool,
}

impl OpenArgs {
    fn target(&self) -> Option<&'static str> {
        match (self.browser, self.window) {
            (true, _) => Some("browser"),
            (_, true) => Some("window"),
            _ => None,
        }
    }
}

// Each bool is a command-line switch.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, clap::Args)]
pub struct DaemonArgs {
    /// Loopback port: 45000 normally, 45100 with --dev; 0 picks a free one.
    #[arg(long, env = "UXDUCK_PORT")]
    port: Option<u16>,
    /// The dev server's origin, allowed and used as the UI address with `--dev`.
    #[arg(long, default_value = DEV_ORIGIN)]
    dev_origin: String,
    /// Another site that may pair and connect with a token, as UXDuck on the
    /// web does; for tests. Repeatable.
    #[arg(long, value_name = "ORIGIN", hide = true)]
    hosted_origin: Vec<String>,
    /// Serve a built app from this directory (apps/web/dist).
    #[arg(long, value_name = "DIR")]
    ui_dir: Option<PathBuf>,
    /// Open the app, already paired, once the daemon is listening.
    #[arg(long)]
    open: bool,
    /// Write a checkpoint after this many events.
    #[arg(long, default_value_t = 500, hide = true)]
    checkpoint_every: u64,
    /// The agent's model provider: `uxduck`, the hosted model for a
    /// signed-in UXDuck; in development builds also `openai`, a
    /// developer's own OpenAI or Anthropic key, `auto`, the own key when one is set and the
    /// hosted model otherwise, and `fake`, to test without either.
    #[arg(long, env = "UXDUCK_AGENT_PROVIDER", default_value = DEFAULT_PROVIDER)]
    agent_provider: String,
    /// Models offered in the selector, comma-separated; the first is the default.
    #[arg(
        long,
        env = "UXDUCK_AGENT_MODELS",
        value_delimiter = ',',
        default_value = "gpt-6-sol,gpt-6-luna,gpt-6-astra,claude-opus-5-5,claude-sonnet-5-5"
    )]
    agent_models: Vec<String>,
    /// Another OpenAI-compatible endpoint, such as a proxy.
    #[arg(long, env = "OPENAI_BASE_URL", hide = true)]
    openai_base_url: Option<String>,
    /// Another Anthropic-compatible endpoint, such as a proxy.
    #[arg(long, env = "ANTHROPIC_BASE_URL", hide = true)]
    anthropic_base_url: Option<String>,
    /// Heartbeat age, in milliseconds, after which a vehicle is stale.
    #[arg(long, default_value_t = 3_000, hide = true)]
    presence_stale_ms: u64,
    /// Heartbeat age, in milliseconds, after which a vehicle is offline.
    #[arg(long, default_value_t = 15_000, hide = true)]
    presence_offline_ms: u64,
    /// Do not check the release channel for updates (once at start, then daily).
    #[arg(long)]
    no_update_check: bool,
    /// Switch usage analytics off before anything is sent, and keep them off
    /// (Settings > Data). New installs are still counted by the update
    /// check, with no identifier.
    #[arg(long, env = "UXDUCK_NO_ANALYTICS")]
    no_analytics: bool,
    /// How long analytics events gather before they are sent, in
    /// milliseconds.
    #[arg(long, default_value_t = 60_000, hide = true)]
    analytics_gather_ms: u64,
    /// What to do when the last window closes: keep running, or quit after
    /// a short grace period unless a keep-alive link or `uxduck forward`
    /// needs the daemon.
    #[arg(long, value_enum, default_value_t = ShutdownChoice::KeepRunning)]
    on_last_client_closed: ShutdownChoice,
    /// Do not read gamepads.
    #[arg(long)]
    no_gamepads: bool,
    /// The UDP port cameras send video to (MPEG-TS, or RTP carrying it), or
    /// `off`. Defaults to 5600 normally, 45110 with --dev.
    #[arg(long, value_name = "PORT", value_parser = video_port, env = "UXDUCK_VIDEO_PORT")]
    video_port: Option<VideoPort>,
    /// Where finding vehicles listens for UDP (tests).
    #[arg(long = "find-udp-port", value_delimiter = ',', hide = true)]
    find_udp_ports: Option<Vec<u16>>,
    /// Which TCP ports on this computer finding vehicles tries (tests).
    #[arg(long, value_delimiter = ',', default_values_t = [5760u16, 5761, 5762, 5763], hide = true)]
    find_tcp_ports: Vec<u16>,
    /// Finding vehicles does not try the gateway (tests).
    #[arg(long, hide = true)]
    find_no_gateway: bool,
    /// A fake controller, driven by JSON datagrams to this loopback UDP port.
    #[arg(long, value_name = "PORT", hide = true)]
    fake_controller: Option<u16>,
    /// Burn a core in a busy loop, so `cargo xtask perf` can be seen to
    /// fail (S24). For tests only.
    #[arg(long, hide = true)]
    spin_for_tests: bool,
    #[command(subcommand)]
    action: Option<DaemonAction>,
}

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// Start a plugin: a folder with `plugin.json`, the script, and a test
    /// (`test.tlog` and `test.json`) that `uxduck plugin test` runs.
    New {
        id: String,
        /// Write it in TypeScript: one file, no build step.
        #[arg(long)]
        typescript: bool,
        /// Where to write it; a folder named after the id here if left out.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Install from a directory holding `plugin.json` and its module. The
    /// plugin gets the capabilities its manifest asks for.
    Install {
        path: PathBuf,
    },
    List,
    /// Run a plugin against a tlog as the daemon would: the telemetry items,
    /// decoded messages and frames it subscribes to, at its cadences on the
    /// tlog's clock, then `--command` (if given). Prints each call's input
    /// and effects as JSON lines. No daemon is needed.
    Test {
        path: PathBuf,
        /// The recorded flight; `test.tlog` in the plugin's folder if left out.
        #[arg(long)]
        tlog: Option<PathBuf>,
        /// A command to call after the tlog, as a panel button would.
        #[arg(long)]
        command: Option<String>,
        /// The command's parameters, as JSON.
        #[arg(long)]
        params: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonAction {
    /// Restart the running daemon: a final checkpoint, then the same
    /// program in its place. Clients reconnect on their own.
    Restart {
        /// Restart even while a vehicle is armed.
        #[arg(long)]
        yes: bool,
    },
    /// Start UXDuck in the background when it isn't running, and open
    /// nothing: what Open at login runs (S108).
    Start,
    /// Stop the running daemon, with a final checkpoint, and wait until it
    /// has gone. Nothing happens when none is running.
    Stop {
        /// The installer is about to replace UXDuck (Windows cannot while
        /// it runs): the next `uxduck open` waits for the pages that were
        /// open to come back instead of opening another.
        #[arg(long, hide = true)]
        for_update: bool,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum ShutdownChoice {
    Quit,
    KeepRunning,
}

#[derive(Debug, clap::Args)]
pub struct ForwardArgs {
    /// Where vehicles are heard: `udp:HOST:PORT` (listens),
    /// `udpout:HOST:PORT` (sends to a UDP server), `serial:DEVICE[@BAUD]`,
    /// `tcp:HOST:PORT` (connects) or `tcpin:HOST:PORT` (listens). Repeat for
    /// several.
    #[arg(long = "in", required = true, value_name = "LINK")]
    inputs: Vec<String>,
    /// Where to forward: `udp:HOST:PORT`. Repeat for several.
    #[arg(long = "out", required = true, value_name = "LINK")]
    outputs: Vec<String>,
    /// Only listen and forward; nothing reaches the vehicles.
    #[arg(long)]
    read_only: bool,
    /// Forward only this vehicle's traffic (its VID). Repeat for several.
    #[arg(long = "vehicle", value_name = "VID")]
    vehicles: Vec<String>,
    #[command(flatten)]
    daemon: DaemonArgs,
}

#[derive(Debug, Subcommand)]
pub enum ApprovalsCommand {
    /// Allow a pending approval.
    Allow {
        id: String,
        /// How long the allowance lasts.
        #[arg(long, value_enum, default_value_t = RememberChoice::Once)]
        remember: RememberChoice,
    },
    /// Deny a pending approval.
    Deny { id: String },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum RememberChoice {
    Once,
    Connection,
    Always,
}

#[derive(Debug, Subcommand)]
pub enum UpdateCommand {
    /// Fetch and verify the channel's signed manifest and compare versions.
    Check {
        /// The channel's manifest.
        #[arg(long, env = "UXDUCK_UPDATE_URL", default_value = crate::update::DEFAULT_URL)]
        url: String,
        /// The release public key, for tests; release builds have it built in.
        #[arg(long, hide = true)]
        public_key: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum PortsCommand {
    /// Stop the program holding a port, after confirming. Only programs of
    /// the same user; never system processes or other users'.
    Free {
        port: u16,
        /// A listening TCP port instead of UDP.
        #[arg(long)]
        tcp: bool,
        /// Do not ask first.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum WatchLinkAction {
    /// Makes a link and prints its address, once: share it with whoever
    /// should watch.
    Make {
        /// Who flies, shown on the page.
        #[arg(long)]
        flown_by: String,
        /// Only this vehicle, by name or id; every vehicle on this computer
        /// when left out.
        #[arg(long)]
        vehicle: Option<String>,
        /// How long it lasts, 1 to 168 hours.
        #[arg(long, default_value_t = 24)]
        hours: u32,
    },
    /// This computer's links that have not ended, with how many watch.
    List {
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Ends a link now: everyone watching through it is closed.
    Revoke {
        /// The link's id, as `uxduck account watch-link list` shows it.
        id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum AccountCommand {
    /// Show your username, or choose or change it. Suggestions you send are
    /// credited to it only when you tick that; profiles are private.
    Username { name: Option<String> },
    /// Where your account is signed in: each computer and browser, this
    /// one marked.
    Sessions {
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Sign out one of the places your account is signed in, by the id
    /// `uxduck account sessions --json` shows, or every one but this
    /// computer with `--others`.
    SignOut {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        others: bool,
    },
    /// Email yourself a link that deletes your account. Nothing is deleted
    /// until you open it and confirm.
    Delete,
    /// The computers on your account, online or when last seen, and how
    /// this one is connected.
    Computers {
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// One of the account's computers, live through the cloud, as someone
    /// watching from anywhere sees it: the first online one unless named,
    /// each vehicle with how old its values are.
    Watch {
        /// The computer's name or id.
        computer: Option<String>,
        /// Open this vehicle in detail, by name or id.
        #[arg(long)]
        vehicle: Option<String>,
        /// Print once, after a few seconds, and stop.
        #[arg(long)]
        once: bool,
    },
    /// A command to a vehicle on another of your computers, through the
    /// cloud: what that computer's "Allow remote control" allows, with the
    /// same receipt as at the computer.
    Command {
        /// The computer's name or id.
        computer: String,
        /// The vehicle's name or id.
        vehicle: String,
        /// Take the vehicle's control lease first, as mission and critical
        /// commands need; it ends when this command does.
        #[arg(long)]
        take_control: bool,
        #[command(subcommand)]
        action: VehicleAction,
    },
    /// Watch links (S95): anyone with one watches this computer, or one
    /// vehicle on it, in a browser, read only, until it ends.
    WatchLink {
        #[command(subcommand)]
        action: WatchLinkAction,
    },
    /// The flights your account keeps, from any of your computers.
    Flights {
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// One flight from your account, joined back into the tlog its computer
    /// recorded.
    Flight {
        /// The computer's name or id.
        computer: String,
        /// The flight, as `uxduck account flights` lists it.
        id: String,
        /// Where to write the tlog.
        #[arg(long, default_value = "flight.tlog")]
        out: PathBuf,
    },
    /// The camera recordings your account keeps, from any of your computers.
    Recordings {
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// One camera recording from your account, joined back into the MP4 its
    /// computer recorded.
    Recording {
        /// The computer's name or id.
        computer: String,
        /// The recording, as `uxduck account recordings` lists it.
        id: String,
        /// Where to write the MP4.
        #[arg(long, default_value = "recording.mp4")]
        out: PathBuf,
    },
    /// "Connect this computer to my account": `on`, as it starts, or `off`.
    Connect {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SimCommand {
    /// Starts a simulated vehicle and prints its link. It appears as a
    /// vehicle once it has started.
    Start {
        /// `multicopter`, `plane`, `vtol`, `rover`, `boat`, `submarine`, `airship`
        /// or `jet`.
        #[arg(long, default_value = "multicopter")]
        vehicle: String,
        /// `ardupilot` or `px4` (macOS and Linux; not a boat, submarine, blimp or
        /// jet).
        #[arg(long, default_value = "ardupilot")]
        autopilot: String,
        /// Where it starts: `latitude,longitude`, with ground level and
        /// heading if you like. ArduPilot's usual field by default.
        #[arg(
            long,
            default_value = "-35.363261,149.165230,584,353",
            allow_hyphen_values = true
        )]
        at: String,
        /// The build's frame, such as ArduPilot's `X` or `quadplane-tilt`,
        /// or PX4's `quadx` (see `uxduck sim builds`).
        #[arg(long)]
        frame: Option<String>,
        /// Times real time, 1 to 10.
        #[arg(long)]
        speedup: Option<u8>,
        /// Starts it flying at this height in metres (10 to 500), holding
        /// position: UXDuck arms it and takes off for you.
        #[arg(long, value_name = "HEIGHT")]
        in_air: Option<f64>,
        /// A SITL build of your own.
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Forget its stored parameters each time it starts.
        #[arg(long)]
        wipe: bool,
        #[arg(long)]
        name: Option<String>,
    },
    /// Stops a simulated vehicle, by its link's id.
    Stop { id: String },
    /// Starts a simulated vehicle again from where it started, by its
    /// link's id: after a crash, or to begin afresh. It keeps its name.
    Restart { id: String },
    /// Saves a snapshot of a simulated vehicle, by its id (S34).
    Snapshot {
        vehicle_id: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// The saved snapshots.
    Snapshots,
    /// Starts a new simulated vehicle from a snapshot, by its id: in the air
    /// where it was if it was flying.
    FromSnapshot {
        id: String,
        /// Start it on the ground even if it was flying.
        #[arg(long)]
        on_ground: bool,
        /// Times real time, 1 to 10.
        #[arg(long)]
        speedup: Option<u8>,
    },
    /// The SITL builds UXDuck found, with their frames.
    Builds,
    /// The catalogue's prebuilt builds for this computer (S18).
    Catalogue {
        /// Fetch it again.
        #[arg(long)]
        refresh: bool,
    },
    /// Downloads and installs a catalogue build, by its id.
    Download { id: String },
    /// Deletes an installed catalogue build, by its id.
    Delete { id: String },
}

#[derive(Debug, Subcommand)]
pub enum DebugCommand {
    /// Make the running daemon panic once it has answered. The panic is
    /// recorded and sent to error tracking after the next start, when usage
    /// analytics are on. Refused by release builds.
    Panic,
}

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// UXDuck opens when you log in, with no page open: Open at login.
    Install,
    /// UXDuck no longer opens at login.
    Uninstall,
}

#[derive(Debug, Subcommand)]
pub enum DevCommand {
    /// A vehicle that sends heartbeats over UDP, answers the request for its
    /// UID like an autopilot, and flies a circle at SITL's default home so
    /// there is telemetry to show. Not a flight simulator.
    FakeVehicle {
        /// Where to send, such as the daemon's UDP link.
        #[arg(long, default_value = "127.0.0.1:15010")]
        to: String,
        #[arg(long, default_value_t = 1)]
        system_id: u8,
        /// The autopilot UID, in hex.
        #[arg(long, default_value = "1234abcd")]
        uid: String,
        /// Never answer the UID request, to test a vehicle without one.
        #[arg(long)]
        no_uid: bool,
        /// The firmware version it reports, such as `4.5.7` or `4.5.8-dev`
        /// (also `-alpha`, `-beta`, `-rc`).
        #[arg(long, default_value = "4.5.1")]
        firmware: String,
        /// The git hash it reports it was built from; its first 8 characters
        /// are sent.
        #[arg(long)]
        commit: Option<String>,
        /// A camera component that names this video stream when asked, such
        /// as `rtsp://127.0.0.1:8554/main` or `udp://127.0.0.1:5600` (S39).
        #[arg(long)]
        camera: Option<String>,
        /// Send ArduPilot's start-up messages, two of them warnings (S26).
        #[arg(long)]
        startup_burst: bool,
        /// Say `PreArm: Throttle below failsafe` every 30 s while disarmed.
        #[arg(long)]
        prearm_failure: bool,
        /// Repeat the warning "Vibration high" every second, from 12 s in.
        #[arg(long)]
        repeat_warning: bool,
        /// Shake more and more from 10 s in, past 30 and 60 m/s² (S72).
        #[arg(long)]
        vibration_high: bool,
        /// Say "Leak Detected!" every 10 s, as `ArduSub` does (S72).
        #[arg(long)]
        leak: bool,
        /// No MAVLink FTP: parameters come by the list, without defaults
        /// (S74), and the Files tab has nothing to show (S121).
        #[arg(long)]
        no_ftp: bool,
        /// Lose every seventh MAVLink FTP answer, as a lossy radio does (S121).
        #[arg(long)]
        drop_ftp: bool,
        /// Start with no terrain and ask UXDuck for it, as ArduPilot with an
        /// empty terrain folder (S118); without it, terrain is fixed.
        #[arg(long)]
        terrain: bool,
        /// With `--terrain`, keep every height this many metres off, as a
        /// card holding another dataset (S119).
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        terrain_shift: i16,
        /// Fail the pre-arm checks: arming is refused unless forced (S53).
        #[arg(long)]
        refuse_arm: bool,
        /// Refuse to disarm unless forced, as a vehicle that thinks it is
        /// still flying does.
        #[arg(long)]
        refuse_disarm: bool,
        /// The battery's % at the start, such as 45 to see it low (S77).
        #[arg(long, default_value_t = 100.0)]
        battery: f64,
        /// Never have a GPS fix, so never a position.
        #[arg(long, conflicts_with = "lose_gps_at")]
        no_gps: bool,
        /// Lose the GPS fix, and with it the position, this many seconds in.
        #[arg(long)]
        lose_gps_at: Option<f64>,
    },
    /// Runs a SITL build and stops it when standard input closes: the
    /// simulation manager's guard, so no build outlives the daemon (S17).
    SimGuard {
        /// Where the build's output goes.
        #[arg(long)]
        log: PathBuf,
        /// The build, then its arguments.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Sends a tlog's frames over UDP with their recorded timing, as the
    /// vehicle sent them: a feedback report's `flight.tlog` brings back
    /// the vehicle's side of the moment (S31).
    Replay {
        tlog: PathBuf,
        /// Where to send, such as the daemon's UDP link.
        #[arg(long, default_value = "127.0.0.1:15010")]
        to: String,
        /// Faster (2) or slower (0.5) than recorded.
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
        /// Sends nothing: judges a practice exercise over the tlog, such as
        /// `bring-it-home`, and prints its steps and result (S100).
        #[arg(long)]
        practice: Option<String>,
        /// With `--practice`, seconds into the tlog the attempt began.
        #[arg(long, default_value_t = 0.0, requires = "practice")]
        from: f64,
    },
    /// Prints a tlog's MAVLink messages by name, one JSON line each, as
    /// plugins receive them; `--plugin` adds that plugin's vendor dialect
    /// (S60). Frames no dialect knows are printed by id.
    Decode {
        tlog: PathBuf,
        /// A plugin folder whose `plugin.json` names a dialect.
        #[arg(long)]
        plugin: Option<PathBuf>,
    },
    /// Sends an MPEG-TS capture over UDP as a camera would, byte for byte
    /// and paced by its PCR: seven packets to a datagram (S38).
    VideoReplay {
        capture: PathBuf,
        /// Where to send, such as UXDuck's video port.
        #[arg(long, default_value = "127.0.0.1:5600")]
        to: String,
        /// Wrap each datagram in RTP (payload type 33), as many cameras do.
        #[arg(long, conflicts_with = "rtp_video")]
        rtp: bool,
        /// Send the pictures as RTP carrying H.264 or H.265 straight
        /// (payload type 96), as QGroundControl sends by default.
        #[arg(long)]
        rtp_video: bool,
        /// Start again from the first PAT at the end, until interrupted.
        #[arg(long = "loop")]
        repeat: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// `uxduck config set openai.key` or `anthropic.key`: reads the value
    /// from standard input when it is not given, so it stays out of shell history.
    /// `uxduck config set interface.updates off` pins the interface UXDuck
    /// serves now, for sites that must not change software by themselves.
    /// `uxduck config set interface.channel staging` follows staging's
    /// interface instead of production's, for the maintainer; About says so.
    Set { name: String, value: Option<String> },
}

#[derive(Debug, Subcommand)]
pub enum VehicleAction {
    Arm,
    Disarm,
    Land,
    /// Return to launch.
    Rtl,
    /// A flight mode by name, such as `LOITER`.
    Mode {
        name: String,
    },
    /// `MAV_CMD_NAV_TAKEOFF` to an altitude in metres.
    Takeoff {
        #[arg(long, default_value_t = 10.0)]
        altitude: f32,
    },
    /// `MAV_CMD_MISSION_START`: flies the uploaded mission.
    MissionStart,
    /// Goes to a place and holds there, at a height above home, sea
    /// level or the terrain (`DO_REPOSITION` in GUIDED).
    GoTo {
        #[arg(long)]
        lat: f64,
        #[arg(long)]
        lon: f64,
        /// Metres, in `--frame`.
        #[arg(long, default_value_t = 30.0)]
        altitude: f64,
        /// What the height is measured from: `above-home`, `amsl` or
        /// `above-ground`.
        #[arg(long, default_value = "above-home")]
        frame: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum RpcCommand {
    /// `uxduck rpc call pairing/approve --json '{"code":"123456"}'`
    Call {
        method: String,
        /// Params as a JSON object.
        #[arg(long, value_name = "JSON")]
        json: Option<String>,
    },
}

/// Runs the parsed command line.
pub async fn run(cli: Cli) -> ExitCode {
    if let Err(error) =
        crate::dev_api::initialize(&crate::paths::data_dir(cli.data_dir.as_deref(), cli.dev))
    {
        crate::startup::record(&error);
        eprintln!("uxduck: {error}");
        return ExitCode::from(2);
    }
    // Daemons handle EOF inside their normal shutdown path, including the
    // final checkpoint. Simulator guards already own their stdin and must
    // reap their child themselves. Other test helpers just end.
    if !cli.exit_on_stdin_close
        || matches!(
            &cli.command,
            Some(
                Command::Daemon(_)
                    | Command::Forward(_)
                    | Command::Dev {
                        command: DevCommand::SimGuard { .. },
                    }
            )
        )
    {
        return run_command(cli).await;
    }
    let closed = match crate::parent::stdin_closed(true) {
        Ok(closed) => closed,
        Err(error) => {
            crate::startup::record(&format!("cannot watch the parent pipe: {error}"));
            eprintln!("uxduck: cannot watch the parent pipe: {error}");
            return ExitCode::from(2);
        }
    };
    tokio::select! {
        code = run_command(cli) => code,
        () = closed => ExitCode::SUCCESS,
    }
}

#[allow(clippy::too_many_lines, reason = "one arm per command")]
async fn run_command(cli: Cli) -> ExitCode {
    let data_dir = crate::paths::data_dir(cli.data_dir.as_deref(), cli.dev);
    let owned = cli.exit_on_stdin_close;
    let result = match cli.command {
        None => {
            eprintln!("uxduck {}: try `uxduck --help`", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some(Command::Daemon(DaemonArgs {
            action: Some(action),
            ..
        })) => daemon_action(&data_dir, action).await,
        Some(Command::Daemon(args)) => {
            run_daemon(args, data_dir, cli.dev, Vec::new(), false, owned).await
        }
        Some(Command::Forward(args)) => {
            let links = match link_spec::forward_links(
                &args.inputs,
                &args.outputs,
                args.read_only,
                &args.vehicles,
            ) {
                Ok(links) => links,
                Err(error) => {
                    eprintln!("uxduck forward: {error}");
                    return ExitCode::from(2);
                }
            };
            run_daemon(args.daemon, data_dir, cli.dev, links, true, owned).await
        }
        Some(Command::Update {
            command: UpdateCommand::Check { url, public_key },
        }) => {
            let key = public_key
                .as_deref()
                .or(crate::update::APP_UPDATE_PUBLIC_KEY);
            return match crate::update::check(&url, key, &data_dir).await {
                Ok(report) => {
                    println!("{report}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("uxduck update check: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        Some(Command::Mcp { all_tools }) => crate::mcp::run(&data_dir, all_tools).await,
        Some(Command::Approvals { command }) => approvals(&data_dir, command).await,
        Some(Command::Plugin { command }) => plugin(&data_dir, command).await,
        Some(Command::Ports { command: None }) => ports_list(&data_dir).await,
        Some(Command::Ports {
            command: Some(PortsCommand::Free { port, tcp, yes }),
        }) => {
            ports_free(port, tcp, yes);
            Ok(())
        }
        Some(Command::Rpc {
            command: RpcCommand::Call { method, json },
        }) => rpc_call(&data_dir, &method, json.as_deref()).await,
        Some(Command::Send {
            action,
            vehicle,
            confirm,
            yes,
        }) => vehicle_command(&data_dir, action, vehicle, confirm, yes).await,
        Some(Command::Trace) => trace(&data_dir).await,
        Some(Command::Debug {
            command: DebugCommand::Panic,
        }) => rpc_call(&data_dir, "debug/panic", None).await,
        Some(Command::Wakeups { seconds }) => wakeups(&data_dir, seconds).await,
        Some(Command::Dev { command }) => dev(command).await,
        Some(Command::Sim { command }) => sim(&data_dir, command).await,
        Some(Command::Params { command }) => params::run(&data_dir, command).await,
        Some(Command::Vehicle { command }) => vehicle::run(&data_dir, command).await,
        Some(Command::Practice { command }) => practice::run(&data_dir, command).await,
        Some(Command::Plan { command }) => plan::run(&data_dir, command).await,
        Some(Command::Mission { command }) => mission::run(&data_dir, command).await,
        Some(Command::Terrain { command }) => terrain::run(&data_dir, command).await,
        Some(Command::Video { command }) => video::run(&data_dir, command).await,
        Some(Command::Find {
            network,
            all_serial,
            json,
        }) => find::run(&data_dir, network, all_serial, json).await,
        Some(Command::Login {
            email,
            product_updates,
        }) => login(&data_dir, email, product_updates).await,
        Some(Command::Logout) => logout(&data_dir).await,
        Some(Command::Account { command }) => account(&data_dir, command).await,
        Some(Command::Config {
            command: ConfigCommand::Set { name, value },
        }) => config_set(&data_dir, &name, value).await,
        Some(Command::Open(args)) => open(&data_dir, cli.dev, args.print, args.target()).await,
        Some(Command::Service { command }) => service(&command),
        Some(Command::Status { json }) => {
            status(&data_dir, json);
            Ok(())
        }
    };
    exit_code(result)
}

/// How a command ended: an API error as JSON, anything else in words.
fn exit_code(result: Result<(), CliError>) -> ExitCode {
    if let Err(error) = &result {
        crate::startup::record(&error.to_string());
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Api(error)) => {
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&error).unwrap_or_default()
            );
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("uxduck: {error}");
            ExitCode::from(2)
        }
    }
}

/// `uxduck dev`: tools for development and tests.
async fn dev(command: DevCommand) -> Result<(), CliError> {
    match command {
        DevCommand::FakeVehicle {
            to,
            system_id,
            uid,
            no_uid,
            firmware,
            commit,
            camera,
            refuse_arm,
            refuse_disarm,
            startup_burst,
            prearm_failure,
            repeat_warning,
            vibration_high,
            leak,
            no_ftp,
            drop_ftp,
            terrain,
            terrain_shift,
            battery,
            no_gps,
            lose_gps_at,
        } => {
            let version = FakeVersion::parse(&firmware, commit.as_deref())?;
            let options = FakeOptions {
                no_uid,
                camera,
                refuse_arm,
                refuse_disarm,
                startup_burst,
                prearm_failure,
                repeat_warning,
                vibration_high,
                leak,
                no_ftp,
                drop_ftp,
                terrain,
                terrain_shift,
                battery,
                gps_lost_from_s: if no_gps { Some(0.0) } else { lose_gps_at },
            };
            fake_vehicle(&to, system_id, &uid, version, options).await
        }
        DevCommand::Replay {
            practice: Some(exercise),
            tlog,
            from,
            ..
        } => {
            let replayed = practice_replay::judge_tlog(&std::fs::read(&tlog)?, &exercise, from)
                .map_err(CliError::Usage)?;
            practice_replay::print(&exercise, &replayed);
            Ok(())
        }
        DevCommand::Replay {
            tlog, to, speed, ..
        } => replay(&tlog, &to, speed).await,
        DevCommand::Decode { tlog, plugin } => decode_tlog(&tlog, plugin.as_deref()),
        DevCommand::VideoReplay {
            capture,
            to,
            rtp,
            rtp_video,
            repeat,
        } => video_replay(&capture, &to, rtp, rtp_video, repeat).await,
        DevCommand::SimGuard { log, command } => match command.split_first() {
            // The build's exit code is passed on, for the daemon to report.
            Some((program, args)) => {
                std::process::exit(crate::sim::guard(&log, std::path::Path::new(program), args))
            }
            None => Err(CliError::Usage("sim-guard needs a build to run".into())),
        },
    }
}

fn prompt(question: &str) -> Result<String, CliError> {
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprint!("{question}");
    }
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

/// `uxduck login`: the email code sign-in, in a terminal (ADR-0013 P9).
async fn login(
    data_dir: &std::path::Path,
    email: Option<String>,
    product_updates: bool,
) -> Result<(), CliError> {
    let email = match email {
        Some(email) => email,
        None => prompt("Email: ")?,
    };
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    client
        .call("account/signIn", serde_json::json!({ "email": email }))
        .await?;
    eprintln!("A code is on its way to {email}.");
    let code = prompt("Code: ")?;
    let state = client
        .call(
            "account/verify",
            serde_json::json!({ "email": email, "code": code, "productUpdates": product_updates }),
        )
        .await?;
    let email = state["email"].as_str().unwrap_or(email.as_str());
    if state["created"] == true {
        eprintln!("Account created. Signed in as {email}");
    } else {
        eprintln!("Signed in as {email}");
    }
    Ok(())
}

/// `uxduck account username [NAME]` (S73).
async fn username(data_dir: &std::path::Path, name: Option<&str>) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let account = match name {
        Some(name) => {
            client
                .call(
                    "account/setUsername",
                    serde_json::json!({ "username": name }),
                )
                .await?
        }
        None => client.call("account/get", serde_json::json!({})).await?,
    };
    if account["signedIn"] != true {
        eprintln!("You're not signed in. Run `uxduck login` first.");
        return Ok(());
    }
    match account["username"].as_str() {
        Some(username) => println!("@{username}"),
        None => eprintln!("No username yet; choose one with `uxduck account username NAME`"),
    }
    Ok(())
}

/// `uxduck account sessions` (S114).
async fn sessions(data_dir: &std::path::Path, json: bool) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let answer = client
        .call("account/sessions", serde_json::json!({}))
        .await?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&answer).unwrap_or_default()
        );
        return Ok(());
    }
    for session in answer["sessions"].as_array().into_iter().flatten() {
        let place = session["computer"].as_str().map_or_else(
            || {
                format!(
                    "UXDuck Web · {}",
                    session["userAgent"].as_str().unwrap_or("a browser")
                )
            },
            |name| format!("UXDuck Desktop · {name}"),
        );
        let day = session["lastActiveAt"]
            .as_str()
            .unwrap_or("")
            .get(..10)
            .unwrap_or("");
        let mark = if session["current"] == true {
            "  (this computer)"
        } else {
            ""
        };
        println!("{place}  last used {day}{mark}");
    }
    Ok(())
}

/// `uxduck account sign-out` (S114).
async fn sign_out_session(
    data_dir: &std::path::Path,
    id: Option<String>,
    others: bool,
) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let state = match (id, others) {
        (_, true) => {
            client
                .call("account/signOutOthers", serde_json::json!({}))
                .await?
        }
        (Some(id), false) => {
            client
                .call("account/signOutSession", serde_json::json!({ "id": id }))
                .await?
        }
        (None, false) => {
            return Err(CliError::Usage(
                "give a session's id, or --others for every one but this computer".into(),
            ));
        }
    };
    if state["signedIn"] == true {
        eprintln!("Signed out.");
    } else {
        eprintln!("Signed out, this computer too.");
    }
    Ok(())
}

/// `uxduck account delete` (S114, ADR-0013 P23).
async fn request_deletion(data_dir: &std::path::Path) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let account = client.call("account/get", serde_json::json!({})).await?;
    if account["signedIn"] != true {
        eprintln!("You're not signed in. Run `uxduck login` first.");
        return Ok(());
    }
    client
        .call("account/requestDeletion", serde_json::json!({}))
        .await?;
    eprintln!(
        "A link is on its way to {}. Open it within an hour and confirm to delete your account.",
        account["email"].as_str().unwrap_or("your email")
    );
    Ok(())
}

async fn account(data_dir: &std::path::Path, command: AccountCommand) -> Result<(), CliError> {
    use crate::uploader::Stored;
    match command {
        AccountCommand::Username { name } => username(data_dir, name.as_deref()).await,
        AccountCommand::Sessions { json } => sessions(data_dir, json).await,
        AccountCommand::SignOut { id, others } => sign_out_session(data_dir, id, others).await,
        AccountCommand::Delete => request_deletion(data_dir).await,
        AccountCommand::Computers { json } => computers(data_dir, json).await,
        AccountCommand::WatchLink { action } => watch_link(data_dir, action).await,
        AccountCommand::Connect { state } => connect_to_account(data_dir, state == "on").await,
        AccountCommand::Flights { json } => crate::uploader::list(data_dir, Stored::Flights, json)
            .await
            .map_err(CliError::Usage),
        AccountCommand::Flight { computer, id, out } => {
            crate::uploader::download(data_dir, Stored::Flights, &computer, &id, &out)
                .await
                .map_err(CliError::Usage)
        }
        AccountCommand::Recordings { json } => {
            crate::uploader::list(data_dir, Stored::Recordings, json)
                .await
                .map_err(CliError::Usage)
        }
        AccountCommand::Recording { computer, id, out } => {
            crate::uploader::download(data_dir, Stored::Recordings, &computer, &id, &out)
                .await
                .map_err(CliError::Usage)
        }
        AccountCommand::Watch {
            computer,
            vehicle,
            once,
        } => crate::watch::watch(data_dir, computer.as_deref(), vehicle.as_deref(), once)
            .await
            .map_err(CliError::Usage),
        AccountCommand::Command {
            computer,
            vehicle,
            take_control,
            action,
        } => crate::watch::command(
            data_dir,
            &computer,
            &vehicle,
            action_json(&action),
            take_control,
        )
        .await
        .map_err(CliError::Usage),
    }
}

async fn watch_link(data_dir: &std::path::Path, action: WatchLinkAction) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match action {
        WatchLinkAction::Make {
            flown_by,
            vehicle,
            hours,
        } => {
            let vehicle_id = match vehicle {
                Some(wanted) => Some(vehicle::resolve(&mut client, &wanted).await?),
                None => None,
            };
            let made = client
                .call(
                    "watchLink/create",
                    serde_json::json!({ "vehicleId": vehicle_id, "flownBy": flown_by, "hours": hours }),
                )
                .await?;
            println!("{}", made["url"].as_str().unwrap_or_default());
            eprintln!(
                "Anyone with this address can watch until it ends or you revoke it (`uxduck account watch-link revoke {}`).",
                made["link"]["id"].as_str().unwrap_or_default()
            );
        }
        WatchLinkAction::List { json } => {
            let listed = client.call("watchLink/list", serde_json::json!({})).await?;
            if json {
                println!("{listed}");
                return Ok(());
            }
            let links = listed["links"].as_array().cloned().unwrap_or_default();
            if links.is_empty() {
                println!("No watch links.");
            }
            for entry in &links {
                let link = &entry["link"];
                let ends_in = link["expiresAtMs"]
                    .as_u64()
                    .map(|at| {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
                        format!("ends in {} min", at.saturating_sub(now) / 60_000)
                    })
                    .unwrap_or_default();
                println!(
                    "{}  {}  flown by {}  {} watching  {ends_in}",
                    link["id"].as_str().unwrap_or_default(),
                    link["vehicleName"].as_str().unwrap_or("every vehicle"),
                    link["flownBy"].as_str().unwrap_or_default(),
                    entry["watching"].as_u64().unwrap_or(0),
                );
            }
        }
        WatchLinkAction::Revoke { id } => {
            client
                .call("watchLink/revoke", serde_json::json!({ "linkId": id }))
                .await?;
            println!("Revoked: anyone watching through it has been closed.");
        }
    }
    Ok(())
}

async fn computers(data_dir: &std::path::Path, as_json: bool) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let listed = client
        .call("account/computers", serde_json::json!({}))
        .await?;
    if as_json {
        println!("{listed}");
        return Ok(());
    }
    let edge = &listed["edge"];
    match edge["state"].as_str() {
        Some("signed-out") => {
            eprintln!("You're not signed in. Run `uxduck login` first.");
            return Ok(());
        }
        Some("off") => println!(
            "This computer is not connected to your account (`uxduck account connect on`)."
        ),
        Some("connected") => println!(
            "This computer is connected to your account{}.",
            edge["roundTripMs"]
                .as_u64()
                .map(|ms| format!(", {ms} ms away"))
                .unwrap_or_default()
        ),
        _ => println!(
            "Connecting this computer to your account{}",
            edge["problem"]
                .as_str()
                .map_or_else(|| "…".into(), |why| format!(": {why}"))
        ),
    }
    let this = listed["thisComputer"].as_str();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    for computer in listed["computers"].as_array().into_iter().flatten() {
        let name = computer["name"].as_str().unwrap_or("A computer");
        let mark = if computer["id"].as_str() == this {
            " (this one)"
        } else {
            ""
        };
        let state = if computer["online"] == true {
            "online".to_owned()
        } else {
            computer["lastSeenAtMs"].as_u64().map_or_else(
                || "offline".to_owned(),
                |at| format!("last seen {}", ago(now_ms.saturating_sub(at))),
            )
        };
        println!("  {name}{mark}: {state}");
    }
    Ok(())
}

async fn connect_to_account(data_dir: &std::path::Path, on: bool) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let state = client
        .call(
            "state/subscribe",
            serde_json::json!({ "entities": ["settings"] }),
        )
        .await?;
    let version = state["snapshot"]["settings"]["version"]
        .as_u64()
        .unwrap_or(0);
    client
        .call(
            "settings/update",
            serde_json::json!({ "expectedVersion": version, "connectToAccount": on }),
        )
        .await?;
    eprintln!(
        "{}",
        if on {
            "This computer connects to your account while you are signed in."
        } else {
            "This computer no longer connects to your account."
        }
    );
    Ok(())
}

/// A duration in the past, roughly: "12 s ago", "3 min ago", "2 h ago", "4 days ago".
fn ago(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s} s ago"),
        60..3600 => format!("{} min ago", s / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86_400),
    }
}

async fn logout(data_dir: &std::path::Path) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    client
        .call("account/signOut", serde_json::json!({}))
        .await?;
    eprintln!("Signed out");
    Ok(())
}

/// `uxduck sim`: the simulation manager through the API (S17).
async fn sim(data_dir: &std::path::Path, command: SimCommand) -> Result<(), CliError> {
    match command {
        SimCommand::Start {
            vehicle,
            autopilot,
            at,
            frame,
            speedup,
            in_air,
            binary,
            wipe,
            name,
        } => {
            let numbers: Vec<f64> = at
                .split(',')
                .map(|part| part.trim().parse::<f64>())
                .collect::<Result<_, _>>()
                .map_err(|e| CliError::Usage(format!("--at: {e}")))?;
            let [latitude, longitude, rest @ ..] = numbers.as_slice() else {
                return Err(CliError::Usage("--at is latitude,longitude".into()));
            };
            let params = serde_json::json!({
                "vehicle": vehicle,
                "autopilot": autopilot,
                "latitudeDeg": latitude,
                "longitudeDeg": longitude,
                "altitudeM": rest.first(),
                "headingDeg": rest.get(1),
                "frame": frame,
                "speedup": speedup,
                "inAirM": in_air,
                "binary": binary.map(|b| b.display().to_string()),
                "wipe": wipe,
                "name": name,
            });
            let params = strip_nulls(params).to_string();
            rpc_call(data_dir, "sim/start", Some(&params)).await
        }
        SimCommand::Stop { id } => {
            let params = serde_json::json!({ "id": id }).to_string();
            rpc_call(data_dir, "sim/stop", Some(&params)).await
        }
        SimCommand::Restart { id } => {
            let params = serde_json::json!({ "id": id }).to_string();
            rpc_call(data_dir, "sim/restart", Some(&params)).await
        }
        SimCommand::Snapshot { vehicle_id, name } => {
            let params = serde_json::json!({ "vehicleId": vehicle_id, "name": name });
            let params = strip_nulls(params).to_string();
            rpc_call(data_dir, "sim/snapshot", Some(&params)).await
        }
        SimCommand::Snapshots => rpc_call(data_dir, "snapshot/list", None).await,
        SimCommand::FromSnapshot {
            id,
            on_ground,
            speedup,
        } => {
            let params = serde_json::json!({ "id": id, "onGround": on_ground, "speedup": speedup });
            let params = strip_nulls(params).to_string();
            rpc_call(data_dir, "snapshot/start", Some(&params)).await
        }
        SimCommand::Builds => rpc_call(data_dir, "sim/binaries", None).await,
        SimCommand::Catalogue { refresh } => {
            let params = serde_json::json!({ "refresh": refresh }).to_string();
            rpc_call(data_dir, "sim/catalogue", Some(&params)).await
        }
        SimCommand::Download { id } => {
            let params = serde_json::json!({ "id": id }).to_string();
            rpc_call(data_dir, "sim/download", Some(&params)).await
        }
        SimCommand::Delete { id } => {
            let params = serde_json::json!({ "id": id }).to_string();
            rpc_call(data_dir, "sim/delete", Some(&params)).await
        }
    }
}

/// Leaves out the options that were not given.
fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).collect())
        }
        other => other,
    }
}

async fn rpc_call(
    data_dir: &std::path::Path,
    method: &str,
    json: Option<&str>,
) -> Result<(), CliError> {
    let params: Value = match json {
        Some(text) => serde_json::from_str(text)
            .map_err(|e| CliError::Usage(format!("--json is not valid JSON: {e}")))?,
        None => Value::Object(serde_json::Map::new()),
    };
    let mut client = LocalClient::connect(data_dir).await?;
    let result = if method == "initialize" {
        if json.is_some() {
            client.call("initialize", params).await?
        } else {
            client.initialize().await?
        }
    } else {
        client.initialize().await?;
        client.call(method, params).await?
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&result).unwrap_or_default()
    );
    Ok(())
}

/// A `uxduck command` action as the `Command` contract has it.
pub(crate) fn action_json(action: &VehicleAction) -> Value {
    match action {
        VehicleAction::Arm => serde_json::json!({ "type": "arm" }),
        VehicleAction::Disarm => serde_json::json!({ "type": "disarm" }),
        VehicleAction::Land => serde_json::json!({ "type": "land" }),
        VehicleAction::Rtl => serde_json::json!({ "type": "rtl" }),
        VehicleAction::Mode { name } => serde_json::json!({ "type": "set-mode", "mode": name }),
        VehicleAction::Takeoff { altitude } => serde_json::json!({
            "type": "long",
            "command": 22,
            "params": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, altitude],
        }),
        VehicleAction::MissionStart => {
            serde_json::json!({ "type": "long", "command": 300, "params": [0.0, 0.0] })
        }
        VehicleAction::GoTo {
            lat,
            lon,
            altitude,
            frame,
        } => serde_json::json!({
            "type": "go-to",
            "latitudeDeg": lat,
            "longitudeDeg": lon,
            "altitudeM": altitude,
            "altitudeFrame": frame,
        }),
    }
}

/// `uxduck command`: one `command/send`, and its receipt. A command that
/// was not sent, or that the vehicle refused, fails.
async fn vehicle_command(
    data_dir: &std::path::Path,
    action: VehicleAction,
    vehicle: Option<String>,
    confirm: bool,
    yes: bool,
) -> Result<(), CliError> {
    let command = action_json(&action);
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let vehicle_id = if let Some(id) = vehicle {
        id
    } else {
        let list = client.call("vehicle/list", serde_json::json!({})).await?;
        let vehicles = list["vehicles"].as_array().cloned().unwrap_or_default();
        match vehicles.as_slice() {
            [one] => one["id"].as_str().unwrap_or_default().to_owned(),
            [] => return Err(CliError::Usage("no vehicle is connected".into())),
            _ => {
                return Err(CliError::Usage(
                    "more than one vehicle: name one with --vehicle".into(),
                ));
            }
        }
    };
    if !yes {
        ask_first(&mut client, &vehicle_id, &command).await?;
    }
    let receipt = client
        .call(
            "command/send",
            serde_json::json!({
                "vehicleId": vehicle_id,
                "command": command,
                "confirm": confirm,
            }),
        )
        .await?;
    let status = receipt["status"].as_str().unwrap_or("unknown");
    match receipt["detail"].as_str() {
        Some(detail) => println!("{status}: {detail}"),
        None => println!("{status}"),
    }
    if status == "accepted" {
        Ok(())
    } else {
        Err(CliError::Api(receipt))
    }
}

/// As the app does (S53): a command whose risk, on the ground or, while
/// the vehicle is armed, in flight, is to confirm or dangerous asks first
/// in a terminal. A script is not asked, but a dangerous command from one
/// needs `--yes`, so nothing sends one without saying so; `--yes` skips
/// the question everywhere.
async fn ask_first(
    client: &mut LocalClient,
    vehicle_id: &str,
    command: &Value,
) -> Result<(), CliError> {
    let listed = client
        .call("command/list", serde_json::json!({ "id": vehicle_id }))
        .await?;
    let vehicles = client.call("vehicle/list", serde_json::json!({})).await?;
    let vehicle = vehicles["vehicles"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|v| v["id"] == vehicle_id);
    let armed = vehicle.is_some_and(|v| v["armed"] == true);
    let name = vehicle
        .and_then(|v| v["name"].as_str())
        .unwrap_or(vehicle_id)
        .to_owned();
    let same = |info: &&Value| {
        info["command"]["type"] == command["type"]
            && (command["type"] != "set-mode" || info["command"]["mode"] == command["mode"])
    };
    let Some(info) = listed["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .find(same)
    else {
        return Ok(());
    };
    let risk = if armed {
        &info["riskInFlight"]
    } else {
        &info["risk"]
    };
    let level = risk["level"].as_str().unwrap_or("none");
    if level == "none" {
        return Ok(());
    }
    let label = info["label"].as_str().unwrap_or("Send it");
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        if level != "dangerous" {
            return Ok(());
        }
        let why = risk["reason"].as_str().unwrap_or("It is dangerous now.");
        return Err(CliError::Usage(format!(
            "not sent: {label} {name}? {why} Run it again with --yes to send it."
        )));
    }
    if let Some(why) = risk["reason"].as_str() {
        eprintln!("{why}");
    }
    let answer = prompt(&format!("{label} {name}? [y/N] "))?;
    if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") {
        Ok(())
    } else {
        Err(CliError::Usage("not sent".into()))
    }
}

/// Two `daemon/wakeups` readings `seconds` apart, as a table of rates.
async fn wakeups(data_dir: &std::path::Path, seconds: u64) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let read = |value: Value| -> std::collections::BTreeMap<String, u64> {
        value["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| Some((t["name"].as_str()?.to_owned(), t["count"].as_u64()?)))
            .collect()
    };
    let empty = || Value::Object(serde_json::Map::new());
    let before = read(client.call("daemon/wakeups", empty()).await?);
    let started = std::time::Instant::now();
    tokio::time::sleep(std::time::Duration::from_secs(seconds.max(1))).await;
    let after = read(client.call("daemon/wakeups", empty()).await?);
    let elapsed = started.elapsed().as_secs_f64();
    let mut rows: Vec<(String, u64, u64)> = after
        .into_iter()
        .map(|(name, total)| {
            let since = total.saturating_sub(before.get(&name).copied().unwrap_or(0));
            (name, since, total)
        })
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    println!("{:<40} {:>10} {:>10}", "Task", "per second", "total");
    for (name, since, total) in rows {
        #[allow(clippy::cast_precision_loss)]
        let rate = since as f64 / elapsed;
        println!("{name:<40} {rate:>10.1} {total:>10}");
    }
    println!(
        "\nThe engine counts once per piece of work; its own calls from this command add a few."
    );
    Ok(())
}

async fn trace(data_dir: &std::path::Path) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    client
        .call("trace/subscribe", Value::Object(serde_json::Map::new()))
        .await?;
    while let Some(notification) = client.next_notification().await? {
        if notification["method"] == "trace/frame" {
            println!("{}", notification["params"]);
        }
    }
    Ok(())
}

/// Where things are: the data directory, how to reach its daemon, and
/// whether one is running.
fn status(data_dir: &std::path::Path, json: bool) {
    let info = crate::paths::read_info(data_dir);
    #[cfg(unix)]
    let endpoint = crate::paths::socket_path(data_dir).display().to_string();
    #[cfg(not(unix))]
    let endpoint = crate::paths::pipe_name(data_dir);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "dataDir": data_dir.display().to_string(),
                "socket": info.as_ref().map_or(endpoint, |i| i.socket.clone()),
                "running": info.is_some(),
                "daemon": info,
            })
        );
    } else if let Some(info) = info {
        println!(
            "uxduck {} is running (pid {}) on port {}, data in {}",
            info.version,
            info.pid,
            info.port,
            data_dir.display()
        );
    } else {
        println!("No daemon is running for {}", data_dir.display());
    }
}

/// Starts UXDuck in the background for `open` and waits until it answers.
/// It outlives this command, in its own process group, with no terminal.
pub(crate) async fn start_daemon(
    data_dir: &std::path::Path,
    dev: bool,
) -> Result<LocalClient, CliError> {
    // Several explicit app launches may arrive together. Serialize startup,
    // then check again: the daemon's lifetime lock alone only rejects a loser.
    std::fs::create_dir_all(data_dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data_dir.join("daemon-start.lock"))?;
    let _startup = tokio::task::spawn_blocking(move || {
        lock.lock()?;
        Ok::<_, std::io::Error>(lock)
    })
    .await
    .map_err(|error| CliError::Usage(format!("could not coordinate UXDuck startup: {error}")))??;
    if let Ok(client) = LocalClient::connect(data_dir).await {
        return Ok(client);
    }
    let exe = std::env::current_exe()
        .map_err(|e| CliError::Usage(format!("cannot find uxduck to start it: {e}")))?;
    let mut command = std::process::Command::new(exe);
    let mut diagnostic = tempfile::NamedTempFile::new()?;
    command.env(crate::startup::ERROR_FILE, diagnostic.path());
    if dev {
        command.arg("--dev");
    }
    command
        .arg("--data-dir")
        .arg(data_dir)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let mut child = command
        .spawn()
        .map_err(|e| CliError::Usage(format!("could not start UXDuck: {e}")))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(client) = LocalClient::connect(data_dir).await {
            return Ok(client);
        }
        if let Ok(Some(status)) = child.try_wait() {
            let reason = crate::startup::read(diagnostic.as_file_mut())?;
            return Err(CliError::Usage(format!(
                "UXDuck stopped while starting ({status}). {reason}"
            )));
        }
        if std::time::Instant::now() > deadline {
            return Err(CliError::Usage(
                "UXDuck did not start within 30 seconds; run `uxduck daemon` to see why".into(),
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

async fn open(
    data_dir: &std::path::Path,
    dev: bool,
    print: bool,
    target: Option<&str>,
) -> Result<(), CliError> {
    let (mut client, reopened) = match LocalClient::connect(data_dir).await {
        Ok(client) => (client, true),
        Err(CliError::NoDaemon(_)) => (start_daemon(data_dir, dev).await?, false),
        Err(error) => return Err(error),
    };
    client.initialize().await?;
    if !print {
        if focus_after_update(&mut client, data_dir, target).await? {
            return Ok(());
        }
        // The desktop app when chosen (S110), else a page already open comes
        // forward (S107), else a new page in a browser. Opened again while
        // it ran, the tray says so under its icon; never at first start.
        let opened = client
            .call(
                "app/open",
                serde_json::json!({ "reopened": reopened, "target": target }),
            )
            .await?;
        let Some(url) = opened["url"].as_str() else {
            eprintln!("UXDuck is running.");
            return Ok(());
        };
        crate::adapters::browser::open(url)
            .map_err(|e| CliError::Usage(format!("could not open a browser: {e}; open {url}")))?;
        return Ok(());
    }
    let bootstrap = client
        .call(
            "session/createBootstrap",
            Value::Object(serde_json::Map::new()),
        )
        .await?;
    let (Some(ui_url), Some(token)) = (bootstrap["uiUrl"].as_str(), bootstrap["token"].as_str())
    else {
        return Err(CliError::Protocol(
            "session/createBootstrap returned no token".into(),
        ));
    };
    println!("{ui_url}?bootstrap={token}");
    Ok(())
}

/// Reconnecting browser pages get a grace period after an update. A saved
/// menu-bar/window choice must never focus a browser as a side effect.
async fn focus_after_update(
    client: &mut LocalClient,
    data_dir: &std::path::Path,
    target: Option<&str>,
) -> Result<bool, CliError> {
    if std::fs::remove_file(data_dir.join(REOPEN_AFTER_UPDATE)).is_err() {
        return Ok(false);
    }
    let browser = if let Some(target) = target {
        target == "browser"
    } else {
        let state = client
            .call(
                "state/subscribe",
                serde_json::json!({"entities":["settings"]}),
            )
            .await?;
        state["snapshot"]["settings"]["launchAction"] == "browser"
    };
    if !browser {
        return Ok(false);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let focus = client.call("app/focus", serde_json::json!({})).await?;
        if focus["focused"] == true {
            return Ok(true);
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Ok(false)
}

/// Release builds use the hosted model only (ADR-0013 P5).
const DEFAULT_PROVIDER: &str = if cfg!(debug_assertions) {
    "auto"
} else {
    "uxduck"
};

async fn config_set(
    data_dir: &std::path::Path,
    name: &str,
    value: Option<String>,
) -> Result<(), CliError> {
    // A setting of the interface store (S109), for sites that must not
    // change software by themselves; read at UXDuck's next check.
    if name == "interface.updates" {
        let value = value.unwrap_or_default();
        crate::interface::set_updates(data_dir, &value).map_err(CliError::Usage)?;
        eprintln!(
            "interface.updates {value}: {}",
            if value == "off" {
                "UXDuck keeps the interface it serves now"
            } else {
                "UXDuck follows the latest interface"
            }
        );
        return Ok(());
    }
    if name == "interface.channel" {
        let value = value.unwrap_or_default();
        crate::interface::set_channel(data_dir, &value).map_err(CliError::Usage)?;
        // A running UXDuck checks now rather than within the hour.
        let checked = match LocalClient::connect(data_dir).await {
            Ok(mut client) => {
                client.initialize().await?;
                client
                    .call("interface/check", serde_json::json!({}))
                    .await?;
                true
            }
            Err(CliError::NoDaemon(_)) => false,
            Err(error) => return Err(error),
        };
        eprintln!(
            "interface.channel {value}: UXDuck follows {value}'s interface{}",
            if checked {
                "; reload its pages once it is ready"
            } else {
                " from its next start"
            }
        );
        return Ok(());
    }
    if (name == uxduck_engine::OPENAI_KEY || name == uxduck_engine::ANTHROPIC_KEY)
        && !cfg!(debug_assertions)
    {
        return Err(CliError::Usage(
            "this build of UXDuck uses its own hosted model; sign in with `uxduck login`".into(),
        ));
    }
    let value = if let Some(value) = value {
        value
    } else {
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            eprintln!("Paste the value for {name}, then press Enter:");
        }
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line.trim().to_owned()
    };
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    client
        .call(
            "credentials/set",
            serde_json::json!({ "name": name, "value": value }),
        )
        .await?;
    eprintln!(
        "{name} {}",
        if value.is_empty() { "removed" } else { "saved" }
    );
    Ok(())
}

async fn plugin(data_dir: &std::path::Path, command: PluginCommand) -> Result<(), CliError> {
    match command {
        PluginCommand::Install { path } => {
            let path = std::fs::canonicalize(&path)?;
            let mut client = LocalClient::connect(data_dir).await?;
            client.initialize().await?;
            let plugin = client
                .call(
                    "plugin/install",
                    serde_json::json!({ "path": path.display().to_string() }),
                )
                .await?;
            println!(
                "Installed {} {}, which may: {}.",
                plugin["name"].as_str().unwrap_or(""),
                plugin["version"].as_str().unwrap_or(""),
                plugin["capabilities"]
                    .as_array()
                    .map(|c| c
                        .iter()
                        .filter_map(|c| c.as_str())
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_default()
            );
        }
        PluginCommand::List => {
            let mut client = LocalClient::connect(data_dir).await?;
            client.initialize().await?;
            let state = client
                .call(
                    "state/subscribe",
                    serde_json::json!({ "entities": ["plugin"] }),
                )
                .await?;
            let plugins = state["snapshot"]["plugins"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if plugins.is_empty() {
                println!("No plugins are installed.");
            }
            for p in plugins {
                println!(
                    "{}  {} {}  {}{}",
                    p["id"].as_str().unwrap_or(""),
                    p["name"].as_str().unwrap_or(""),
                    p["version"].as_str().unwrap_or(""),
                    p["status"].as_str().unwrap_or(""),
                    p["detail"]
                        .as_str()
                        .map(|d| format!(": {d}"))
                        .unwrap_or_default()
                );
            }
        }
        PluginCommand::Test {
            path,
            tlog,
            command,
            params,
        } => {
            let tlog = tlog.unwrap_or_else(|| path.join("test.tlog"));
            plugin_test(&path, &tlog, command, params)?;
        }
        PluginCommand::New {
            id,
            typescript,
            dir,
        } => {
            if !typescript {
                return Err(CliError::Usage(
                    "add --typescript: new plugins start in TypeScript; for Rust, copy plugins/home-distance"
                        .into(),
                ));
            }
            let dir = dir.unwrap_or_else(|| PathBuf::from(&id));
            new_plugin::write(&id, &dir).map_err(CliError::Usage)?;
            println!(
                "Wrote {}: plugin.json, index.ts and a test. Try it with `uxduck plugin test {}`.",
                dir.display(),
                dir.display()
            );
        }
    }
    Ok(())
}

/// The harness: a plugin against a tlog, as the daemon would run it.
#[cfg(feature = "plugins")]
fn plugin_test(
    path: &std::path::Path,
    tlog: &std::path::Path,
    command: Option<String>,
    params: Option<String>,
) -> Result<(), CliError> {
    let params = match params {
        Some(text) => Some(
            serde_json::from_str(&text).map_err(|e| CliError::Usage(format!("--params: {e}")))?,
        ),
        None => None,
    };
    let bytes = std::fs::read(tlog)?;
    // What it produced, for its own test (`test.json`).
    let mut produced = std::collections::BTreeSet::new();
    let mut failed = None;
    let mut lines = Vec::new();
    let description = crate::daemon::plugins::harness(
        path,
        &bytes,
        command.map(|c| (c, params.unwrap_or_else(|| serde_json::json!({})))),
        &mut |function, input, result| match result {
            Ok(effects) => {
                produced.extend(effects.items.iter().map(|i| i.key.clone()));
                lines.push(
                    serde_json::json!({ "call": function, "input": input, "effects": effects }),
                );
            }
            Err(why) => failed = Some(why.clone()),
        },
    )
    .map_err(CliError::Usage)?;
    // What it describes first, then each call in order.
    println!("{}", serde_json::json!({ "describe": description }));
    for line in lines {
        println!("{line}");
    }
    if let Some(why) = failed {
        return Err(CliError::Usage(why));
    }
    own_test(path, &produced)
}

/// A plugin's own test, `test.json` beside it: the items it must produce.
#[cfg(feature = "plugins")]
fn own_test(
    path: &std::path::Path,
    produced: &std::collections::BTreeSet<String>,
) -> Result<(), CliError> {
    let Ok(text) = std::fs::read_to_string(path.join("test.json")) else {
        return Ok(());
    };
    let expected: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| CliError::Usage(format!("test.json: {e}")))?;
    let missing: Vec<String> = expected["expect"]["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|k| k.as_str())
        .filter(|k| !produced.contains(*k))
        .map(str::to_owned)
        .collect();
    if !missing.is_empty() {
        return Err(CliError::Usage(format!(
            "test.json: it never produced {}",
            missing.join(", ")
        )));
    }
    eprintln!("test.json: passed");
    Ok(())
}

#[cfg(not(feature = "plugins"))]
fn plugin_test(
    _: &std::path::Path,
    _: &std::path::Path,
    _: Option<String>,
    _: Option<String>,
) -> Result<(), CliError> {
    Err(CliError::Usage("this build has no plugin host".into()))
}

/// Asks the daemon to restart, then waits until it answers again.
async fn daemon_action(data_dir: &std::path::Path, action: DaemonAction) -> Result<(), CliError> {
    match action {
        DaemonAction::Restart { yes } => restart_daemon(data_dir, yes).await,
        DaemonAction::Start => match LocalClient::connect(data_dir).await {
            Ok(_) => Ok(()),
            Err(CliError::NoDaemon(_)) => start_daemon(data_dir, false).await.map(drop),
            Err(error) => Err(error),
        },
        DaemonAction::Stop { for_update } => stop_daemon(data_dir, for_update).await,
    }
}

/// Left by `uxduck daemon stop --for-update`: pages were open when the
/// installer stopped UXDuck, and will come back.
const REOPEN_AFTER_UPDATE: &str = "reopen-after-update";

async fn stop_daemon(data_dir: &std::path::Path, for_update: bool) -> Result<(), CliError> {
    let mut client = match LocalClient::connect(data_dir).await {
        Ok(client) => client,
        Err(CliError::NoDaemon(_)) => return Ok(()),
        Err(error) => return Err(error),
    };
    client.initialize().await?;
    if for_update {
        std::fs::write(data_dir.join(REOPEN_AFTER_UPDATE), b"")?;
    }
    client
        .call("daemon/stop", Value::Object(serde_json::Map::new()))
        .await?;
    drop(client);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while LocalClient::connect(data_dir).await.is_ok() {
        if std::time::Instant::now() > deadline {
            return Err(CliError::Usage(
                "UXDuck did not stop within 30 seconds".into(),
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    Ok(())
}

async fn restart_daemon(data_dir: &std::path::Path, yes: bool) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let started = crate::paths::read_info(data_dir).map(|info| info.started_us);
    match client
        .call("daemon/restart", serde_json::json!({ "confirm": yes }))
        .await
    {
        Err(CliError::Api(error)) if error["data"]["kind"] == "confirmation-required" => {
            eprintln!(
                "uxduck: {}\nRun `uxduck daemon restart --yes` to restart anyway.",
                error["message"].as_str().unwrap_or("confirmation required")
            );
            return Err(CliError::Usage("not restarted".into()));
        }
        other => {
            other?;
        }
    }
    drop(client);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    // It goes, then comes back: wait for the new one's `daemon.json`, then
    // for a connection that works.
    loop {
        let info = crate::paths::read_info(data_dir);
        if let Some(info) = info.filter(|info| Some(info.started_us) != started)
            && let Ok(mut client) = LocalClient::connect(data_dir).await
            && client.initialize().await.is_ok()
        {
            println!("Restarted on port {}.", info.port);
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err(CliError::Usage(
                "the daemon did not come back within 30 seconds".into(),
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// `uxduck dev replay`: a recorded tlog's frames, one datagram each, at the
/// pace they were recorded.
/// `uxduck service install` and `uninstall` (S64): Open at login, as the
/// switch in the menu bar and Settings turns it on and off (S108).
fn service(command: &ServiceCommand) -> Result<(), CliError> {
    let said =
        crate::service::set(matches!(command, ServiceCommand::Install)).map_err(CliError::Usage)?;
    println!("{said}");
    Ok(())
}

fn decode_tlog(tlog: &std::path::Path, plugin: Option<&std::path::Path>) -> Result<(), CliError> {
    if let Some(dir) = plugin {
        #[cfg(not(feature = "plugins"))]
        return Err(CliError::Usage(format!(
            "cannot load plugin {}: this build has no plugin support; rebuild with --features plugins",
            dir.display()
        )));
        #[cfg(feature = "plugins")]
        register_tlog_dialect(dir)?;
    }
    decode_tlog_frames(tlog)
}

#[cfg(feature = "plugins")]
fn register_tlog_dialect(dir: &std::path::Path) -> Result<(), CliError> {
    let manifest =
        crate::daemon::plugins::inspect(&dir.display().to_string()).map_err(CliError::Usage)?;
    let dialect = manifest
        .dialect
        .as_ref()
        .map(|file| dir.join(file).display().to_string());
    crate::daemon::plugins::register_dialect(&manifest.id, dialect.as_deref())
        .map_err(CliError::Usage)?;
    Ok(())
}

fn decode_tlog_frames(tlog: &std::path::Path) -> Result<(), CliError> {
    let bytes = std::fs::read(tlog)?;
    for (at, frame) in uxduck_mavlink::tlog::read(&bytes) {
        let Some(header) = uxduck_mavlink::framing::header(frame) else {
            continue;
        };
        let decoded = uxduck_mavlink::framing::payload(frame)
            .and_then(|payload| uxduck_mavlink::named::decode(header.message_id, payload));
        let line = match decoded {
            Some(decoded) => serde_json::json!({
                "tsUs": at, "systemId": header.system_id, "componentId": header.component_id,
                "name": decoded.name, "fields": decoded.fields,
            }),
            None => serde_json::json!({
                "tsUs": at, "systemId": header.system_id, "componentId": header.component_id,
                "id": header.message_id,
            }),
        };
        println!("{line}");
    }
    Ok(())
}

async fn replay(tlog: &std::path::Path, to: &str, speed: f64) -> Result<(), CliError> {
    let target: std::net::SocketAddr = to
        .parse()
        .map_err(|e| CliError::Usage(format!("--to: {e}")))?;
    if !(speed.is_finite() && speed > 0.0) {
        return Err(CliError::Usage("--speed must be above 0".into()));
    }
    let bytes = std::fs::read(tlog)?;
    let records = uxduck_mavlink::tlog::read(&bytes);
    let Some(first) = records.first().map(|(at, _)| *at) else {
        return Err(CliError::Usage(format!(
            "{} holds no frames",
            tlog.display()
        )));
    };
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    eprintln!("replaying {} frames to {target}", records.len());
    let started = tokio::time::Instant::now();
    for (at, frame) in &records {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let offset = std::time::Duration::from_micros(((at - first) as f64 / speed) as u64);
        tokio::time::sleep_until(started + offset).await;
        socket.send_to(frame, target).await?;
    }
    Ok(())
}

async fn video_replay(
    capture: &std::path::Path,
    to: &str,
    rtp: bool,
    rtp_video: bool,
    repeat: bool,
) -> Result<(), CliError> {
    use uxduck_media::replay::{datagrams, first_pat, rtp_video_datagrams};
    let target: std::net::SocketAddr = to
        .parse()
        .map_err(|e| CliError::Usage(format!("--to: {e}")))?;
    let file = std::fs::read(capture)?;
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    let ssrc = u32::from_be_bytes(*b"uxdk");
    let mut sequence = 0u16;
    let mut start = 0;
    let mut sent = 0u64;
    loop {
        let plan = if rtp_video {
            rtp_video_datagrams(&file[start..], ssrc)
        } else {
            datagrams(&file, start, rtp.then_some((ssrc, sequence)))
        };
        if plan.is_empty() {
            return Err(CliError::Usage(format!(
                "{} holds no MPEG-TS packets",
                capture.display()
            )));
        }
        let began = tokio::time::Instant::now();
        for datagram in &plan {
            tokio::time::sleep_until(began + std::time::Duration::from_micros(datagram.at_us))
                .await;
            socket.send_to(&datagram.bytes, target).await?;
            sent += 1;
        }
        #[allow(clippy::cast_possible_truncation, reason = "RTP sequence numbers wrap")]
        {
            sequence = sequence.wrapping_add(plan.len() as u16);
        }
        if !repeat {
            break;
        }
        start = first_pat(&file);
    }
    eprintln!(
        "sent {sent} datagrams to {target}{}",
        if rtp_video {
            " as RTP video"
        } else if rtp {
            " as RTP"
        } else {
            ""
        }
    );
    Ok(())
}

/// What a fake vehicle says it runs.
struct FakeVersion {
    flight_sw_version: u32,
    commit: [u8; 8],
}

impl FakeVersion {
    /// `4.5.8-dev` as `flight_sw_version` bytes: major, minor, patch, and
    /// `FIRMWARE_VERSION_TYPE` (dev 0, alpha 64, beta 128, rc 192, official
    /// 255).
    fn parse(firmware: &str, commit: Option<&str>) -> Result<Self, CliError> {
        let usage = || CliError::Usage(format!("--firmware {firmware}: use 4.5.7 or 4.5.8-dev"));
        let (number, kind) = firmware.split_once('-').unwrap_or((firmware, "official"));
        let parts: Vec<u8> = number
            .split('.')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| usage())?;
        let [major, minor, patch] = parts[..] else {
            return Err(usage());
        };
        let kind: u8 = match kind {
            "dev" => 0,
            "alpha" => 64,
            "beta" => 128,
            "rc" => 192,
            "official" => 255,
            _ => return Err(usage()),
        };
        let mut hash = [0; 8];
        if let Some(commit) = commit {
            if commit.len() < 8 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(CliError::Usage(format!(
                    "--commit {commit}: at least 8 hex characters"
                )));
            }
            hash.copy_from_slice(&commit.as_bytes()[..8]);
        }
        Ok(Self {
            flight_sw_version: u32::from_be_bytes([major, minor, patch, kind]),
            commit: hash,
        })
    }
}

/// How `dev fake-vehicle` behaves beyond its identity.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is a flag of the fake vehicle"
)]
struct FakeOptions {
    no_uid: bool,
    camera: Option<String>,
    refuse_arm: bool,
    refuse_disarm: bool,
    startup_burst: bool,
    prearm_failure: bool,
    repeat_warning: bool,
    vibration_high: bool,
    leak: bool,
    no_ftp: bool,
    drop_ftp: bool,
    terrain: bool,
    terrain_shift: i16,
    battery: f64,
    gps_lost_from_s: Option<f64>,
}

async fn fake_vehicle(
    to: &str,
    system_id: u8,
    uid: &str,
    version: FakeVersion,
    options: FakeOptions,
) -> Result<(), CliError> {
    let FakeOptions {
        no_uid,
        camera,
        refuse_arm,
        refuse_disarm,
        startup_burst,
        prearm_failure,
        repeat_warning,
        vibration_high,
        leak,
        no_ftp,
        drop_ftp,
        terrain,
        terrain_shift,
        battery,
        gps_lost_from_s,
    } = options;
    let target: std::net::SocketAddr = to
        .parse()
        .map_err(|e| CliError::Usage(format!("--to: {e}")))?;
    let uid = u64::from_str_radix(uid.trim_start_matches("0x"), 16)
        .map_err(|e| CliError::Usage(format!("--uid: {e}")))?;
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let mut vehicle = uxduck_mavlink::sim::FakeVehicle::new(system_id, uid);
    vehicle.answers_version = !no_uid;
    vehicle.flight_sw_version = version.flight_sw_version;
    vehicle.commit = version.commit;
    vehicle.camera = camera;
    vehicle.refuses_arm = refuse_arm;
    vehicle.refuses_disarm = refuse_disarm;
    vehicle.startup_burst = startup_burst;
    vehicle.prearm_failure = prearm_failure;
    vehicle.repeat_warning = repeat_warning;
    vehicle.vibration_high = vibration_high;
    vehicle.leak = leak;
    vehicle.ftp = !no_ftp;
    vehicle.files.drop_every = drop_ftp.then_some(7);
    vehicle.terrain = terrain.then(|| {
        let mut own = uxduck_mavlink::fake_terrain::FakeTerrain::default();
        own.shift_m = terrain_shift;
        own
    });
    if terrain {
        // This fixture stores terrain as sent and applies no arming offset.
        vehicle
            .parameters
            .push(("TERRAIN_OFS_MAX".into(), 0.0, uxduck_mavlink::sim::REAL32));
    }
    vehicle.battery_from_pct = battery.clamp(0.0, 100.0);
    vehicle.gps_lost_from_s = gps_lost_from_s;
    eprintln!("fake vehicle {system_id} (uid {uid:x}) sending to {target}; Ctrl-C to stop");
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(1));
    // Telemetry at 5 Hz, one datagram per frame as an autopilot sends them.
    let mut telemetry = tokio::time::interval(std::time::Duration::from_millis(200));
    let started = std::time::Instant::now();
    let mut buffer = vec![0u8; 2048];
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                socket.send_to(&vehicle.heartbeat(), target).await?;
                if let Some(camera) = vehicle.camera_heartbeat() {
                    socket.send_to(&camera, target).await?;
                }
            }
            _ = telemetry.tick() => {
                let t_s = started.elapsed().as_secs_f64();
                for frame in vehicle.telemetry(t_s) {
                    socket.send_to(&frame, target).await?;
                }
                // After its first second, so the daemon knows it by then.
                if t_s > 1.0 {
                    for frame in vehicle.status_texts(t_s) {
                        socket.send_to(&frame, target).await?;
                    }
                }
            }
            received = socket.recv_from(&mut buffer) => {
                let (len, from) = match received {
                    Ok(received) => received,
                    // Windows reports an earlier datagram's port unreachable
                    // here when nothing listens at --to yet; keep sending.
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => continue,
                    Err(error) => return Err(error.into()),
                };
                for reply in vehicle.answer(&buffer[..len]) {
                    socket.send_to(&reply, from).await?;
                }
            }
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
    }
}

async fn run_daemon(
    args: DaemonArgs,
    data_dir: PathBuf,
    dev: bool,
    links: Vec<uxduck_contracts::dto::link::LinkAddParams>,
    headless: bool,
    exit_on_stdin_close: bool,
) -> Result<(), CliError> {
    if args.spin_for_tests {
        spin_for_tests();
    }
    let dev_build = cfg!(debug_assertions);
    let agent = match args.agent_provider.as_str() {
        "uxduck" | "hosted" => crate::daemon::agent::ProviderChoice::Hosted,
        "openai" if dev_build => crate::daemon::agent::ProviderChoice::OpenAi {
            api_base: args.openai_base_url,
            anthropic_base: args.anthropic_base_url,
        },
        "auto" if dev_build => crate::daemon::agent::ProviderChoice::Auto {
            api_base: args.openai_base_url,
            anthropic_base: args.anthropic_base_url,
        },
        "fake" => crate::daemon::agent::ProviderChoice::Fake {
            delay: std::time::Duration::from_millis(40),
        },
        other => {
            return Err(CliError::Usage(format!(
                "unknown agent provider {other}; use {}",
                if dev_build {
                    "uxduck, openai, auto or fake"
                } else {
                    "uxduck or fake"
                }
            )));
        }
    };
    let options = DaemonOptions {
        data_dir,
        exit_on_stdin_close,
        port: args
            .port
            .unwrap_or(if dev { DEV_PORT } else { DEFAULT_PORT }),
        dev_origin: dev.then_some(args.dev_origin),
        hosted_origins: crate::api::listeners::http::HOSTED_ORIGINS
            .iter()
            .map(|origin| (*origin).to_owned())
            .chain(args.hosted_origin)
            .collect(),
        ui_dir: args.ui_dir,
        open: args.open,
        checkpoint_every: args.checkpoint_every.max(1),
        models: args.agent_models,
        agent,
        presence: uxduck_mavlink::session::Config {
            stale_after_us: args.presence_stale_ms * 1_000,
            offline_after_us: args.presence_offline_ms.max(args.presence_stale_ms) * 1_000,
            ..uxduck_mavlink::session::Config::default()
        },
        on_last_client_closed: match args.on_last_client_closed {
            ShutdownChoice::Quit => uxduck_engine::OnLastClientClosed::Quit,
            ShutdownChoice::KeepRunning => uxduck_engine::OnLastClientClosed::KeepRunning,
        },
        headless,
        links,
        update_check: !args.no_update_check,
        analytics: crate::analytics::Config::from_env(
            dev,
            std::time::Duration::from_millis(args.analytics_gather_ms),
        ),
        no_analytics: args.no_analytics,
        controllers: crate::daemon::ControllerInput {
            gamepads: !args.no_gamepads,
            fake: args.fake_controller,
        },
        video_port: args
            .video_port
            .unwrap_or(VideoPort(Some(if dev { DEV_VIDEO_PORT } else { 5600 })))
            .0,
        find: crate::find::Places {
            udp_ports: args.find_udp_ports.unwrap_or_else(|| {
                if dev {
                    DEV_FIND_UDP_PORTS.to_vec()
                } else {
                    vec![14_550, 15_010]
                }
            }),
            tcp_ports: args.find_tcp_ports,
            gateway: !args.find_no_gateway,
        },
    };
    daemon::run(options)
        .await
        .map_err(|error| CliError::Usage(error.to_string()))
}

/// `--video-port`: a port, or `off`.
#[derive(Clone, Copy, Debug)]
struct VideoPort(Option<u16>);

fn video_port(value: &str) -> Result<VideoPort, String> {
    if value.eq_ignore_ascii_case("off") {
        return Ok(VideoPort(None));
    }
    value
        .parse()
        .map(|port| VideoPort(Some(port)))
        .map_err(|_| format!("{value} is not a port number or off"))
}

/// `uxduck ports`: the daemon's own port and every link's, with who holds
/// a port the daemon could not get.
async fn ports_list(data_dir: &std::path::Path) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    let init = client.initialize().await?;
    let state = client
        .call(
            "state/subscribe",
            serde_json::json!({ "entities": ["link"] }),
        )
        .await?;
    let info = crate::paths::read_info(data_dir);
    println!("PORT   PROTO  USE                                  STATUS");
    if let Some(info) = info {
        println!(
            "{:<6} {:<6} {:<36} listening (uxduck {})",
            info.port,
            "TCP",
            "the app and API (127.0.0.1)",
            init["daemonVersion"].as_str().unwrap_or("")
        );
    }
    for link in state["snapshot"]["links"].as_array().into_iter().flatten() {
        let kind = &link["kind"];
        let (port, proto) = match kind["type"].as_str() {
            Some("udp-listen") => (kind["port"].to_string(), "UDP"),
            Some("udp-forward") => (format!("→{}", kind["port"]), "UDP"),
            Some("tcp-client") => (format!("→{}", kind["port"]), "TCP"),
            _ => ("-".into(), "serial"),
        };
        let status = link["status"].as_str().unwrap_or("");
        let detail = link["statusDetail"]
            .as_str()
            .map(|d| format!(": {d}"))
            .unwrap_or_default();
        println!(
            "{:<6} {:<6} {:<36} {status}{detail}",
            port,
            proto,
            link["name"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

/// `uxduck ports free <port>`: names the holder and, if it is this user's
/// own program, stops it after confirmation (ADR-0004 port ownership).
fn ports_free(port: u16, tcp: bool, yes: bool) {
    use crate::ports::{Free, Protocol, free, may_stop, owner};
    let protocol = if tcp { Protocol::Tcp } else { Protocol::Udp };
    let Some(holder) = owner(protocol, port) else {
        println!("No program UXDuck can see is using {protocol} port {port}.");
        return;
    };
    println!("{protocol} port {port} is held by {holder}.");
    if !may_stop(&holder) {
        println!("It belongs to the system or another user, so UXDuck will not stop it.");
        return;
    }
    if !yes {
        eprint!("Stop {}? [y/N] ", holder.name);
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err()
            || !matches!(answer.trim(), "y" | "Y" | "yes")
        {
            println!("Left running.");
            return;
        }
    }
    match free(holder) {
        Free::Stopped(stopped) => println!("Stopped {stopped}."),
        Free::Failed(holder, error) => println!("Could not stop {holder}: {error}"),
        Free::NotYours(_) => {}
    }
}

/// `uxduck approvals`: the pending list, or an answer to one.
async fn approvals(
    data_dir: &std::path::Path,
    command: Option<ApprovalsCommand>,
) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match command {
        None => {
            let list = client.call("approval/list", serde_json::json!({})).await?;
            let approvals = list["approvals"].as_array().cloned().unwrap_or_default();
            if approvals.is_empty() {
                println!("No approvals are waiting.");
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_micros());
            for approval in approvals {
                let left = u128::from(approval["deadlineUs"].as_u64().unwrap_or(0))
                    .saturating_sub(now)
                    / 1_000_000;
                println!(
                    "{}  {} wants to {}  ({} s left)",
                    approval["id"].as_str().unwrap_or(""),
                    approval["clientName"].as_str().unwrap_or(""),
                    approval["summary"].as_str().unwrap_or(""),
                    left
                );
            }
            println!("Answer with `uxduck approvals allow <id>` or `uxduck approvals deny <id>`.");
        }
        Some(ApprovalsCommand::Allow { id, remember }) => {
            let remember = match remember {
                RememberChoice::Once => "once",
                RememberChoice::Connection => "connection",
                RememberChoice::Always => "always",
            };
            client
                .call(
                    "approval/resolve",
                    serde_json::json!({ "id": id, "allow": true, "remember": remember }),
                )
                .await?;
            println!("Allowed ({remember}).");
        }
        Some(ApprovalsCommand::Deny { id }) => {
            client
                .call(
                    "approval/resolve",
                    serde_json::json!({ "id": id, "allow": false }),
                )
                .await?;
            println!("Denied.");
        }
    }
    Ok(())
}

/// A deliberate busy loop on its own thread, counted as a task, so the
/// resource budget and `uxduck wakeups` can be seen to catch one (S24).
fn spin_for_tests() {
    tracing::warn!("--spin-for-tests: burning a core on purpose");
    let _ = std::thread::Builder::new()
        .name("spin-for-tests".into())
        .spawn(|| {
            loop {
                for _ in 0..1_000_000 {
                    std::hint::spin_loop();
                }
                crate::wakes::count("spin-for-tests (a deliberate busy loop)");
            }
        });
}
