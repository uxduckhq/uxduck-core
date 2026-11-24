//! The command line (ADR-0001, Deployable shape).

pub mod checklist;
pub mod client;
mod companion;
mod fake_board;
pub mod find;
pub mod firmware;
pub mod link_spec;
pub mod marker;
pub mod mavlink;
pub mod mission;
mod new_plugin;
pub mod operation;
pub mod params;
pub mod plan;
pub mod practice;
mod practice_replay;
pub mod preflight;
pub mod replay;
#[cfg(test)]
mod replay_wire_tests;
pub mod setup;
mod sim_build;
mod sim_own;
mod ssh;
pub mod terrain;
mod traffic;
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
    /// A computer on the vehicle that carries no vehicle traffic, such as a
    /// second Raspberry Pi: once a second it sends its temperature, CPU,
    /// memory and storage to the ground station as an onboard computer.
    /// With --config it also sends its cameras over WebRTC and records them.
    /// `uxduck companion --to udp:192.168.1.20:15010 --computer-id 192`
    Companion(companion::CompanionArgs),
    /// A terminal on a vehicle's companion computer, through UXDuck on this
    /// computer, which signs in with the host key and the key a check uses.
    /// `uxduck ssh buggy camera` gives its prompt. The computer is left
    /// out when the vehicle has one. After `--`, one command runs with no
    /// terminal and this exits with its exit status.
    /// `uxduck ssh buggy camera -- journalctl -u mediamtx -n 50`
    Ssh(ssh::SshArgs),
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
        /// Local operator ID; otherwise use a profile named after your email.
        #[arg(long, value_name = "ID")]
        operator: Option<String>,
        /// Join the product updates list.
        #[arg(long)]
        product_updates: bool,
    },
    /// Sign out; everything on this computer keeps working.
    Logout,
    /// Read and agree to UXDuck's Terms of Service, which UXDuck asks of an
    /// operator before it arms a real vehicle for them.
    Terms {
        /// Local operator ID; needed when this computer has several.
        #[arg(long, value_name = "ID")]
        operator: Option<String>,
        /// Agree without the prompt, having read the terms.
        #[arg(long)]
        agree: bool,
    },
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
    /// Practice exercises on a simulated vehicle: `uxduck practice
    /// list`, `start <exercise> --vehicle <name>`, `status`, `stop` and `guide`.
    Practice {
        #[command(subcommand)]
        command: practice::PracticeCommand,
    },
    /// A vehicle's missions over time: `uxduck mission history Alpha`.
    Mission {
        #[command(subcommand)]
        command: mission::MissionCommand,
    },
    /// Markers on the map: `uxduck marker add --lat 53.07 --lon -4.08 --look flag`.
    Marker {
        #[command(subcommand)]
        command: marker::MarkerCommand,
    },
    /// A vehicle's setup: `uxduck setup status Alpha`, and `uxduck
    /// setup compass Alpha` to calibrate its compasses while it is turned.
    Setup {
        #[command(subcommand)]
        command: setup::SetupCommand,
    },
    /// A vehicle's preflight checks: `uxduck preflight Alpha`, and
    /// `uxduck preflight Alpha --check plan --severity block` to change one
    /// of UXDuck's own.
    Preflight(preflight::PreflightArgs),
    /// Plays a flight back in UXDuck: `uxduck replay` for the
    /// replay open now or the last flight, `uxduck replay <flight id>`,
    /// `uxduck replay flight.tlog`,
    /// with `--play`, `--speed 5` and `--at 90`; `--close` shows live again.
    Replay(replay::ReplayArgs),
    /// Operations: `uxduck operation list`, `rename <id>
    /// "Night survey"`, `add <id> <flight id>…`, `create --name … <flight
    /// id>…`, `remove <id> <flight id>…`.
    Operation {
        #[command(subcommand)]
        command: operation::OperationCommand,
    },
    /// A vehicle's own checklist: `uxduck checklist Alpha`, `add
    /// "Propellers tight"`, `add "Battery" --rule "battery.voltage_v >=
    /// 22.2" --hold 3`, `tick 1`, `export`, `import list.md`, `copy
    /// --from Bravo`. A preflight is a run of it: `new`,
    /// `finish`, `reopen`, `runs`.
    Checklist(checklist::ChecklistArgs),
    /// Saved plans: `uxduck plan import mission.plan`.
    Plan {
        #[command(subcommand)]
        command: plan::PlanCommand,
    },
    /// Terrain heights: `uxduck terrain at --lat 53.07 --lon -4.08`.
    Terrain {
        #[command(subcommand)]
        command: terrain::TerrainCommand,
    },
    /// Other aircraft near the vehicles: `uxduck traffic` lists
    /// them, `uxduck traffic sources` lists who reports them, and `uxduck
    /// traffic on Alpha` uses Alpha's ADS-B receiver.
    Traffic {
        #[command(subcommand)]
        command: Option<traffic::TrafficCommand>,
        /// The aircraft as JSON, for scripts.
        #[arg(long)]
        json: bool,
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
    /// MAVLink: `uxduck mavlink dialects` lists, adds, binds or removes
    /// the dialects of a fork's own messages.
    Mavlink {
        #[command(subcommand)]
        command: mavlink::MavlinkCommand,
    },
    /// A fork's firmware profiles: `uxduck firmware add <url>` reads the one
    /// file its CI publishes naming its parameter descriptions and dialects.
    Firmware {
        #[command(subcommand)]
        command: firmware::FirmwareCommand,
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
        /// Launch past UXDuck's failing checks that hold the launch
        /// (`uxduck preflight`), naming them: `uxduck command takeoff
        /// --anyway`. Asks first unless `--yes`. Arming is never held by
        /// checks and needs none of this.
        #[arg(long, global = true, alias = "arm-anyway")]
        anyway: bool,
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
    /// Loopback port: reuse this profile's last port, otherwise try 45000
    /// normally or 45100 with --dev, then a free port if occupied.
    /// An explicit port overrides the saved choice and must be free; 0 picks a new one.
    #[arg(long, env = "UXDUCK_PORT")]
    port: Option<u16>,
    /// The dev server's origin, allowed and used as the UI address with `--dev`.
    #[arg(long, default_value = DEV_ORIGIN)]
    dev_origin: String,
    /// Another site that may pair and connect with a token, as UXDuck on the
    /// web does; for tests. Repeatable.
    #[arg(long, value_name = "ORIGIN", hide = true)]
    hosted_origin: Vec<String>,
    /// Serve a built UXDuck interface from this directory.
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
    /// Models offered in the selector with your own keys or the fake,
    /// comma-separated; the first is the default. The hosted model offers
    /// what the account server says.
    #[arg(
        long,
        env = "UXDUCK_AGENT_MODELS",
        value_delimiter = ',',
        default_value = uxduck_agent::models::DEFAULT_MODELS
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
    /// fail. For tests only.
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
    /// plugin gets the capabilities its manifest asks for. A plugin whose
    /// checks ask to hold the launch only warns until you allow it, here with
    /// `--checks-block` or later with `uxduck plugin checks`.
    Install {
        path: PathBuf,
        /// Let its blocking checks hold the launch while they don't pass.
        #[arg(long, conflicts_with = "checks_warn")]
        checks_block: bool,
        /// Keep its checks as warnings only, and stop asking.
        #[arg(long)]
        checks_warn: bool,
    },
    /// Whether a plugin's blocking checks may hold the launch (`block`) or
    /// only warn (`warn`): your answer, kept on this computer.
    Checks {
        id: String,
        #[arg(value_enum)]
        may: ChecksMay,
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

/// What a plugin's blocking checks may do.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum ChecksMay {
    /// Hold the launch while they don't pass.
    Block,
    /// Only warn.
    Warn,
}

#[derive(Debug, Subcommand)]
pub enum DaemonAction {
    /// Restart the running daemon: a final checkpoint, then the same
    /// program in its place. Clients reconnect on their own.
    Restart {
        /// Confirm interruption of connected vehicles and active work.
        #[arg(long)]
        yes: bool,
    },
    /// Start UXDuck in the background when it isn't running, and open
    /// nothing: what Open at login runs.
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
    /// Where vehicles are heard: `udp:HOST:PORT` or `udpin:HOST:PORT`
    /// (listens), `udpout:HOST:PORT` (sends to a UDP server),
    /// `serial:DEVICE[@BAUD]` or `DEVICE[,BAUD]` such as `/dev/ttyUSB0,57600`,
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
    /// This computer is on the vehicle: send its temperature, CPU, memory
    /// and storage out of every `--out` once a second, as an onboard
    /// computer (MAVLink's `ONBOARD_COMPUTER_STATUS`).
    #[arg(long)]
    onboard: bool,
    /// The onboard computer id to send as, 191 to 194; the first free one
    /// when left out. Pin it when several computers cannot hear each other.
    #[arg(long, requires = "onboard", value_parser = clap::value_parser!(u8).range(191..=194))]
    computer_id: Option<u8>,
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
        /// Include newer numbered beta releases alongside stable releases.
        #[arg(long)]
        beta: bool,
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
        /// The operator's name, shown on the page.
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
    /// Watch links: anyone with one watches this computer, or one
    /// vehicle on it, in a browser, read only, until it ends.
    WatchLink {
        #[command(subcommand)]
        action: WatchLinkAction,
    },
    /// The flights saved to your account, from any of your computers.
    #[command(name = "logbook", visible_alias = "flights")]
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
        /// The flight, as `uxduck account logbook` lists it.
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
    /// "Live sharing from this computer": `on`, as it starts, or `off`.
    /// Off, nothing live leaves the computer and its watch links end.
    LiveSharing {
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
        /// or PX4's `quadx` (see `uxduck sim builds`). An ArduPilot
        /// multicopter, plane or VTOL frame can fly a JSON physics model of
        /// your own after a colon: `X:/full/path/quad.json`.
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
        /// Allow this build to listen on the network if it cannot keep
        /// connections local. Other computers may connect and control it.
        #[arg(long, requires = "binary")]
        allow_network: bool,
        /// Forget its stored parameters each time it starts.
        #[arg(long)]
        wipe: bool,
        /// Carries a simulated camera, MAVLink component 100 (ArduPilot
        /// and PX4).
        #[arg(long)]
        camera: bool,
        /// Carries a simulated gimbal, MAVLink component 154, which the
        /// autopilot manages (ArduPilot and PX4).
        #[arg(long)]
        gimbal: bool,
        /// Carries a gimbal that ArduPilot simulates itself, with its
        /// camera, in place of `--camera` and `--gimbal`: `viewpro`, which
        /// only the autopilot speaks for, or `avt-cm62`, a MAVLink gimbal
        /// behind the autopilot. It sends no video.
        #[arg(
            long,
            value_name = "MODEL",
            value_parser = ["viewpro", "avt-cm62"],
            conflicts_with_all = ["camera", "gimbal"]
        )]
        ardupilot_gimbal: Option<String>,
        /// What kind the simulated camera and gimbal are, in place of a
        /// camera that takes photos, records, zooms and focuses with a
        /// gimbal apart from it: `stream-only` for a camera that only
        /// sends video, `photo-video` for one that takes photos in one
        /// mode and records in the other, `integrated` for a camera and
        /// gimbal that are one component, or `thermal-tracking` for such
        /// a unit with a thermal picture and tracking. The last two need
        /// `--gimbal` too.
        #[arg(
            long,
            value_name = "KIND",
            value_parser = ["stream-only", "photo-video", "integrated", "thermal-tracking"],
            requires = "camera"
        )]
        payload_profile: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Stops a simulated vehicle, by its link's id. It is kept, stopped,
    /// until it is started again or its link is removed.
    Stop { id: String },
    /// Switches a fault on or off in a simulated vehicle's camera or
    /// gimbal, by its link's id, to see what UXDuck does with it. It lasts
    /// until it is switched off or the simulation stops.
    Fault {
        id: String,
        /// `camera-silent`, `gimbal-silent`, `card-full`, `gimbal-error`
        /// or `autopilot-holds`, or with `--command` `command-refused` or
        /// `command-in-progress`.
        #[arg(value_parser = [
            "camera-silent",
            "gimbal-silent",
            "card-full",
            "command-refused",
            "command-in-progress",
            "gimbal-error",
            "autopilot-holds",
        ])]
        fault: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
        /// The `MAV_CMD` number a refused or slow command fault is about,
        /// such as 2000 to take a photo. `uxduck sim faults` lists them.
        #[arg(long, value_name = "MAV_CMD")]
        command: Option<u16>,
    },
    /// The faults a simulated vehicle's camera and gimbal can have, by its
    /// link's id: which are on, which it cannot have and why, and the
    /// commands one can name.
    Faults { id: String },
    /// Starts a simulated vehicle again from where it started, by its
    /// link's id: after a crash, or to begin afresh. It keeps its name.
    Restart { id: String },
    /// Saves a snapshot of a simulated vehicle, by its id.
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
    /// The builds `uxduck sim start` chooses from, with their frames: the
    /// catalogue's downloads and any in UXDuck's `simulators/bin` folder.
    ///
    /// Builds of your own from `uxduck sim build` are not listed. Start one
    /// with `uxduck sim start --binary <path>`.
    Builds,
    /// Builds the commit an ArduPilot checkout has checked out as a
    /// simulator UXDuck can start, with UXDuck's local-only network patch,
    /// in a worktree of its own (macOS and Linux). Prints the program's
    /// path, for `uxduck sim start --binary` or New simulation's own build.
    Build {
        checkout: PathBuf,
        /// `multicopter`, `plane`, `rover`, `submarine` or `airship`.
        #[arg(long, default_value = "multicopter")]
        vehicle: String,
        /// When no version of the patch fits the release, build it without
        /// one: while it runs, other computers on your network can reach it.
        #[arg(long)]
        allow_network: bool,
    },
    /// The catalogue's prebuilt builds for this computer.
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
#[expect(
    clippy::large_enum_variant,
    reason = "a command line is parsed once, and each flag of the fake vehicle is a field of its variant"
)]
pub enum DevCommand {
    /// A fake vehicle on a serial port of its own (a pseudo-terminal whose
    /// path it prints) that restarts into a fake bootloader when asked and
    /// back into its firmware once installed, reporting the version
    /// written. Add a serial link to the path, then install.
    FakeBoard {
        #[arg(long, default_value_t = 1)]
        system_id: u8,
        /// The autopilot UID, in hex.
        #[arg(long, default_value = "b0a2d140")]
        uid: String,
        /// The firmware version it reports before an install.
        #[arg(long, default_value = "4.6.2")]
        firmware: String,
        /// Its board id, as firmware files name boards (140: Cube Orange).
        #[arg(long, default_value_t = 140)]
        board_id: u32,
        /// Its flash for firmware, KB.
        #[arg(long, default_value_t = 2016)]
        flash_kb: u32,
        /// Starts in its bootloader, as a board with no firmware.
        #[arg(long)]
        bootloader: bool,
        /// Writes a fake .apj for this board first, to install from a file.
        #[arg(long)]
        write_apj: Option<PathBuf>,
        /// The version in the fake .apj.
        #[arg(long, default_value = "4.7.2")]
        apj_version: String,
        /// The fake .apj's image, KB.
        #[arg(long, default_value_t = 64)]
        apj_kb: u32,
        /// Milliseconds per chunk written (and a hundred times it to
        /// erase), so each stage can be seen.
        #[arg(long, default_value_t = 0)]
        pace_ms: u64,
    },
    /// A gimbal device alone on component 154, with no autopilot and no
    /// manager of its own. Whoever speaks as component 1 of its
    /// system is its autopilot and its manager, and it reports its yaw from
    /// north. MAVSDK's gimbal device tester expects exactly this, with
    /// `--system-id 33`.
    FakeGimbal {
        /// Where to send, such as the tester's `udpin` address.
        #[arg(long, default_value = "127.0.0.1:14550")]
        to: String,
        #[arg(long, default_value_t = 1)]
        system_id: u8,
    },
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
        /// A camera on component 100. With an address, such as
        /// `rtsp://127.0.0.1:8554/main` or `udp://127.0.0.1:5600`, it names
        /// that video stream when asked.
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        camera: Option<String>,
        /// Its camera only sends video: it names its stream and
        /// says it takes no images, records nothing and has no settings.
        #[arg(long, requires = "camera")]
        camera_video_only: bool,
        /// Its camera loses power this many seconds in: no
        /// heartbeats and no answers from then on.
        #[arg(long, requires = "camera")]
        camera_silent_at: Option<f64>,
        /// Its camera has power again this many seconds in.
        #[arg(long, requires = "camera_silent_at")]
        camera_back_at: Option<f64>,
        /// Its camera's card has only this many mebibytes left:
        /// with 0 the card is full, and the camera refuses photos and
        /// recordings until the card is formatted.
        #[arg(long, requires = "camera")]
        camera_card_left: Option<f32>,
        /// What kind its camera and gimbal are: `stream-only` for a camera
        /// that only sends video, `photo-video` for one that takes photos
        /// in one mode and records in the other with no zoom or focus,
        /// `integrated` for a camera and gimbal that are one component,
        /// the camera's, or `thermal-tracking` for such a unit with a
        /// thermal picture and tracking. The last two need `--gimbal` too.
        #[arg(
            long,
            value_name = "KIND",
            value_parser = ["stream-only", "photo-video", "integrated", "thermal-tracking"],
            requires = "camera"
        )]
        payload_profile: Option<String>,
        /// A fault its camera or gimbal has from the start, and may be
        /// given more than once: `camera-silent`, `gimbal-silent`,
        /// `card-full`, `gimbal-error`, `autopilot-holds`, or with a
        /// `MAV_CMD` number `command-refused=2000` or
        /// `command-in-progress=2000`.
        #[arg(long, value_name = "FAULT")]
        payload_fault: Vec<String>,
        /// Its camera names this address for its definition file in place
        /// of its own, or `none` for a camera that names no file.
        #[arg(long, requires = "camera")]
        camera_definition: Option<String>,
        /// Its camera serves this file over MAVLink FTP where its own
        /// definition file would be.
        #[arg(long, requires = "camera")]
        camera_definition_file: Option<PathBuf>,
        /// Its camera lists this many settings beyond the ones its
        /// definition file names, each named `X` and its place in the
        /// list: for a camera whose list is longer than UXDuck keeps.
        #[arg(long, value_name = "COUNT", requires = "camera")]
        camera_more_settings: Option<u16>,
        /// When its camera describes a photo it took: `when-asked` for
        /// one that only counts a photo until it is asked for it, or
        /// `never` for one that answers no such request either.
        #[arg(
            long,
            value_name = "WHEN",
            value_parser = ["when-asked", "never"],
            requires = "camera"
        )]
        camera_photos_said: Option<String>,
        /// A gimbal on component 154 that manages itself.
        #[arg(long)]
        gimbal: bool,
        /// Its gimbal starts in another's control: `autopilot` for the
        /// vehicle's own, or a MAVLink system and component such as
        /// `254/190` for another ground station.
        #[arg(long, requires = "gimbal")]
        gimbal_held_by: Option<String>,
        /// Its autopilot drives a camera of its own, as ArduPilot does with
        /// `CAM1_TYPE 1`: asked what cameras it has, it names one
        /// that takes images, as ArduPilot 4.7 does.
        #[arg(long)]
        autopilot_camera: bool,
        /// Send ArduPilot's start-up messages, two of them warnings.
        #[arg(long)]
        startup_burst: bool,
        /// Say `PreArm: Throttle below failsafe` every 30 s while disarmed.
        #[arg(long)]
        prearm_failure: bool,
        /// Repeat the warning "Vibration high" every second, from 12 s in.
        #[arg(long)]
        repeat_warning: bool,
        /// Shake more and more from 10 s in, past 30 and 60 m/s².
        #[arg(long)]
        vibration_high: bool,
        /// Its ADS-B receiver hears this many aircraft circling home, sent as
        /// `ADSB_VEHICLE` once a second.
        #[arg(long, default_value_t = 0)]
        traffic: u8,
        /// Say "Leak Detected!" every 10 s, as `ArduSub` does.
        #[arg(long)]
        leak: bool,
        /// Send a fork's own messages, as `crates/uxduck-mavlink/fixtures/acme_fork.xml`
        /// describes them: `ACME_STATUS` (43001) and a `SYS_STATUS` with one more
        /// extension.
        #[arg(long)]
        fork_messages: bool,
        /// No MAVLink FTP: parameters come by the list, without defaults
        ///, and the Files tab has nothing to show.
        #[arg(long)]
        no_ftp: bool,
        /// Lose every seventh MAVLink FTP answer, as a lossy radio does.
        #[arg(long)]
        drop_ftp: bool,
        /// Start with no terrain and ask UXDuck for it, as ArduPilot with an
        /// empty terrain folder; without it, terrain is fixed.
        #[arg(long)]
        terrain: bool,
        /// With `--terrain`, keep every height this many metres off, as a
        /// card holding another dataset.
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        terrain_shift: i16,
        /// Fail the pre-arm checks: arming is refused unless forced.
        #[arg(long)]
        refuse_arm: bool,
        /// Refuse to disarm unless forced, as a vehicle that thinks it is
        /// still flying does.
        #[arg(long)]
        refuse_disarm: bool,
        /// The battery's % at the start, such as 45 to see it low.
        #[arg(long, default_value_t = 100.0)]
        battery: f64,
        /// Never have a GPS fix, so never a position.
        #[arg(long, conflicts_with = "lose_gps_at")]
        no_gps: bool,
        /// Lose the GPS fix, and with it the position, this many seconds in.
        #[arg(long)]
        lose_gps_at: Option<f64>,
        /// With `--no-gps` or `--lose-gps-at`, report the position DCM
        /// reckons while it has no fix, drifting from where it last had one
        /// or from 0, 0, as `ArduPlane` and Rover do.
        #[arg(long)]
        dead_reckon: bool,
        /// List this many compasses (1 to 3), not calibrated: its pre-arm
        /// checks fail with "Compass not calibrated" until they are, and it
        /// answers a compass calibration as ArduPilot does.
        #[arg(long, default_value_t = 0)]
        compasses: u8,
        /// With `--compasses`, start calibrated.
        #[arg(long, requires = "compasses")]
        compasses_calibrated: bool,
        /// With `--compasses`, fail the last compass's calibration with this
        /// `MAG_CAL_STATUS`, such as 6 for a bad orientation.
        #[arg(long, requires = "compasses")]
        compass_fails: Option<u8>,
        /// List its accelerometers' offsets, all zero: its pre-arm checks
        /// fail with "3D Accel calibration needed" until it is calibrated
        /// in six positions, which it asks for as ArduPilot does.
        #[arg(long)]
        accel_uncalibrated: bool,
        /// Fail the accelerometer calibration after its last position.
        #[arg(long)]
        accel_fails: bool,
        /// List its radio's parameters at ArduPilot's defaults: its pre-arm
        /// checks fail with "RC not calibrated" until it is calibrated. Asked
        /// for its channels, its sticks move as a person calibrating it would
        ///: centred for 8 s, swept to their ends until 16 s, centred
        /// until 24 s, then roll right, pitch forward, throttle up and yaw
        /// right in turn.
        #[arg(long)]
        radio: bool,
        /// With `--radio`, rolling right lowers channel 1, so a calibration
        /// offers to reverse it.
        #[arg(long, requires = "radio")]
        radio_reversed_roll: bool,
        /// With `--radio`, how fast its sticks move compared with the times
        /// above: 0.25 gives four times as long for each step.
        #[arg(long, requires = "radio", default_value_t = 1.0)]
        radio_pace: f64,
        /// List a quad X's frame class and type, so the Setup tab
        /// draws its motors. Every fake vehicle answers a motor test,
        /// listed or not: armed for it, the motor tested shows in its
        /// outputs until the test's time runs out.
        #[arg(long)]
        motors: bool,
        /// List a copter's ESC parameters with this `MOT_PWM_TYPE` (0 PWM,
        /// 6 `DShot600`) and `ESC_CALIBRATION` 0. Set to 3, it
        /// restarts by itself eight seconds later with it back at 0, as a
        /// person replugging its battery would have it.
        #[arg(long)]
        escs: Option<u8>,
        /// List `SCR_ENABLE` with this value (0 off, 1 Lua) and a
        /// `hello.lua` in `/APM/scripts`. It answers
        /// `MAV_CMD_SCRIPTING`'s restart and says "Scripting: restarted".
        #[arg(long)]
        scripting: Option<u8>,
        /// Be a plane with an airspeed sensor: it sits on the ground
        /// until armed and circles once armed; it zeroes its sensor as
        /// `ArduPlane` does, and with `ARSPD_AUTOCAL` on in flight its ratio
        /// settles. Raise `SIM_ARSPD_OFS` by 60 to blow into its pitot, or
        /// set `SIM_ARSPD_FAIL` to stick the reading, as in SITL.
        #[arg(long)]
        airspeed: bool,
        /// With `--airspeed`, its sensor not zeroed: it reads 4 m/s at rest
        /// until it is.
        #[arg(long, requires = "airspeed")]
        airspeed_unzeroed: bool,
        /// Be PX4 1.17: a quad X with PX4's parameters, calibrating
        /// as PX4 does and saying how in its `[cal]` status text, turned
        /// onto each side by itself; it answers `MAV_CMD_ACTUATOR_TEST`.
        /// With `--accel-uncalibrated` its accelerometer is not calibrated
        /// (`CAL_ACC0_ID` 0), with `--accel-fails` its calibration times
        /// out after two sides.
        #[arg(long, conflicts_with_all = ["airspeed", "compasses", "radio", "motors"])]
        px4: bool,
        /// With `--px4`, a plane with an airspeed sensor, which PX4 zeroes
        /// and then asks to be blown into.
        #[arg(long, requires = "px4")]
        px4_plane: bool,
    },
    /// Runs a SITL build and stops it when standard input closes: the
    /// simulation manager's guard, so no build outlives the daemon.
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
    /// the vehicle's side of the moment. Frames ground stations sent
    /// (UXDuck's own, which its tlogs keep, ADR-0050 L1) are left out.
    Replay {
        tlog: PathBuf,
        /// Where to send, such as the daemon's UDP link.
        #[arg(long, default_value = "127.0.0.1:15010")]
        to: String,
        /// Faster (2) or slower (0.5) than recorded.
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
        /// Sends nothing: judges a practice exercise over the tlog, such as
        /// `bring-it-home`, and prints its steps and result.
        #[arg(long)]
        practice: Option<String>,
        /// With `--practice`, seconds into the tlog the attempt began.
        #[arg(long, default_value_t = 0.0, requires = "practice")]
        from: f64,
    },
    /// Prints a tlog's MAVLink messages by name, one JSON line each, as
    /// plugins receive them; `--plugin` adds that plugin's vendor dialect
    ///. Frames no dialect knows are printed by id.
    Decode {
        tlog: PathBuf,
        /// A plugin folder whose `plugin.json` names a dialect.
        #[arg(long)]
        plugin: Option<PathBuf>,
    },
    /// Sends an MPEG-TS capture over UDP as a camera would, byte for byte
    /// and paced by its PCR: seven packets to a datagram.
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
    /// Writes the made-up ArduPilot log the tests read (a copter armed for
    /// 40 s on GPS time, system id 7) to a file, for end-to-end tests of
    /// autopilot logs.
    DataflashFixture { path: PathBuf },
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
    /// `MAV_CMD_NAV_TAKEOFF` to an altitude in metres. A disarmed copter or
    /// VTOL is switched to GUIDED and armed first.
    Takeoff {
        #[arg(long, default_value_t = 10.0)]
        altitude: f32,
    },
    /// `MAV_CMD_MISSION_START`: flies the uploaded mission.
    MissionStart,
    /// Goes to a place and holds there, at a height above home, sea
    /// level or the terrain (`DO_REPOSITION` in GUIDED).
    GoTo {
        // West and south are negative: `--lon -121.88` is a value, not a flag.
        #[arg(long, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        lon: f64,
        /// Metres, in `--frame`; negative is below it (a submarine's dive).
        #[arg(long, default_value_t = 30.0, allow_hyphen_values = true)]
        altitude: f64,
        /// What the height is measured from: `above-home`, `amsl` or
        /// `above-ground`.
        #[arg(long, default_value = "above-home")]
        frame: String,
    },
    /// Moves home (`DO_SET_HOME`): to `--lat` and `--lon`, on the ground
    /// there unless `--altitude` says otherwise; without them, to where the
    /// vehicle is now.
    SetHome {
        #[arg(long, allow_hyphen_values = true, requires = "lon")]
        lat: Option<f64>,
        #[arg(long, allow_hyphen_values = true, requires = "lat")]
        lon: Option<f64>,
        /// Metres above mean sea level; the ground's height there, from
        /// UXDuck's terrain, when left out.
        #[arg(long, allow_hyphen_values = true, requires = "lat")]
        altitude: Option<f64>,
    },
    /// Sets the EKF origin, for a vehicle without GPS
    /// (`DO_SET_GLOBAL_ORIGIN`). ArduPilot takes it only while it has none.
    SetEkfOrigin {
        #[arg(long, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        lon: f64,
        /// Metres above mean sea level; the ground's height there when left out.
        #[arg(long, allow_hyphen_values = true)]
        altitude: Option<f64>,
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
            run_daemon(args, data_dir, cli.dev, (Vec::new(), None), false, owned).await
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
            let onboard = args.onboard.then_some(crate::daemon::onboard::Onboard {
                component_id: args.computer_id,
            });
            run_daemon(
                args.daemon,
                data_dir,
                cli.dev,
                (links, onboard),
                true,
                owned,
            )
            .await
        }
        Some(Command::Update {
            command:
                UpdateCommand::Check {
                    url,
                    public_key,
                    beta,
                },
        }) => {
            let key = public_key
                .as_deref()
                .or(crate::update::APP_UPDATE_PUBLIC_KEY);
            return match crate::update::check_channels(&url, beta, key, &data_dir).await {
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
            anyway,
        }) => vehicle_command(&data_dir, action, vehicle, confirm, (yes, anyway)).await,
        Some(Command::Preflight(args)) => preflight::run(&data_dir, args).await,
        Some(Command::Replay(args)) => replay::run(&data_dir, args).await,
        Some(Command::Companion(args)) => companion::run(args).await,
        // Exits with the remote exit status.
        Some(Command::Ssh(args)) => return ssh::run(&data_dir, args).await,
        Some(Command::Operation { command }) => operation::run(&data_dir, command).await,
        Some(Command::Checklist(args)) => checklist::run(&data_dir, args).await,
        Some(Command::Trace) => trace(&data_dir).await,
        Some(Command::Debug {
            command: DebugCommand::Panic,
        }) => rpc_call(&data_dir, "debug/panic", None).await,
        Some(Command::Wakeups { seconds }) => wakeups(&data_dir, seconds).await,
        Some(Command::Dev { command }) => dev(command).await,
        Some(Command::Sim { command }) => sim(&data_dir, command).await,
        Some(Command::Params { command }) => params::run(&data_dir, command).await,
        Some(Command::Mavlink { command }) => mavlink::run(&data_dir, command).await,
        Some(Command::Firmware { command }) => firmware::run(&data_dir, command).await,
        Some(Command::Vehicle { command }) => vehicle::run(&data_dir, command).await,
        Some(Command::Practice { command }) => practice::run(&data_dir, command).await,
        Some(Command::Plan { command }) => plan::run(&data_dir, command).await,
        Some(Command::Marker { command }) => marker::run(&data_dir, command).await,
        Some(Command::Setup { command }) => setup::run(&data_dir, command).await,
        Some(Command::Mission { command }) => mission::run(&data_dir, command).await,
        Some(Command::Terrain { command }) => terrain::run(&data_dir, command).await,
        Some(Command::Traffic { command, json }) => traffic::run(&data_dir, command, json).await,
        Some(Command::Video { command }) => video::run(&data_dir, command).await,
        Some(Command::Find {
            network,
            all_serial,
            json,
        }) => find::run(&data_dir, network, all_serial, json).await,
        Some(Command::Login {
            email,
            operator,
            product_updates,
        }) => login(&data_dir, email, operator.as_deref(), product_updates).await,
        Some(Command::Logout) => logout(&data_dir).await,
        Some(Command::Terms { operator, agree }) => {
            terms(&data_dir, operator.as_deref(), agree).await
        }
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
#[allow(clippy::too_many_lines, reason = "one arm per command")]
async fn dev(command: DevCommand) -> Result<(), CliError> {
    match command {
        DevCommand::FakeBoard {
            system_id,
            uid,
            firmware,
            board_id,
            flash_kb,
            bootloader,
            write_apj,
            apj_version,
            apj_kb,
            pace_ms,
        } => {
            let version = FakeVersion::parse(&firmware, None)?;
            let uid = u64::from_str_radix(uid.trim_start_matches("0x"), 16)
                .map_err(|e| CliError::Usage(format!("--uid: {e}")))?;
            let options = fake_board::BoardOptions {
                system_id,
                uid,
                flight_sw_version: version.flight_sw_version,
                board_id,
                flash_kb,
                in_bootloader: bootloader,
                write_apj: write_apj.map(|path| (path, apj_version, apj_kb)),
                pace_ms,
            };
            tokio::task::spawn_blocking(move || fake_board::run(options))
                .await
                .map_err(|e| CliError::Usage(e.to_string()))?
        }
        DevCommand::FakeGimbal { to, system_id } => fake_gimbal(&to, system_id).await,
        DevCommand::FakeVehicle {
            to,
            system_id,
            uid,
            no_uid,
            firmware,
            commit,
            camera,
            camera_video_only,
            camera_silent_at,
            camera_back_at,
            camera_card_left,
            payload_profile,
            payload_fault,
            camera_definition,
            camera_definition_file,
            camera_more_settings,
            camera_photos_said,
            gimbal,
            gimbal_held_by,
            autopilot_camera,
            refuse_arm,
            refuse_disarm,
            startup_burst,
            prearm_failure,
            repeat_warning,
            vibration_high,
            leak,
            fork_messages,
            no_ftp,
            drop_ftp,
            terrain,
            terrain_shift,
            battery,
            no_gps,
            lose_gps_at,
            dead_reckon,
            traffic,
            compasses,
            compasses_calibrated,
            compass_fails,
            accel_uncalibrated,
            accel_fails,
            radio,
            radio_reversed_roll,
            radio_pace,
            motors,
            escs,
            scripting,
            airspeed,
            airspeed_unzeroed,
            px4,
            px4_plane,
        } => {
            let version = FakeVersion::parse(&firmware, commit.as_deref())?;
            let options = FakeOptions {
                no_uid,
                camera,
                camera_video_only,
                camera_silent_s: camera_silent_at
                    .map(|from| (from, camera_back_at.unwrap_or(f64::INFINITY))),
                camera_card_left_mib: camera_card_left,
                payload: fake_payload(payload_profile.as_deref(), &payload_fault, gimbal)?,
                camera_defined: uxduck_mavlink::payload::Defined {
                    uri: camera_definition.map(|uri| Some(uri).filter(|uri| uri != "none")),
                    file: camera_definition_file
                        .map(|path| {
                            std::fs::read(&path).map_err(|e| {
                                CliError::Usage(format!(
                                    "--camera-definition-file: cannot read {}: {e}",
                                    path.display()
                                ))
                            })
                        })
                        .transpose()?,
                },
                camera_more_settings: camera_more_settings.unwrap_or(0),
                camera_photos_said: match camera_photos_said.as_deref() {
                    Some("when-asked") => uxduck_mavlink::payload::PhotosSaid::WhenAsked,
                    Some("never") => uxduck_mavlink::payload::PhotosSaid::Never,
                    _ => uxduck_mavlink::payload::PhotosSaid::AsTaken,
                },
                gimbal,
                gimbal_held_by: gimbal_held_by
                    .as_deref()
                    .map(|who| gimbal_holder(who, system_id))
                    .transpose()?,
                autopilot_camera,
                refuse_arm,
                refuse_disarm,
                startup_burst,
                prearm_failure,
                repeat_warning,
                vibration_high,
                leak,
                fork_messages,
                no_ftp,
                drop_ftp,
                terrain,
                terrain_shift,
                battery,
                gps_lost_from_s: if no_gps { Some(0.0) } else { lose_gps_at },
                dead_reckon,
                traffic,
                compasses: (compasses > 0).then_some((
                    compasses,
                    compasses_calibrated,
                    compass_fails,
                )),
                accel: (accel_uncalibrated, accel_fails),
                radio: radio.then_some((radio_reversed_roll, radio_pace)),
                motors,
                escs,
                scripting,
                airspeed: airspeed.then_some(!airspeed_unzeroed),
                px4: px4.then_some(px4_plane),
            };
            fake_vehicle(&to, system_id, &uid, version, options).await
        }
        DevCommand::Replay {
            practice: Some(exercise),
            tlog,
            from,
            ..
        } => {
            let bytes = std::fs::read(&tlog)
                .map_err(|e| CliError::Usage(format!("cannot read {}: {e}", tlog.display())))?;
            let replayed =
                practice_replay::judge_tlog(&bytes, &exercise, from).map_err(CliError::Usage)?;
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
        DevCommand::DataflashFixture { path } => {
            std::fs::write(&path, uxduck_mavlink::dataflash::fixture::flight())
                .map_err(|e| CliError::Usage(format!("{}: {e}", path.display())))
        }
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
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| CliError::Usage(format!("cannot read standard input: {e}")))?;
    Ok(line.trim().to_owned())
}

/// `uxduck login`: the email code sign-in, in a terminal.
async fn login(
    data_dir: &std::path::Path,
    email: Option<String>,
    operator: Option<&str>,
    product_updates: bool,
) -> Result<(), CliError> {
    let email = match email {
        Some(email) => email,
        None => prompt("Email: ")?,
    }
    .trim()
    .to_lowercase();
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    select_login_operator(&mut client, &email, operator).await?;
    client
        .call("account/signIn", serde_json::json!({ "email": email }))
        .await?;
    eprintln!("A code is on its way to {email}.");
    eprintln!(
        "By entering the code, you agree to UXDuck's Terms of Service ({TERMS_URL}) and acknowledge its Privacy Policy ({PRIVACY_URL})."
    );
    let code = prompt("Code: ")?;
    let state = client
        .call(
            "account/verify",
            serde_json::json!({
                "email": email,
                "code": code,
                "productUpdates": product_updates,
                "termsVersion": uxduck_contracts::version::TERMS_VERSION,
            }),
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

const TERMS_URL: &str = "https://uxduck.com/terms";
const PRIVACY_URL: &str = "https://uxduck.com/privacy";

/// `uxduck terms`: agrees to the Terms of Service for an operator, as the
/// app asks before it arms a real vehicle, with no window.
async fn terms(
    data_dir: &std::path::Path,
    operator: Option<&str>,
    agree: bool,
) -> Result<(), CliError> {
    use uxduck_contracts::dto::operator::{Operator, OperatorListResult};
    use uxduck_contracts::version::TERMS_VERSION;

    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let id = if let Some(id) = operator {
        id.to_owned()
    } else {
        let listed: OperatorListResult =
            serde_json::from_value(client.call("operator/list", serde_json::json!({})).await?)
                .map_err(|error| CliError::Protocol(error.to_string()))?;
        match listed.operators.as_slice() {
            [one] => one.id.clone(),
            [] => {
                let created: Operator = serde_json::from_value(
                    client
                        .call(
                            "operator/create",
                            serde_json::json!({ "name": uxduck_contracts::dto::operator::DEFAULT_OPERATOR_NAME }),
                        )
                        .await?,
                )
                .map_err(|error| CliError::Protocol(error.to_string()))?;
                created.id
            }
            several => {
                let names: Vec<String> = several
                    .iter()
                    .map(|o| format!("{} ({})", o.name, o.id))
                    .collect();
                return Err(CliError::Usage(format!(
                    "Choose who agrees with --operator ID: {}.",
                    names.join(", ")
                )));
            }
        }
    };
    eprintln!("UXDuck's Terms of Service, version {TERMS_VERSION}: {TERMS_URL}");
    eprintln!(
        "You are the operator of your vehicles, and 18 or older, or a parent or guardian agrees for you and supervises your flights. UXDuck is not certified by any aviation authority and is not meant to be the only way you control, navigate or watch a vehicle. What it shows can be wrong, late or missing, and its agent can be wrong. UXDuck is provided as is, and its liability is limited as the terms say."
    );
    if !agree && !prompt("Type \"agree\" to agree to them: ")?.eq_ignore_ascii_case("agree") {
        return Err(CliError::Usage(
            "Not agreed. UXDuck won't arm a real vehicle for this operator until they do.".into(),
        ));
    }
    client
        .call("operator/switch", serde_json::json!({ "operatorId": id }))
        .await?;
    let agreed: Operator = serde_json::from_value(
        client
            .call(
                "terms/accept",
                serde_json::json!({ "version": TERMS_VERSION, "context": "real-vehicle" }),
            )
            .await?,
    )
    .map_err(|error| CliError::Protocol(error.to_string()))?;
    eprintln!(
        "{} agreed to the Terms of Service ({TERMS_VERSION}).",
        agreed.name
    );
    Ok(())
}

/// CLI connections have no window's operator selection. Choose before asking
/// for a code, and use the same handover guards as the people switcher.
async fn select_login_operator(
    client: &mut LocalClient,
    email: &str,
    operator: Option<&str>,
) -> Result<(), CliError> {
    use uxduck_contracts::dto::operator::{Operator, OperatorListResult};

    let id = if let Some(id) = operator {
        id.to_owned()
    } else {
        if email.chars().count() > uxduck_core::domain::operator::MAX_NAME_CHARS {
            return Err(CliError::Usage(
                "This email is too long for a profile name. Create a named local operator with `uxduck rpc call operator/create`, then sign in with --operator ID.".into(),
            ));
        }
        let listed: OperatorListResult =
            serde_json::from_value(client.call("operator/list", serde_json::json!({})).await?)
                .map_err(|error| CliError::Protocol(error.to_string()))?;
        let matches: Vec<_> = listed
            .operators
            .iter()
            .filter(|profile| profile.name.eq_ignore_ascii_case(email))
            .collect();
        match matches.as_slice() {
            [profile] => profile.id.clone(),
            [] => {
                let profile: Operator = serde_json::from_value(
                    client
                        .call("operator/create", serde_json::json!({ "name": email }))
                        .await?,
                )
                .map_err(|error| CliError::Protocol(error.to_string()))?;
                profile.id
            }
            _ => {
                return Err(CliError::Usage(
                    "Several local profiles use this email. Choose one with --operator ID; list them with `uxduck rpc call operator/list`.".into(),
                ));
            }
        }
    };
    client
        .call("operator/switch", serde_json::json!({ "operatorId": id }))
        .await?;
    Ok(())
}

/// `uxduck account username [NAME]`.
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

/// `uxduck account sessions`.
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

/// `uxduck account sign-out`.
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

/// `uxduck account delete`.
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
        AccountCommand::WatchLink { action } => watch_link(data_dir, action).await,
        AccountCommand::LiveSharing { state } => live_sharing(data_dir, state == "on").await,
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
                    "{}  {}  operator {}  {} watching  {ends_in}",
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

async fn live_sharing(data_dir: &std::path::Path, on: bool) -> Result<(), CliError> {
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
            serde_json::json!({ "expectedVersion": version, "liveSharing": on }),
        )
        .await?;
    eprintln!(
        "{}",
        if on {
            "Live sharing is on: watch links can show this computer while you are signed in."
        } else {
            "Live sharing is off: nothing live leaves this computer, and its watch links end."
        }
    );
    Ok(())
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

/// `uxduck sim`: the simulation manager through the API.
#[expect(clippy::too_many_lines, reason = "one arm per subcommand")]
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
            allow_network,
            wipe,
            camera,
            gimbal,
            ardupilot_gimbal,
            payload_profile,
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
                "binary": binary.as_ref().map(|b| b.display().to_string()),
                "allowNetwork": allow_network,
                "wipe": wipe,
                "camera": camera,
                "gimbal": gimbal,
                "ardupilotGimbal": ardupilot_gimbal,
                "payloadProfile": payload_profile,
                "name": name,
            });
            if binary.is_none() {
                sim_build::ensure(data_dir, &vehicle, &autopilot).await?;
            }
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
        SimCommand::Fault {
            id,
            fault,
            state,
            command,
        } => {
            let params = serde_json::json!({
                "id": id, "kind": fault, "command": command, "on": state == "on",
            });
            let params = strip_nulls(params).to_string();
            rpc_call(data_dir, "sim/fault", Some(&params)).await
        }
        SimCommand::Faults { id } => {
            let params = serde_json::json!({ "id": id }).to_string();
            rpc_call(data_dir, "sim/faults", Some(&params)).await
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
        SimCommand::Build {
            checkout,
            vehicle,
            allow_network,
        } => {
            let data_dir = data_dir.to_owned();
            tokio::task::spawn_blocking(move || {
                sim_own::build(&data_dir, &checkout, &vehicle, allow_network)
            })
            .await
            .map_err(|e| CliError::Protocol(e.to_string()))?
        }
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
        VehicleAction::SetHome {
            lat: Some(lat),
            lon: Some(lon),
            altitude,
        } => serde_json::json!({
            "type": "set-home",
            "place": { "latitudeDeg": lat, "longitudeDeg": lon, "altitudeAmslM": altitude },
        }),
        VehicleAction::SetHome { .. } => serde_json::json!({ "type": "set-home" }),
        VehicleAction::SetEkfOrigin { lat, lon, altitude } => serde_json::json!({
            "type": "set-ekf-origin",
            "place": { "latitudeDeg": lat, "longitudeDeg": lon, "altitudeAmslM": altitude },
        }),
    }
}

