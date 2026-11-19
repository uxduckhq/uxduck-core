//! `uxduck ssh <vehicle> [computer] [-- command…]` (S422, ADR-0065 T5): the
//! terminal this is run in, attached to a terminal the daemon on this
//! computer opens on a vehicle's companion computer. The daemon signs in with
//! the pinned host key and the key a check uses. This program holds neither.
//!
//! `crossterm` (T3) puts the local terminal in raw mode and reads its size,
//! so every key goes to the computer and the remote terminal follows the
//! window. After `--`, one command runs with no terminal and this exits with
//! its exit status.

use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::Path;
use std::process::ExitCode;

use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use uxduck_contracts::dto::terminal::{TerminalEnd, TerminalEnded};

use super::client::{CliError, LocalClient};
use super::vehicle::vehicle_text;

#[derive(Debug, clap::Args)]
pub struct SshArgs {
    /// The vehicle, by its name in any case or its id. The start of its
    /// name is enough when no other vehicle's starts the same.
    vehicle: String,
    /// The companion computer, by its name or id, as the vehicle is named.
    /// Leave it out when the vehicle has one.
    computer: Option<String>,
    /// After `--`, one command to run with no terminal and no input. Its
    /// output is printed and its exit status is this program's.
    /// `uxduck ssh buggy camera -- journalctl -u mediamtx -n 50`
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<String>,
}

/// What `ssh` itself exits with when it could not run or lost the computer,
/// so scripts read this as they read `ssh`.
const COULD_NOT: u8 = 255;

/// A wrong name or a wrong way of calling, as the rest of the CLI exits.
const USAGE: u8 = 2;

/// How many reads of this terminal's keys wait for the daemon. When it is
/// full the keyboard is read no more until the daemon takes one, so nothing
/// typed is dropped.
const TYPED: usize = 32;

/// The size a command is given when this is not run in a terminal: the size
/// a terminal has when nothing says otherwise. A command has no terminal on
/// the computer, so nothing there reads it.
const NO_WINDOW: (u16, u16) = (80, 24);

/// Something with a name and an id that a word on the command line picks.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Named {
    id: String,
    name: String,
    /// Other exact words it answers to: a vehicle's earlier ids.
    also: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Picked<'a> {
    One(&'a Named),
    None,
    Several(Vec<&'a Named>),
}

/// The one `wanted` names: by id, else by its whole name in any case, else
/// by the start of its name in any case. Several that fit equally well are
/// all returned, so the person can say which.
fn pick<'a>(among: &'a [Named], wanted: &str) -> Picked<'a> {
    let lower = wanted.to_lowercase();
    let rules: [&dyn Fn(&Named) -> bool; 3] = [
        &|item| item.id == wanted || item.also.iter().any(|also| also == wanted),
        &|item| item.name.to_lowercase() == lower,
        &|item| item.name.to_lowercase().starts_with(&lower),
    ];
    for rule in rules {
        let fit: Vec<&Named> = among.iter().filter(|item| rule(item)).collect();
        match fit.len() {
            0 => {}
            1 => return Picked::One(fit[0]),
            _ => return Picked::Several(fit),
        }
    }
    Picked::None
}

/// "Buggy", "Buggy and Rover", "Buggy, Rover and Skiff".
fn listed(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [most @ .., last] => format!("{} and {last}", most.join(", ")),
    }
}