/// A place given without a height goes on the ground there: its height
/// from `terrain/at`, or an error saying why there is none.
async fn on_the_ground(client: &mut LocalClient, command: &mut Value) -> Result<(), CliError> {
    let Some(place) = command.get_mut("place") else {
        return Ok(());
    };
    if !place["altitudeAmslM"].is_null() {
        return Ok(());
    }
    let ground = client
        .call(
            "terrain/at",
            serde_json::json!({
                "latitudeDeg": place["latitudeDeg"],
                "longitudeDeg": place["longitudeDeg"],
            }),
        )
        .await?;
    let Some(height) = ground["heightM"].as_f64() else {
        let why = ground["detail"]
            .as_str()
            .unwrap_or("there is no height there");
        return Err(CliError::Usage(format!(
            "not sent: the ground's height isn't known ({why}). Give it with --altitude"
        )));
    };
    eprintln!("The ground there is {height} m above sea level.");
    place["altitudeAmslM"] = height.into();
    Ok(())
}

/// `uxduck command`: one `command/send`, and its receipt. A command that
/// was not sent, or that the vehicle refused, fails.
async fn vehicle_command(
    data_dir: &std::path::Path,
    action: VehicleAction,
    vehicle: Option<String>,
    confirm: bool,
    (yes, anyway): (bool, bool),
) -> Result<(), CliError> {
    // A take-off or a mission's start: what the checks hold (K23).
    let launches = matches!(
        action,
        VehicleAction::Takeoff { .. } | VehicleAction::MissionStart
    );
    let mut command = action_json(&action);
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    on_the_ground(&mut client, &mut command).await?;
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
    // Launching anyway names the checks it launches past (ADR-0038 K24).
    let mut past: Vec<Value> = Vec::new();
    if anyway && launches {
        let gating = preflight::gating(&mut client, &vehicle_id).await?;
        if !gating.is_empty() {
            let titles: Vec<&str> = gating.iter().filter_map(|c| c["title"].as_str()).collect();
            eprintln!("Past checks that hold the launch: {}.", titles.join(", "));
            if !yes {
                if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    return Err(CliError::Usage(
                        "not sent: run it again with --yes to launch past them".into(),
                    ));
                }
                let answer = prompt("Launch past them? [y/N] ")?;
                if !(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")) {
                    return Err(CliError::Usage("not sent".into()));
                }
            }
        }
        past = gating.iter().map(|c| c["id"].clone()).collect();
    }
    let mut params = serde_json::json!({
        "vehicleId": vehicle_id,
        "command": command,
        "confirm": confirm,
    });
    if !past.is_empty() {
        params["overridePreflight"] = Value::Array(past);
    }
    let receipt = client.call("command/send", params).await?;
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

/// As the app does: a command whose risk, on the ground or, while
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

/// The daemon that started since `before`, once it answers. It listens on its
/// socket before it writes `daemon.json`, which `uxduck status` and the
/// desktop app read, so a connection alone is too early to say it started.
async fn started_since(
    data_dir: &std::path::Path,
    before: Option<u64>,
) -> Option<(crate::paths::DaemonInfo, LocalClient)> {
    let info = crate::paths::read_info(data_dir).filter(|info| Some(info.started_us) != before)?;
    let client = LocalClient::connect(data_dir).await.ok()?;
    Some((info, client))
}

#[cfg(all(test, unix))]
mod started_since_tests {
    use super::*;

    #[tokio::test]
    async fn a_daemon_that_listens_has_not_started_until_status_can_read_it() {
        let dir = tempfile::tempdir().unwrap();
        let socket = crate::paths::socket_path(dir.path());
        let _listening = tokio::net::UnixListener::bind(&socket).unwrap();
        let written = |started_us| {
            crate::paths::write_info(
                dir.path(),
                &crate::paths::DaemonInfo {
                    pid: std::process::id(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    port: 0,
                    socket: socket.display().to_string(),
                    ui_url: String::new(),
                    dev: false,
                    started_us,
                },
            )
            .unwrap();
        };
        // The daemon listens first: `uxduck status` would still say none runs.
        assert!(LocalClient::connect(dir.path()).await.is_ok());
        assert!(started_since(dir.path(), None).await.is_none());
        // What a daemon that was killed left behind is not the one listening now.
        written(1);
        assert!(started_since(dir.path(), Some(1)).await.is_none());
        written(2);
        let (info, _) = started_since(dir.path(), Some(1)).await.unwrap();
        assert_eq!(info.started_us, 2);
        let _ = std::fs::remove_file(&socket);
    }
}

/// Starts UXDuck in the background for `open` and waits until it has started.
/// It outlives this command, in its own process group, with no terminal.
pub(crate) async fn start_daemon(
    data_dir: &std::path::Path,
    dev: bool,
) -> Result<LocalClient, CliError> {
    // Several explicit app launches may arrive together. Serialize startup,
    // then check again: the daemon's lifetime lock alone only rejects a loser.
    std::fs::create_dir_all(data_dir)
        .map_err(|e| CliError::Usage(format!("cannot create {}: {e}", data_dir.display())))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data_dir.join("daemon-start.lock"))
        .map_err(|e| CliError::Usage(format!("cannot open UXDuck's startup lock: {e}")))?;
    let _startup = tokio::task::spawn_blocking(move || {
        lock.lock()?;
        Ok::<_, std::io::Error>(lock)
    })
    .await
    .map_err(|error| CliError::Usage(format!("could not coordinate UXDuck startup: {error}")))?
    .map_err(|error| CliError::Usage(format!("could not coordinate UXDuck startup: {error}")))?;
    if let Ok(client) = LocalClient::connect(data_dir).await {
        return Ok(client);
    }
    let exe = crate::paths::executable()
        .map_err(|e| CliError::Usage(format!("cannot find uxduck to start it: {e}")))?;
    let mut command = std::process::Command::new(exe);
    let mut diagnostic = tempfile::NamedTempFile::new()
        .map_err(|e| CliError::Usage(format!("cannot make a temporary file: {e}")))?;
    command.env(crate::startup::ERROR_FILE, diagnostic.path());
    command.env("UXDUCK_BACKGROUND_LOG", data_dir.join("logs/daemon.log"));
    // This command exits once the daemon answers, and macOS refuses the
    // local network to a daemon whose responsible process is gone.
    #[cfg(target_os = "macos")]
    command.env(crate::responsibility::DISCLAIM, "1");
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
        // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP: a console of its own
        // with no window, which the console programs it starts (a
        // simulator, netstat, ffmpeg) share instead of each opening one.
        command.creation_flags(0x0800_0000 | 0x0000_0200);
    }
    // What a daemon that was killed left behind is not this one's.
    let before = crate::paths::read_info(data_dir).map(|info| info.started_us);
    let mut child = command
        .spawn()
        .map_err(|e| CliError::Usage(format!("could not start UXDuck: {e}")))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Some((_, client)) = started_since(data_dir, before).await {
            return Ok(client);
        }
        if let Ok(Some(status)) = child.try_wait() {
            let reason = crate::startup::read(diagnostic.as_file_mut())
                .map_err(|e| CliError::Usage(format!("cannot read why UXDuck stopped: {e}")))?;
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
        // it ran in the menu bar only, the tray says so under its icon;
        // never at first start, nor when a window or page shows.
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

/// Release builds use the hosted model only.
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
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| CliError::Usage(format!("cannot read standard input: {e}")))?;
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