/// One line for each, with its id, which tells two of one name apart.
fn lines(among: &[&Named]) -> String {
    among
        .iter()
        .map(|item| format!("  {}  ({})", item.name, item.id))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The computer to open a terminal on, from `vehicle/list`'s answer and the
/// words given: its id and "Buggy › Camera Pi".
fn choose(
    vehicles: &[Value],
    vehicle: &str,
    computer: Option<&str>,
) -> Result<(String, String), String> {
    let named = |value: &Value, name: String| Named {
        id: value["id"].as_str().unwrap_or_default().to_owned(),
        name,
        also: value["aliases"]
            .as_array()
            .map(|aliases| {
                aliases
                    .iter()
                    .filter_map(|alias| alias.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
    };
    let all: Vec<Named> = vehicles
        .iter()
        .map(|value| named(value, vehicle_text(value)))
        .collect();
    let chosen = match pick(&all, vehicle) {
        Picked::One(chosen) => chosen,
        Picked::None if all.is_empty() => {
            return Err("UXDuck has no vehicles yet.".to_owned());
        }
        Picked::None => {
            let has: Vec<&str> = all.iter().map(|v| v.name.as_str()).collect();
            return Err(format!(
                "No vehicle is called {vehicle}. UXDuck has {}.",
                listed(&has)
            ));
        }
        Picked::Several(fit) => {
            return Err(format!(
                "{vehicle} fits several vehicles:\n{}\nSay more of its name, or use its id.",
                lines(&fit)
            ));
        }
    };
    let computers: Vec<Named> = vehicles
        .iter()
        .find(|value| value["id"] == chosen.id.as_str())
        .and_then(|value| value["companions"].as_array())
        .map(|computers| {
            computers
                .iter()
                .map(|value| named(value, value["name"].as_str().unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    let every: Vec<&Named> = computers.iter().collect();
    let picked = match computer {
        Some(wanted) => match pick(&computers, wanted) {
            Picked::One(picked) => picked,
            Picked::None if computers.is_empty() => {
                return Err(format!(
                    "{} has no companion computers. Add one on its page in UXDuck.",
                    chosen.name
                ));
            }
            Picked::None => {
                let has: Vec<&str> = computers.iter().map(|c| c.name.as_str()).collect();
                return Err(format!(
                    "{} has no companion computer called {wanted}. It has {}.",
                    chosen.name,
                    listed(&has)
                ));
            }
            Picked::Several(fit) => {
                return Err(format!(
                    "{wanted} fits several of {}'s companion computers:\n{}\nSay more of its name, or use its id.",
                    chosen.name,
                    lines(&fit)
                ));
            }
        },
        None => match every[..] {
            [only] => only,
            [] => {
                return Err(format!(
                    "{} has no companion computers. Add one on its page in UXDuck.",
                    chosen.name
                ));
            }
            _ => {
                return Err(format!(
                    "{} has several companion computers:\n{}\nName one, as in: uxduck ssh {vehicle} <computer>",
                    chosen.name,
                    lines(&every)
                ));
            }
        },
    };
    Ok((
        picked.id.clone(),
        format!("{} \u{203a} {}", chosen.name, picked.name),
    ))
}

/// This terminal in raw mode until dropped, on every way out: a return, an
/// error and a panic, which leaves raw mode before it prints.
struct Raw;

impl Raw {
    fn on() -> Result<Self, String> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = crossterm::terminal::disable_raw_mode();
            previous(info);
        }));
        crossterm::terminal::enable_raw_mode()
            .map(|()| Self)
            .map_err(|e| format!("cannot put this terminal in raw mode: {e}"))
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Resolves when this program is told to stop from outside. In raw mode
/// Ctrl-C is a key like any other and goes to the computer.
async fn stopped() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut hup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = hup.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close};
        let (Ok(mut close), Ok(mut brk)) = (ctrl_close(), ctrl_break()) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = close.recv() => {}
            _ = brk.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
}

/// Resolves each time this terminal's window may have changed size.
struct Resized {
    #[cfg(unix)]
    signal: Option<tokio::signal::unix::Signal>,
    /// Windows has no signal for a resized console, so its size is read
    /// this often while a terminal is attached.
    #[cfg(not(unix))]
    poll: tokio::time::Interval,
}

impl Resized {
    fn new() -> Self {
        Self {
            #[cfg(unix)]
            signal: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                .ok(),
            #[cfg(not(unix))]
            poll: tokio::time::interval(std::time::Duration::from_millis(250)),
        }
    }

    async fn next(&mut self) {
        #[cfg(unix)]
        match &mut self.signal {
            Some(signal) => {
                signal.recv().await;
            }
            None => std::future::pending().await,
        }
        #[cfg(not(unix))]
        self.poll.tick().await;
    }
}

/// `uxduck ssh`. Exits with the remote exit status.
pub async fn run(data_dir: &Path, args: SshArgs) -> ExitCode {
    match attach(data_dir, args).await {
        Ok(status) => ExitCode::from(status),
        Err(CliError::Usage(why)) => {
            eprintln!("uxduck ssh: {why}");
            ExitCode::from(USAGE)
        }
        // The daemon's refusal in its own words, such as a computer never
        // checked or no `ssh` on this computer.
        Err(CliError::Api(error)) => {
            eprintln!(
                "uxduck ssh: {}",
                error["message"].as_str().unwrap_or("UXDuck refused")
            );
            ExitCode::from(COULD_NOT)
        }
        Err(error) => {
            eprintln!("uxduck ssh: {error}");
            ExitCode::from(COULD_NOT)
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one session from its names to its exit status"
)]
async fn attach(data_dir: &Path, args: SshArgs) -> Result<u8, CliError> {
    let interactive = args.command.is_empty();
    if interactive && !std::io::stdin().is_terminal() {
        return Err(CliError::Usage(format!(
            "this is not run in a terminal, so there is nothing to type into. To run one command, put it after --, as in: uxduck ssh {} {}-- uname -m",
            args.vehicle,
            args.computer
                .as_ref()
                .map(|computer| format!("{computer} "))
                .unwrap_or_default()
        )));
    }
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let vehicles = client.call("vehicle/list", json!({})).await?;
    let vehicles = vehicles["vehicles"].as_array().cloned().unwrap_or_default();
    let (computer_id, _) =
        choose(&vehicles, &args.vehicle, args.computer.as_deref()).map_err(CliError::Usage)?;
    let size = || {
        crossterm::terminal::size()
            .ok()
            .filter(|(cols, rows)| *cols > 0 && *rows > 0)
    };
    let mut last = size().unwrap_or(NO_WINDOW);
    let mut open = json!({ "computerId": computer_id, "cols": last.0, "rows": last.1 });
    if !interactive {
        // As `ssh host words…` does: the words with a space between them.
        open["command"] = json!(args.command.join(" "));
    }
    let opened = client.call("terminal/open", open).await?;
    let (Some(terminal_id), Some(ticket)) =
        (opened["terminalId"].as_str(), opened["ticket"].as_str())
    else {
        return Err(CliError::Protocol(
            "terminal/open returned no ticket".into(),
        ));
    };
    let terminal_id = terminal_id.to_owned();
    let Some(info) = crate::paths::read_info(data_dir) else {
        return Err(CliError::NoDaemon(data_dir.display().to_string()));
    };
    let url = format!("ws://127.0.0.1:{}/terminal?ticket={ticket}", info.port);
    let (socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|error| {
            CliError::Protocol(format!(
                "UXDuck did not open the terminal's socket: {error}"
            ))
        })?;
    let (mut sink, mut stream) = socket.split();

    let raw = if interactive {
        Some(Raw::on().map_err(CliError::Usage)?)
    } else {
        None
    };
    let (typed_tx, mut typed) = tokio::sync::mpsc::channel::<Vec<u8>>(TYPED);
    if interactive {
        // A thread of its own: the process ends without waiting for a key.
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buffer = [0_u8; 4096];
            while let Ok(read) = stdin.read(&mut buffer) {
                if read == 0 || typed_tx.blocking_send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
        });
    } else {
        drop(typed_tx);
    }
    let mut resized = Resized::new();
    // Listening from here on, so a signal between two turns of the loop is
    // not missed.
    let stopped = stopped();
    tokio::pin!(stopped);
    let mut stdout = std::io::stdout();
    let mut typing = interactive;
    let ended: Result<TerminalEnded, String> = loop {
        tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(Message::Binary(bytes))) => {
                    if stdout.write_all(&bytes).and_then(|()| stdout.flush()).is_err() {
                        break Err("this program's output closed".to_owned());
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if let Ok(ended) = serde_json::from_str::<TerminalEnded>(text.as_str()) {
                        break Ok(ended);
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => {
                    break Err("the connection to UXDuck was lost".to_owned());
                }
                Some(Ok(_)) => {}
            },
            bytes = typed.recv(), if typing => match bytes {
                Some(bytes) => {
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        break Err("the connection to UXDuck was lost".to_owned());
                    }
                }
                None => typing = false,
            },
            () = resized.next(), if interactive => {
                if let Some(now) = size().filter(|now| *now != last) {
                    last = now;
                    client
                        .send_request(
                            "terminal/resize",
                            json!({ "terminalId": terminal_id, "cols": now.0, "rows": now.1 }),
                        )
                        .await?;
                }
            }
            // Answers to resizes are read and left. A daemon that has gone
            // has hung the terminal up (T13).
            message = client.read() => {
                if !matches!(message, Ok(Some(_))) {
                    break Err("the connection to UXDuck was lost".to_owned());
                }
            }
            () = &mut stopped => break Err("stopped".to_owned()),
        }
    };
    // Closing the socket hangs the terminal up, whatever happens next.
    let _ = sink.close().await;
    drop(raw);
    match ended {
        Ok(ended) => {
            if let Some(problem) = &ended.problem {
                eprintln!("uxduck ssh: {problem}");
            }
            match ended.how {
                TerminalEnd::Exited => Ok(ended
                    .exit_status
                    .and_then(|status| u8::try_from(status).ok())
                    .unwrap_or(COULD_NOT)),
                // Why is in `problem`, printed above.
                TerminalEnd::Failed
                | TerminalEnd::EndedByOperator
                | TerminalEnd::CapabilityRemoved
                // Only a command an agent runs has a deadline.
                | TerminalEnd::Deadline => Ok(COULD_NOT),
                TerminalEnd::Closed => {
                    eprintln!("uxduck ssh: the terminal was ended from UXDuck");
                    Ok(COULD_NOT)
                }
                TerminalEnd::Stopped => {
                    eprintln!("uxduck ssh: UXDuck stopped");
                    Ok(COULD_NOT)
                }
                TerminalEnd::HungUp | TerminalEnd::Unclaimed => {
                    eprintln!("uxduck ssh: the terminal was hung up");
                    Ok(COULD_NOT)
                }
            }
        }
        Err(why) => {
            eprintln!("uxduck ssh: {why}");
            Ok(COULD_NOT)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vehicles() -> Vec<Value> {
        vec![
            json!({ "id": "v1", "name": "Buggy", "aliases": ["mav:1"], "companions": [
                { "id": "cc1", "name": "Camera Pi" },
                { "id": "cc2", "name": "Vision" },
            ] }),
            json!({ "id": "v2", "name": "Boat", "companions": [
                { "id": "cc3", "name": "Jetson" },
            ] }),
            json!({ "id": "v3", "name": "Skiff" }),
            json!({ "id": "v4", "name": "Rover", "companions": [
                { "id": "cc5", "name": "Camera" },
                { "id": "cc6", "name": "Camera" },
                { "id": "cc7", "name": "camera two" },
            ] }),
        ]
    }

    #[test]
    fn one_name_picks_its_computer() {
        let all = vehicles();
        // The whole name in any case, the start of a name, an id and an
        // earlier id each pick one.
        for (vehicle, computer, id, text) in [
            ("buggy", Some("camera"), "cc1", "Buggy \u{203a} Camera Pi"),
            ("BUGGY", Some("VISION"), "cc2", "Buggy \u{203a} Vision"),
            ("bu", Some("v"), "cc2", "Buggy \u{203a} Vision"),
            ("v1", Some("cc1"), "cc1", "Buggy \u{203a} Camera Pi"),
            ("mav:1", Some("cc2"), "cc2", "Buggy \u{203a} Vision"),
            // The computer is left out when the vehicle has one.
            ("boat", None, "cc3", "Boat \u{203a} Jetson"),
            // An id picks one of two with the same name.
            ("rover", Some("cc6"), "cc6", "Rover \u{203a} Camera"),
        ] {
            assert_eq!(
                choose(&all, vehicle, computer),
                Ok((id.to_owned(), text.to_owned())),
                "{vehicle} {computer:?}"
            );
        }
        // A whole name wins over the start of a longer one.
        let longer = vec![
            json!({ "id": "a", "name": "Buggy", "companions": [{ "id": "c1", "name": "Pi" }] }),
            json!({ "id": "b", "name": "Buggy Two", "companions": [{ "id": "c2", "name": "Pi" }] }),
        ];
        assert_eq!(choose(&longer, "buggy", None).unwrap().0, "c1");
    }

    #[test]
    fn no_name_says_which_names_there_are() {
        let all = vehicles();
        assert_eq!(
            choose(&all, "bugy", None).unwrap_err(),
            "No vehicle is called bugy. UXDuck has Buggy, Boat, Skiff and Rover."
        );
        assert_eq!(
            choose(&[], "buggy", None).unwrap_err(),
            "UXDuck has no vehicles yet."
        );
        assert_eq!(
            choose(&all, "buggy", Some("jetson")).unwrap_err(),
            "Buggy has no companion computer called jetson. It has Camera Pi and Vision."
        );
        for computer in [None, Some("camera")] {
            assert_eq!(
                choose(&all, "skiff", computer).unwrap_err(),
                "Skiff has no companion computers. Add one on its page in UXDuck."
            );
        }
    }

    #[test]
    fn several_names_are_listed() {
        let all = vehicles();
        assert_eq!(
            choose(&all, "b", None).unwrap_err(),
            "b fits several vehicles:\n  Buggy  (v1)\n  Boat  (v2)\nSay more of its name, or use its id."
        );
        // The computer left out, with two to choose from.
        assert_eq!(
            choose(&all, "buggy", None).unwrap_err(),
            "Buggy has several companion computers:\n  Camera Pi  (cc1)\n  Vision  (cc2)\nName one, as in: uxduck ssh buggy <computer>"
        );
        // Two with one whole name: their ids tell them apart.
        assert_eq!(
            choose(&all, "rover", Some("camera")).unwrap_err(),
            "camera fits several of Rover's companion computers:\n  Camera  (cc5)\n  Camera  (cc6)\nSay more of its name, or use its id."
        );
        assert_eq!(
            choose(&all, "rover", Some("cam")).unwrap_err(),
            "cam fits several of Rover's companion computers:\n  Camera  (cc5)\n  Camera  (cc6)\n  camera two  (cc7)\nSay more of its name, or use its id."
        );
    }
}