/// `uxduck plugin install`: what it may do, and what its checks do.
async fn plugin_install(
    data_dir: &std::path::Path,
    path: &std::path::Path,
    checks_block: bool,
    checks_warn: bool,
) -> Result<(), CliError> {
    let path = std::fs::canonicalize(path)
        .map_err(|e| CliError::Usage(format!("cannot read {}: {e}", path.display())))?;
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let mut params = serde_json::json!({ "path": path.display().to_string() });
    if checks_block || checks_warn {
        params["checksBlock"] = serde_json::json!(checks_block);
    }
    let plugin = client.call("plugin/install", params).await?;
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
    // The hosts it may fetch from (S170).
    if let Some(hosts) = plugin["hosts"].as_array().filter(|h| !h.is_empty()) {
        let hosts: Vec<&str> = hosts.iter().filter_map(|h| h.as_str()).collect();
        println!("It may fetch from {}.", hosts.join(", "));
    }
    if let Some(words) = checks_words(&plugin) {
        println!("{words}");
    }
    Ok(())
}

/// What a plugin's checks do, in words.
fn checks_words(plugin: &serde_json::Value) -> Option<String> {
    let checks = plugin["checks"].as_array().cloned().unwrap_or_default();
    if checks.is_empty() {
        return None;
    }
    let titles: Vec<&str> = checks.iter().filter_map(|c| c["title"].as_str()).collect();
    let asks = checks.iter().any(|c| c["severity"] == "block");
    let doing = match (asks, plugin["checksBlock"].as_bool()) {
        (false, _) | (true, Some(false)) => "They only warn.".to_owned(),
        (true, Some(true)) => "They hold the launch while they don't pass.".to_owned(),
        (true, None) => format!(
            "They ask to hold the launch, and only warn until you allow it: uxduck plugin checks {} block",
            plugin["id"].as_str().unwrap_or_default()
        ),
    };
    Some(format!(
        "Its preflight checks: {}. {doing}",
        titles.join(", ")
    ))
}

async fn plugin(data_dir: &std::path::Path, command: PluginCommand) -> Result<(), CliError> {
    match command {
        PluginCommand::Install {
            path,
            checks_block,
            checks_warn,
        } => plugin_install(data_dir, &path, checks_block, checks_warn).await?,
        PluginCommand::Checks { id, may } => {
            let mut client = LocalClient::connect(data_dir).await?;
            client.initialize().await?;
            let plugin = client
                .call(
                    "plugin/setChecksBlock",
                    serde_json::json!({ "id": id, "block": matches!(may, ChecksMay::Block) }),
                )
                .await?;
            println!("{}", checks_words(&plugin).unwrap_or_default());
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
    let bytes = std::fs::read(tlog).map_err(|e| {
        CliError::Usage(format!(
            "cannot read {}: {e}. Give a recording with --tlog.",
            tlog.display()
        ))
    })?;
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
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Test {
        expect: Expectations,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Expectations {
        items: Vec<String>,
    }
    let file = path.join("test.json");
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("No test.json: replay only; no expectations checked.");
            return Ok(());
        }
        Err(error) => return Err(CliError::Usage(format!("{}: {error}", file.display()))),
    };
    let expected: Test =
        serde_json::from_str(&text).map_err(|e| CliError::Usage(format!("test.json: {e}")))?;
    if expected.expect.items.is_empty() {
        return Err(CliError::Usage(
            "test.json: expect.items must list at least one key; omit test.json for replay only"
                .into(),
        ));
    }
    let missing: Vec<String> = expected
        .expect
        .items
        .into_iter()
        .filter(|key| !produced.contains(key))
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
        DaemonAction::Start => {
            match LocalClient::connect(data_dir).await {
                Ok(_) => {}
                Err(CliError::NoDaemon(_)) => {
                    start_daemon(data_dir, false).await?;
                }
                Err(error) => return Err(error),
            }
            let target = std::fs::read_to_string(data_dir.join(REOPEN_AFTER_UPDATE)).ok();
            if matches!(target.as_deref(), Some("window" | "browser")) {
                open(data_dir, false, false, target.as_deref()).await?;
            }
            Ok(())
        }
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
        if let Some(target) = open_target(&mut client).await? {
            let file = data_dir.join(REOPEN_AFTER_UPDATE);
            std::fs::write(&file, target)
                .map_err(|e| CliError::Usage(format!("cannot write {}: {e}", file.display())))?;
        } else {
            let _ = std::fs::remove_file(data_dir.join(REOPEN_AFTER_UPDATE));
        }
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
    let target = open_target(&mut client).await?;
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
        if let Some((info, mut client)) = started_since(data_dir, started).await
            && client.initialize().await.is_ok()
        {
            println!("Restarted on port {}.", info.port);
            if let Some(target) = target {
                open(data_dir, false, false, Some(target)).await?;
            }
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

/// External installers can query even an older daemon before stopping it.
/// The tray is an operator CLI; only a shell or page needs restoring.
async fn open_target(client: &mut LocalClient) -> Result<Option<&'static str>, CliError> {
    let result = client.call("client/list", serde_json::json!({})).await?;
    let Some(clients) = result["clients"].as_array() else {
        return Err(CliError::Protocol("client/list returned no clients".into()));
    };
    let connected = |kind| {
        clients.iter().any(|row| {
            row["connections"].as_u64().unwrap_or(0) > 0 && row["client"]["kind"] == kind
        })
    };
    Ok(if connected("shell") {
        Some("window")
    } else if connected("operator-ui") {
        Some("browser")
    } else {
        None
    })
}

/// `uxduck dev replay`: a recorded tlog's frames, one datagram each, at the
/// pace they were recorded.
/// `uxduck service install` and `uninstall`: Open at login, as the
/// switch in the menu bar and Settings turns it on and off.
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
    let bytes = std::fs::read(tlog)
        .map_err(|e| CliError::Usage(format!("cannot read {}: {e}", tlog.display())))?;
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
    let bytes = std::fs::read(tlog)
        .map_err(|e| CliError::Usage(format!("cannot read {}: {e}", tlog.display())))?;
    let every = uxduck_mavlink::tlog::read(&bytes);
    let total = every.len();
    let records = uxduck_mavlink::tlog::without_ground_stations(every);
    let Some(first) = records.first().map(|(at, _)| *at) else {
        return Err(CliError::Usage(format!(
            "{} holds no verified autopilot telemetry. UDP replay requires a recorded \
             autopilot heartbeat and excludes commands, transfers, unknown messages and \
             unidentified senders. Use in-app Replay to inspect this log without transmitting it.",
            tlog.display()
        )));
    };
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(|e| CliError::Usage(format!("cannot open a UDP socket: {e}")))?;
    eprintln!(
        "replaying {} verified telemetry frames to {target}; omitted {} frames that are not \
         verified autopilot telemetry (commands, transfers, unknown messages or senders). \
         Use in-app Replay to inspect the full log without transmitting it.",
        records.len(),
        total - records.len()
    );
    let started = tokio::time::Instant::now();
    for (at, frame) in &records {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let offset = std::time::Duration::from_micros(((at - first) as f64 / speed) as u64);
        tokio::time::sleep_until(started + offset).await;
        socket
            .send_to(frame, target)
            .await
            .map_err(|e| CliError::Usage(format!("cannot send to {target}: {e}")))?;
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
    let file = std::fs::read(capture)
        .map_err(|e| CliError::Usage(format!("cannot read {}: {e}", capture.display())))?;
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| CliError::Usage(format!("cannot open a UDP socket: {e}")))?;
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
            socket
                .send_to(&datagram.bytes, target)
                .await
                .map_err(|e| CliError::Usage(format!("cannot send to {target}: {e}")))?;
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

/// `--gimbal-held-by`: `autopilot` for the vehicle's own, or a MAVLink
/// system and component such as `254/190`.
fn gimbal_holder(who: &str, system_id: u8) -> Result<(u8, u8), CliError> {
    if who == "autopilot" {
        return Ok((system_id, 1));
    }
    who.split_once('/')
        .and_then(|(system, component)| Some((system.parse().ok()?, component.parse().ok()?)))
        .ok_or_else(|| {
            CliError::Usage(
                "--gimbal-held-by is autopilot, or a system and component such as 254/190".into(),
            )
        })
}

/// `--payload-profile` and each `--payload-fault` of `dev fake-vehicle`:
/// the kind its camera and gimbal are, and the faults they start with.
fn fake_payload(
    profile: Option<&str>,
    faults: &[String],
    gimbal: bool,
) -> Result<
    (
        uxduck_mavlink::payload::Profile,
        uxduck_mavlink::payload::Faults,
    ),
    CliError,
> {
    use uxduck_core::domain::link::{PayloadProfile, SimFault, SimFaultKind};
    let named = |name: &str| serde_json::Value::String(name.to_owned());
    let name = profile;
    let profile: Option<PayloadProfile> = profile
        .map(|name| serde_json::from_value(named(name)))
        .transpose()
        .map_err(|e| CliError::Usage(format!("--payload-profile: {e}")))?;
    if let Some((_, name)) = profile
        .zip(name)
        .filter(|(profile, _)| profile.integrated() && !gimbal)
    {
        return Err(CliError::Usage(format!(
            "--payload-profile {name} is a camera and a gimbal in one, and needs --gimbal"
        )));
    }
    let faults = faults
        .iter()
        .map(|fault| {
            let (name, command) = fault
                .split_once('=')
                .map_or((fault.as_str(), None), |(name, command)| {
                    (name, Some(command))
                });
            let kind: SimFaultKind = serde_json::from_value(named(name)).map_err(|_| {
                CliError::Usage(format!("--payload-fault: no fault is called {name}"))
            })?;
            let command = command
                .map(str::parse::<u16>)
                .transpose()
                .map_err(|e| CliError::Usage(format!("--payload-fault {fault}: {e}")))?;
            if kind.names_a_command() != command.is_some() {
                return Err(CliError::Usage(format!(
                    "--payload-fault {name} is written {}",
                    if kind.names_a_command() {
                        "with its MAV_CMD number, such as command-refused=2000"
                    } else {
                        "without a number"
                    }
                )));
            }
            Ok(SimFault { kind, command })
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    Ok((profile.into(), uxduck_mavlink::payload::Faults::of(&faults)))
}

/// How `dev fake-vehicle` behaves beyond its identity.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is a flag of the fake vehicle"
)]
struct FakeOptions {
    no_uid: bool,
    /// A camera, and the video stream it names if it names one.
    camera: Option<String>,
    camera_video_only: bool,
    /// From when until when its camera is silent, in seconds.
    camera_silent_s: Option<(f64, f64)>,
    /// The room left on its camera's card, when not a whole empty card.
    camera_card_left_mib: Option<f32>,
    /// What kind its camera and gimbal are, and the faults they start with.
    payload: (
        uxduck_mavlink::payload::Profile,
        uxduck_mavlink::payload::Faults,
    ),
    /// What its camera says of its definition file, when not its own.
    camera_defined: uxduck_mavlink::payload::Defined,
    /// How many settings its camera lists beyond its own.
    camera_more_settings: u16,
    /// When its camera describes a photo it took.
    camera_photos_said: uxduck_mavlink::payload::PhotosSaid,
    gimbal: bool,
    /// The system and component its gimbal starts in the control of.
    gimbal_held_by: Option<(u8, u8)>,
    autopilot_camera: bool,
    refuse_arm: bool,
    refuse_disarm: bool,
    startup_burst: bool,
    prearm_failure: bool,
    repeat_warning: bool,
    vibration_high: bool,
    leak: bool,
    fork_messages: bool,
    no_ftp: bool,
    drop_ftp: bool,
    terrain: bool,
    terrain_shift: i16,
    battery: f64,
    gps_lost_from_s: Option<f64>,
    dead_reckon: bool,
    traffic: u8,
    /// How many compasses, calibrated or not, and a failure to report.
    compasses: Option<(u8, bool, Option<u8>)>,
    /// Its accelerometers not calibrated, and failing their calibration.
    accel: (bool, bool),
    /// Its radio, uncalibrated: whether its roll runs backwards, and
    /// its sticks' pace.
    radio: Option<(bool, f64)>,
    /// Its frame listed for the motor test.
    motors: bool,
    /// Its ESCs' `MOT_PWM_TYPE`, listed for their calibration.
    escs: Option<u8>,
    /// Its `SCR_ENABLE`, listed with a script.
    scripting: Option<u8>,
    /// A plane with an airspeed sensor, zeroed or not.
    airspeed: Option<bool>,
    /// PX4: a plane, or a quad.
    px4: Option<bool>,
}

/// The fake vehicle's compasses, accelerometers, radio
///, frame and airspeed sensor, as the flags ask.
fn fake_sensors(
    vehicle: &mut uxduck_mavlink::sim::FakeVehicle,
    compasses: Option<(u8, bool, Option<u8>)>,
    (accel_uncalibrated, accel_fails): (bool, bool),
    (radio, motors, airspeed): (Option<(bool, f64)>, bool, Option<bool>),
) {
    if let Some(zeroed) = airspeed {
        vehicle.add_airspeed(zeroed);
    }
    if motors {
        vehicle.add_motors();
    }
    if let Some((roll_reversed, pace)) = radio {
        vehicle.add_radio(roll_reversed);
        vehicle.radio.pace = if pace > 0.0 { pace } else { 1.0 };
    }
    if let Some((count, calibrated, fails)) = compasses {
        vehicle.add_compasses(count, calibrated);
        if let Some(compasses) = &mut vehicle.compasses {
            compasses.fails_with = fails;
        }
    }
    // Listed whenever its calibration is asked about; calibrated otherwise,
    // and answering a calibration either way.
    if accel_uncalibrated || accel_fails {
        vehicle.add_accel(!accel_uncalibrated);
    }
    vehicle.accel.fails = accel_fails;
}

/// How often a fake payload is stepped.
const PAYLOAD_STEP: std::time::Duration = std::time::Duration::from_millis(20);

/// `uxduck dev fake-gimbal`: the simulated gimbal device with nothing
/// else, on its own clock.
async fn fake_gimbal(to: &str, system_id: u8) -> Result<(), CliError> {
    use uxduck_mavlink::payload::{ManagedBy, Payload};

    let target: std::net::SocketAddr = to
        .parse()
        .map_err(|e| CliError::Usage(format!("--to: {e}")))?;
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(|e| CliError::Usage(format!("cannot open a UDP socket: {e}")))?;
    let mut payload = Payload::new(system_id).with_gimbal(ManagedBy::Autopilot);
    if let Some(gimbal) = payload.gimbal.as_mut() {
        gimbal.reports_from_north = true;
    }
    eprintln!("fake gimbal {system_id}/154 sending to {target}; Ctrl-C to stop");
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut step = tokio::time::interval(PAYLOAD_STEP);
    let started = std::time::Instant::now();
    let mut buffer = vec![0u8; 2048];
    loop {
        let frames = tokio::select! {
            _ = heartbeat.tick() => payload.announce(),
            _ = step.tick() => {
                let now_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                payload.step(now_ms)
            }
            received = socket.recv_from(&mut buffer) => {
                let len = match received {
                    Ok((len, _)) => len,
                    // Windows reports an earlier datagram's port unreachable
                    // here when nothing listens at --to yet. Keep sending.
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => continue,
                    Err(error) => {
                        return Err(CliError::Usage(format!("cannot receive replies: {error}")));
                    }
                };
                uxduck_mavlink::framing::split(&buffer[..len])
                    .frames
                    .into_iter()
                    .flat_map(|frame| payload.receive(frame))
                    .collect()
            }
            _ = tokio::signal::ctrl_c() => return Ok(()),
        };
        for frame in frames {
            socket
                .send_to(&frame, target)
                .await
                .map_err(|e| CliError::Usage(format!("cannot send to {target}: {e}")))?;
        }
    }
}

#[allow(clippy::too_many_lines, reason = "one line per fake-vehicle option")]
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
        camera_video_only,
        camera_silent_s,
        camera_card_left_mib,
        payload: (profile, faults),
        camera_defined,
        camera_more_settings,
        camera_photos_said,
        gimbal,
        gimbal_held_by,
        autopilot_camera,
        refuse_arm,
        refuse_disarm,
        startup_burst,
        prearm_failure,
        repeat_warning,
        vibration_high,
        leak,
        fork_messages,
        no_ftp,
        drop_ftp,
        terrain,
        terrain_shift,
        battery,
        gps_lost_from_s,
        dead_reckon,
        traffic,
        compasses,
        accel: (accel_uncalibrated, accel_fails),
        radio,
        motors,
        escs,
        scripting,
        airspeed,
        px4,
    } = options;
    let target: std::net::SocketAddr = to
        .parse()
        .map_err(|e| CliError::Usage(format!("--to: {e}")))?;
    let uid = u64::from_str_radix(uid.trim_start_matches("0x"), 16)
        .map_err(|e| CliError::Usage(format!("--uid: {e}")))?;
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(|e| CliError::Usage(format!("cannot open a UDP socket: {e}")))?;
    let mut vehicle = uxduck_mavlink::sim::FakeVehicle::new(system_id, uid);
    vehicle.answers_version = !no_uid;
    vehicle.flight_sw_version = version.flight_sw_version;
    vehicle.commit = version.commit;
    if camera.is_some() || gimbal {
        use uxduck_mavlink::payload::Profile;
        let mut payload = uxduck_mavlink::payload::Payload::new(system_id);
        // One unit that is both, on the camera's component.
        let unit = profile.unit() && gimbal;
        if let Some(stream) = camera {
            let stream = Some(stream).filter(|stream| !stream.is_empty());
            payload = if unit {
                payload.with_unit(stream)
            } else {
                payload.with_camera(stream)
            };
            if let Some(camera) = &mut payload.camera {
                camera.video_only = camera_video_only || profile == Profile::StreamOnly;
                camera.mode_bound = profile == Profile::PhotoVideo;
                camera.thermal_sensor = unit && profile == Profile::ThermalTracking;
                camera.tracks = unit && profile == Profile::ThermalTracking;
                if let Some(mib) = camera_card_left_mib {
                    camera.leave_room(mib);
                }
                camera_defined.apply(camera);
                camera.more_settings = camera_more_settings;
                camera.photos_said = camera_photos_said;
            }
        }
        if gimbal && !unit {
            payload = payload.with_gimbal(uxduck_mavlink::payload::ManagedBy::Itself);
        }
        if let (Some(gimbal), Some(who)) = (&mut payload.gimbal, gimbal_held_by) {
            gimbal.held_by(who);
        }
        payload.set_faults(faults);
        vehicle.payload = Some(payload);
        vehicle.camera_silent_s = camera_silent_s;
    }
    vehicle.own_camera = autopilot_camera;
    vehicle.refuses_arm = refuse_arm;
    vehicle.refuses_disarm = refuse_disarm;
    vehicle.startup_burst = startup_burst;
    vehicle.prearm_failure = prearm_failure;
    vehicle.repeat_warning = repeat_warning;
    vehicle.vibration_high = vibration_high;
    vehicle.leak = leak;
    vehicle.fork_messages = fork_messages;
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
    vehicle.dead_reckon = dead_reckon;
    vehicle.traffic = traffic;
    fake_sensors(
        &mut vehicle,
        compasses,
        (accel_uncalibrated, accel_fails),
        (radio, motors, airspeed),
    );
    if let Some(pwm_type) = escs {
        vehicle.add_escs(pwm_type);
    }
    if let Some(enabled) = scripting {
        vehicle.add_scripting(enabled);
    }
    if let Some(plane) = px4 {
        vehicle.make_px4(plane, !accel_uncalibrated);
        if let Some(px4) = &mut vehicle.px4 {
            px4.accel_fails = accel_fails;
        }
    }
    eprintln!("fake vehicle {system_id} (uid {uid:x}) sending to {target}; Ctrl-C to stop");
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(1));
    // Telemetry at 5 Hz, one datagram per frame as an autopilot sends them.
    let mut telemetry = tokio::time::interval(std::time::Duration::from_millis(200));
    // The payload moves at 50 Hz, so its gimbal turns smoothly and reports
    // ten times a second.
    let mut payload = tokio::time::interval(PAYLOAD_STEP);
    let started = std::time::Instant::now();
    let mut buffer = vec![0u8; 2048];
    let unsent = |e: std::io::Error| CliError::Usage(format!("cannot send to {target}: {e}"));
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                socket.send_to(&vehicle.heartbeat(), target).await.map_err(unsent)?;
                for frame in vehicle.payload_heartbeats() {
                    socket.send_to(&frame, target).await.map_err(unsent)?;
                }
            }
            _ = payload.tick(), if vehicle.payload.is_some() => {
                for frame in vehicle.payload_step(started.elapsed().as_secs_f64()) {
                    socket.send_to(&frame, target).await.map_err(unsent)?;
                }
            }
            _ = telemetry.tick() => {
                let t_s = started.elapsed().as_secs_f64();
                for frame in vehicle.telemetry(t_s) {
                    socket.send_to(&frame, target).await.map_err(unsent)?;
                }
                // After its first second, so the daemon knows it by then.
                if t_s > 1.0 {
                    for frame in vehicle.status_texts(t_s) {
                        socket.send_to(&frame, target).await.map_err(unsent)?;
                    }
                }
            }
            received = socket.recv_from(&mut buffer) => {
                let (len, from) = match received {
                    Ok(received) => received,
                    // Windows reports an earlier datagram's port unreachable
                    // here when nothing listens at --to yet; keep sending.
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => continue,
                    Err(error) => {
                        return Err(CliError::Usage(format!("cannot receive replies: {error}")));
                    }
                };
                for reply in vehicle.answer(&buffer[..len]) {
                    socket
                        .send_to(&reply, from)
                        .await
                        .map_err(|e| CliError::Usage(format!("cannot answer {from}: {e}")))?;
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
    (links, onboard): (
        Vec<uxduck_contracts::dto::link::LinkAddParams>,
        Option<crate::daemon::onboard::Onboard>,
    ),
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
        port_fallback: args.port.is_none(),
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
        onboard,
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
    let video = client.call("video/list", serde_json::json!({})).await?;
    let listens = |port: String, error: Option<&str>| {
        let status =
            error.map_or_else(|| "listening".to_owned(), |e| format!("not listening: {e}"));
        println!(
            "{port:<6} {:<6} {:<36} {status}",
            "UDP", "camera video (every network)"
        );
    };
    match (video["port"].as_u64(), video["listenError"].as_str()) {
        (Some(port), _) => listens(port.to_string(), None),
        (None, Some(error)) => listens("-".into(), Some(error)),
        (None, None) => {}
    }
    for extra in video["extraPorts"].as_array().into_iter().flatten() {
        listens(extra["port"].to_string(), extra["error"].as_str());
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

/// A command an agent asks to run, as `uxduck approvals` prints it: every
/// line of it, each behind a bar, never shortened (ADR-0065 T14). The text
/// is UXDuck's own, in which a character a person could not otherwise see
/// is already a mark.
fn command_lines(shown: &str) -> Vec<String> {
    shown
        .split('\n')
        .map(|line| format!("    | {line}"))
        .collect()
}

/// One waiting approval as `uxduck approvals` lists it, with its command
/// under it when it asks to run one.
fn approval_lines(approval: &serde_json::Value, left_s: u128) -> Vec<String> {
    let mut lines = vec![format!(
        "{}  {} wants to {}  ({} s left)",
        approval["id"].as_str().unwrap_or(""),
        approval["clientName"].as_str().unwrap_or(""),
        approval["summary"].as_str().unwrap_or(""),
        left_s
    )];
    if let Some(command) = approval["shellCommandShown"].as_str() {
        lines.push(
            "    The command, all of it. ⏎ ends a line, ⇥ is a tab and ⟨U+000D⟩ and the like are characters you could not otherwise see. What it prints goes to the agent and its AI model.".into(),
        );
        lines.extend(command_lines(command));
    }
    lines
}

#[cfg(test)]
mod approval_lines_tests {
    use super::*;

    #[test]
    fn a_command_is_listed_whole_under_its_request_and_another_request_is_one_line() {
        let long = format!("echo {}; rm -rf ~", "a".repeat(3000));
        let shown = format!("echo one⏎\n⇥echo two⟨U+000D⟩three⏎\n{long}");
        let lines = approval_lines(
            &serde_json::json!({
                "id": "ap1", "clientName": "claude-code",
                "summary": "run a command on Buggy › Camera Pi",
                "shellCommandShown": shown,
            }),
            42,
        );
        assert_eq!(
            lines[0],
            "ap1  claude-code wants to run a command on Buggy › Camera Pi  (42 s left)"
        );
        assert!(lines[1].contains("The command, all of it."), "{}", lines[1]);
        assert_eq!(
            lines[2..],
            [
                "    | echo one⏎".to_owned(),
                "    | ⇥echo two⟨U+000D⟩three⏎".to_owned(),
                format!("    | {long}"),
            ]
        );
        // Every character of it is there, in order.
        let printed: Vec<&str> = lines[2..].iter().map(|l| &l[6..]).collect();
        assert_eq!(printed.join("\n"), shown);

        let other = approval_lines(
            &serde_json::json!({ "id": "ap2", "clientName": "Codex", "summary": "arm Buggy" }),
            7,
        );
        assert_eq!(other, ["ap2  Codex wants to arm Buggy  (7 s left)"]);
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
                for line in approval_lines(&approval, left) {
                    println!("{line}");
                }
            }
            println!("Answer with `uxduck approvals allow <id>` or `uxduck approvals deny <id>`.");
        }
        Some(ApprovalsCommand::Allow { id, remember }) => {
            let remember = match remember {
                RememberChoice::Once => "once",
                RememberChoice::Connection => "connection",
                RememberChoice::Always => "always",
            };
            // A command is printed before it is allowed, so what was
            // allowed is on the screen whether or not the list was read.
            let list = client.call("approval/list", serde_json::json!({})).await?;
            let asked = list["approvals"]
                .as_array()
                .and_then(|all| all.iter().find(|a| a["id"].as_str() == Some(id.as_str())));
            if let Some(command) = asked.and_then(|a| a["shellCommandShown"].as_str()) {
                println!("Allowing this command:");
                for line in command_lines(command) {
                    println!("{line}");
                }
            }
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
/// resource budget and `uxduck wakeups` can be seen to catch one.
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

#[cfg(all(test, feature = "plugins"))]
mod plugin_test_checks {
    use super::*;

    #[test]
    fn malformed_expectations_never_pass() {
        let dir = tempfile::tempdir().unwrap();
        for text in [
            "{}",
            r#"{"expect":{"itmes":["missing"]}}"#,
            r#"{"expect":{"items":[42]}}"#,
            r#"{"expect":{"items":[]}}"#,
            r#"{"expect":{"items":"missing"}}"#,
            r#"{"expect":{"items":[]},"unexpected":true}"#,
        ] {
            std::fs::write(dir.path().join("test.json"), text).unwrap();
            assert!(
                own_test(dir.path(), &std::collections::BTreeSet::new()).is_err(),
                "{text}"
            );
        }
        std::fs::remove_file(dir.path().join("test.json")).unwrap();
        std::fs::create_dir(dir.path().join("test.json")).unwrap();
        assert!(own_test(dir.path(), &std::collections::BTreeSet::new()).is_err());
    }

    #[test]
    fn expectations_check_every_key_and_allow_replay_without_assertions() {
        let dir = tempfile::tempdir().unwrap();
        assert!(own_test(dir.path(), &std::collections::BTreeSet::new()).is_ok());
        std::fs::write(
            dir.path().join("test.json"),
            r#"{"expect":{"items":["test.first","test.second"]}}"#,
        )
        .unwrap();
        let produced = ["test.first".into()].into_iter().collect();
        let error = own_test(dir.path(), &produced).unwrap_err().to_string();
        assert!(error.contains("never produced test.second"), "{error}");
        let produced = ["test.first".into(), "test.second".into()]
            .into_iter()
            .collect();
        assert!(own_test(dir.path(), &produced).is_ok());
    }
}

#[cfg(test)]
mod fake_payload_arguments {
    use uxduck_mavlink::payload::{Faults, Profile};

    use super::fake_payload;

    fn faults(asked: &[&str]) -> Result<Faults, String> {
        let asked: Vec<String> = asked.iter().map(|&fault| fault.to_owned()).collect();
        fake_payload(None, &asked, true)
            .map(|(_, faults)| faults)
            .map_err(|refused| refused.to_string())
    }

    /// ADR-0063 Y16: `dev fake-vehicle` starts as the kind of camera and
    /// gimbal asked for, with the faults named, each written as its kind is.
    #[test]
    fn a_fake_vehicles_kind_and_faults_are_read_and_a_fault_written_wrong_is_refused() {
        let kind = |name, gimbal| fake_payload(name, &[], gimbal).map(|(kind, _)| kind);
        assert_eq!(kind(None, false).ok(), Some(Profile::Standard));
        assert_eq!(
            kind(Some("stream-only"), false).ok(),
            Some(Profile::StreamOnly)
        );
        assert_eq!(
            kind(Some("photo-video"), true).ok(),
            Some(Profile::PhotoVideo)
        );
        assert_eq!(
            kind(Some("integrated"), true).ok(),
            Some(Profile::Integrated)
        );
        assert_eq!(
            kind(Some("thermal-tracking"), true).ok(),
            Some(Profile::ThermalTracking)
        );
        // One unit is a camera and a gimbal, so it needs both.
        let alone = kind(Some("integrated"), false).expect_err("needs a gimbal");
        assert!(alone.to_string().contains("needs --gimbal"), "{alone}");
        let alone = kind(Some("thermal-tracking"), false).expect_err("needs a gimbal");
        assert!(
            alone
                .to_string()
                .contains("--payload-profile thermal-tracking is a camera and a gimbal in one"),
            "{alone}"
        );
        assert!(kind(Some("thermal"), true).is_err());

        assert_eq!(faults(&[]), Ok(Faults::default()));
        assert_eq!(
            faults(&["card-full", "gimbal-error", "command-refused=2000"]),
            Ok(Faults {
                card_full: true,
                gimbal_error: true,
                refused: Some(2000),
                ..Faults::default()
            })
        );
        assert_eq!(
            faults(&["command-in-progress=2500"]),
            Ok(Faults {
                slow: Some(2500),
                ..Faults::default()
            })
        );
        for (wrong, why) in [
            ("card-empty", "no fault is called card-empty"),
            ("command-refused", "with its MAV_CMD number"),
            ("card-full=1", "without a number"),
            (
                "command-refused=photo",
                "--payload-fault command-refused=photo",
            ),
        ] {
            let refused = faults(&[wrong]).expect_err(wrong);
            assert!(refused.contains(why), "{wrong}: {refused}");
        }
    }
}

#[cfg(test)]
mod go_to_arguments {
    use clap::Parser;

    use super::{Cli, Command, VehicleAction};

    #[test]
    fn west_south_and_below_are_values_not_flags() {
        let cli = Cli::try_parse_from([
            "uxduck",
            "command",
            "go-to",
            "--lat",
            "-33.85",
            "--lon",
            "-121.88",
            "--altitude",
            "-20",
        ])
        .expect("negative numbers are values");
        let Some(Command::Send { action, .. }) = cli.command else {
            panic!("{cli:?}")
        };
        let VehicleAction::GoTo {
            lat, lon, altitude, ..
        } = action
        else {
            panic!("{action:?}")
        };
        assert_eq!((lat, lon, altitude), (-33.85, -121.88, -20.0));
    }

    #[test]
    fn home_goes_on_the_ground_at_a_place_or_where_the_vehicle_is() {
        let action = |args: &[&str]| {
            let cli = Cli::try_parse_from([["uxduck", "command"].as_slice(), args].concat())
                .unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let Some(Command::Send { action, .. }) = cli.command else {
                panic!("{cli:?}")
            };
            super::action_json(&action)
        };
        assert_eq!(
            action(&["set-home"]),
            serde_json::json!({ "type": "set-home" })
        );
        let there = action(&["set-home", "--lat", "-35.36", "--lon", "149.16"]);
        assert_eq!(there["place"]["longitudeDeg"], 149.16);
        // Looked up from the terrain before it is sent.
        assert!(there["place"]["altitudeAmslM"].is_null());
        let given = action(&[
            "set-ekf-origin",
            "--lat",
            "-35.36",
            "--lon",
            "149.16",
            "--altitude",
            "584",
        ]);
        assert_eq!(given["type"], "set-ekf-origin");
        assert_eq!(given["place"]["altitudeAmslM"], 584.0);
        // A height needs a place; a latitude needs a longitude.
        for args in [
            ["uxduck", "command", "set-home", "--altitude", "10"].as_slice(),
            ["uxduck", "command", "set-home", "--lat", "1"].as_slice(),
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
    }
}

#[cfg(test)]
mod logbook_arguments {
    use clap::Parser;

    use super::{AccountCommand, Cli, Command};

    #[test]
    fn logbook_is_the_name_and_flights_still_works() {
        for name in ["logbook", "flights"] {
            let cli = Cli::try_parse_from(["uxduck", "account", name, "--json"])
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let Some(Command::Account {
                command: AccountCommand::Flights { json },
            }) = cli.command
            else {
                panic!("{name}: {cli:?}")
            };
            assert!(json, "{name}");
        }
    }
}

#[cfg(test)]
mod help_text {
    use clap::CommandFactory;

    use super::Cli;

    /// A slice, plan or issue number (`S100`, `ADR-0039`, `FB-87`), which
    /// UXDuck Core users cannot look up.
    fn internal(word: &str) -> bool {
        let digits = |rest: &str| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit());
        let word = word.trim_end_matches(|c: char| c.is_ascii_lowercase());
        word.strip_prefix('S').is_some_and(digits)
            || ["ADR-", "FB-", "UXD-"]
                .iter()
                .any(|prefix| word.strip_prefix(prefix).is_some_and(digits))
    }

    fn visit(command: &mut clap::Command, path: &str, found: &mut Vec<String>) {
        let help = command.render_long_help().to_string();
        for line in help.lines() {
            let private = ["docs/plan", "docs/adr", "apps/"]
                .iter()
                .any(|path| line.contains(path));
            if private
                || line
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                    .any(internal)
            {
                found.push(format!("{path}: {}", line.trim()));
            }
        }
        for sub in command.get_subcommands_mut() {
            if sub.is_hide_set() {
                continue;
            }
            let name = format!("{path} {}", sub.get_name());
            visit(sub, &name, found);
        }
    }

    /// Help is what people see, and in UXDuck Core they cannot look up the
    /// private plan, its decisions or its paths.
    #[test]
    fn help_names_no_internal_slice_decision_issue_or_path() {
        let mut command = Cli::command();
        command.build();
        let mut found = Vec::new();
        visit(&mut command, "uxduck", &mut found);
        assert!(found.is_empty(), "{}", found.join("\n"));
    }
}
