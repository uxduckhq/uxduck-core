//! `uxduck sim build`: the commit an ArduPilot checkout has checked out,
//! built as a simulator UXDuck starts (S258). It is built in a worktree of
//! its own with UXDuck's local-only network patch, so the checkout's files
//! and its `build/` folder are left as they are, and it reports the
//! checkout's commit, so the fork's parameters and messages apply to it.

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use uxduck_contracts::dto::link::{SimAutopilot, SimVehicle};

use super::client::CliError;

/// UXDuck's local-only network patch, as the catalogue applies it, and its
/// ports to older releases, each applied only where it fits exactly.
const LOCAL_ONLY_PATCHES: [(&str, &str); 2] = [
    (
        "current",
        include_str!("../../../../scripts/sitl-patches/local-only-network.patch"),
    ),
    (
        "4.5",
        include_str!("../../../../scripts/sitl-patches/older/local-only-network-4.5.patch"),
    ),
];

/// The frame table of a release before `vehicleinfo.json`, written out from
/// its `vehicleinfo.py`, as the catalogue does for those releases.
const FRAME_TABLE_FROM_PY: &str = "import json, sys
sys.path.insert(0, sys.argv[1])
from vehicleinfo import VehicleInfo
json.dump(VehicleInfo().options, open(sys.argv[2], 'w'), indent=4)
";

pub fn build(
    data_dir: &Path,
    checkout: &Path,
    vehicle: &str,
    allow_network: bool,
) -> Result<(), CliError> {
    if cfg!(windows) {
        return Err(CliError::Usage(
            "Building a simulator needs macOS or Linux. On Windows, send your SITL's MAVLink to UDP port 15010 instead.".into(),
        ));
    }
    let vehicle: SimVehicle = serde_json::from_value(Value::from(vehicle))
        .map_err(|e| CliError::Usage(format!("--vehicle: {e}")))?;
    let program = super::sim_build::program(vehicle, SimAutopilot::Ardupilot)?;
    let checkout = checkout
        .canonicalize()
        .map_err(|e| CliError::Usage(format!("{}: {e}", checkout.display())))?;
    if !checkout.join("waf").is_file() || !checkout.join("Tools/autotest/pysim").is_dir() {
        return Err(CliError::Usage(format!(
            "{} is not an ArduPilot checkout",
            checkout.display()
        )));
    }
    let commit = git(&checkout, &["rev-parse", "HEAD"])?;
    let short = &commit[..8.min(commit.len())];
    if !git(
        &checkout,
        &["status", "--porcelain", "--untracked-files=no"],
    )?
    .is_empty()
    {
        eprintln!(
            "Building commit {short}: changes not committed in {} are left out.",
            checkout.display()
        );
    }
    let sources = data_dir.join("simulators").join("source");
    std::fs::create_dir_all(&sources)?;
    let work = sources.join(short);
    let folder = data_dir.join("simulators").join("own").join(short);
    let built = remove_worktree(&checkout, &work).and_then(|()| {
        build_in(
            &checkout,
            &work,
            &sources,
            &commit,
            program,
            &folder,
            allow_network,
        )
    });
    let removed = remove_worktree(&checkout, &work);
    let patched = built?;
    removed?;
    let path = folder.join(program);
    let binary = crate::sim::probe(&path).map_err(CliError::Usage)?;
    if !patched {
        std::fs::write(
            folder.join(crate::sim::ALLOW_NETWORK),
            "Built with `uxduck sim build --allow-network`: no version of UXDuck's local-only network patch fits this release, so this simulator can be reached from your network while it runs.\n",
        )?;
        println!("{}", path.display());
        eprintln!(
            "Built {program} from {short} without UXDuck's local-only network patch, as none fits this release. While it runs, other computers on your network can connect to it, send it RC input and receive its state. Start it with `uxduck sim start --binary {}`, or choose it in New simulation › Advanced › Your own build.",
            path.display()
        );
        return Ok(());
    }
    let check = folder.join("check");
    std::fs::create_dir_all(&check)?;
    let checked = match network_address() {
        Some(network) => crate::sim::check_local_only(&binary, &check, network).map(|()| network),
        None => Err("this computer has no network address to check it from".to_owned()),
    };
    let _ = std::fs::remove_dir_all(&check);
    let network = checked.map_err(|error| {
        let _ = std::fs::remove_dir_all(&folder);
        CliError::Usage(format!(
            "The build is not kept: it could not be shown to stay on this computer, as {error}."
        ))
    })?;
    println!("{}", path.display());
    eprintln!(
        "Built {program} from {short} with UXDuck's local-only network patch; its port took connections on this computer and refused them from {network}. Start it with `uxduck sim start --binary {}`, or choose it in New simulation › Advanced › Your own build.",
        path.display()
    );
    Ok(())
}

/// Builds the program in a worktree of the commit and lays it out in
/// `folder` with its frame table, as the catalogue lays out an older
/// release.
fn build_in(
    checkout: &Path,
    work: &Path,
    sources: &Path,
    commit: &str,
    program: &str,
    folder: &Path,
    allow_network: bool,
) -> Result<bool, CliError> {
    git(
        checkout,
        &[
            "worktree",
            "add",
            "--detach",
            &work.display().to_string(),
            commit,
        ],
    )?;
    // Its submodules are the checkout's own, linked rather than fetched.
    let modules = git(
        checkout,
        &[
            "config",
            "-f",
            ".gitmodules",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ],
    )
    .unwrap_or_default();
    for path in modules
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
    {
        let source = checkout.join(path);
        if !std::fs::read_dir(&source).is_ok_and(|mut entries| entries.next().is_some()) {
            continue;
        }
        let linked = work.join(path);
        let _ = std::fs::remove_dir(&linked);
        symlink(&source, &linked)?;
    }
    let patched = apply(work, sources)?;
    if !patched && !allow_network {
        return Err(CliError::Usage(
            "No version of UXDuck's local-only network patch fits this ArduPilot release yet (there is one for 4.5 and for current releases). Run again with --allow-network to build it without one: while it runs, other computers on your network can reach it. Or send its MAVLink to UDP port 15010 instead.".into(),
        ));
    }
    let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".to_owned());
    let target = program.strip_prefix("ardu").unwrap_or(program);
    for args in [
        vec![
            "waf",
            "configure",
            "--board",
            "sitl",
            "--no-submodule-update",
        ],
        vec!["waf", target],
    ] {
        let status = waf(&python, work, &args).status()?;
        if !status.success() {
            return Err(CliError::Usage(format!(
                "`{python} {}` failed (see above)",
                args.join(" ")
            )));
        }
    }
    let _ = std::fs::remove_dir_all(folder);
    std::fs::create_dir_all(folder.join("defaults"))?;
    std::fs::copy(
        work.join("build/sitl/bin").join(program),
        folder.join(program),
    )?;
    frame_table(work, folder, &python, program)?;
    Ok(patched)
}

/// A waf step in the worktree. On macOS it links without chained fixups,
/// as `scripts/sitl.sh` does: Xcode's linker refuses ArduPilot's packed
/// version struct ("pointer not aligned") with them, and ignores the
/// `-ld_classic` ArduPilot asks for.
fn waf(python: &str, work: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(python);
    command.args(args).current_dir(work);
    if cfg!(target_os = "macos") {
        let ldflags = std::env::var("LDFLAGS").unwrap_or_default();
        command.env(
            "LDFLAGS",
            format!("{ldflags} -Wl,-no_fixup_chains").trim_start(),
        );
    }
    command
}

/// Applies the first of UXDuck's local-only patches that fits this source
/// exactly, or says none does. A patch placed loosely could compile with
/// its checks in the wrong places, so none is.
fn apply(work: &Path, sources: &Path) -> Result<bool, CliError> {
    for (release, patch) in LOCAL_ONLY_PATCHES {
        let file = sources.join(format!("local-only-network-{release}.patch"));
        std::fs::write(&file, patch)?;
        let file = file.display().to_string();
        if git(work, &["apply", "--check", &file]).is_ok() {
            git(work, &["apply", &file])?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// The frame table beside the program, from the source's
/// `vehicleinfo.json` or its `vehicleinfo.py`, with the default parameter
/// files of the program's frames under `defaults/`.
fn frame_table(work: &Path, folder: &Path, python: &str, program: &str) -> Result<(), CliError> {
    let pysim = work.join("Tools/autotest/pysim");
    let table_file = folder.join("vehicleinfo.json");
    if pysim.join("vehicleinfo.json").is_file() {
        std::fs::copy(pysim.join("vehicleinfo.json"), &table_file)?;
    } else {
        let status = Command::new(python)
            .args(["-c", FRAME_TABLE_FROM_PY])
            .arg(&pysim)
            .arg(&table_file)
            .status()?;
        if !status.success() {
            return Err(CliError::Usage(
                "cannot read the frame table from Tools/autotest/pysim/vehicleinfo.py".into(),
            ));
        }
    }
    let table: Value = serde_json::from_str(&std::fs::read_to_string(&table_file)?)
        .map_err(|e| CliError::Usage(format!("the frame table: {e}")))?;
    let target = format!("bin/{program}");
    let frames: Vec<&Value> = table
        .as_object()
        .into_iter()
        .flat_map(|t| t.values())
        .filter_map(|vehicle| vehicle.get("frames")?.as_object())
        .flat_map(|frames| frames.values())
        .filter(|frame| frame.get("waf_target").and_then(Value::as_str) == Some(target.as_str()))
        .collect();
    if frames.is_empty() {
        return Err(CliError::Usage(format!(
            "the frame table has no frames for {program}"
        )));
    }
    for frame in frames {
        let names = match frame.get("default_params_filename") {
            Some(Value::String(one)) => vec![one.as_str()],
            Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        for name in names {
            let source = work.join("Tools/autotest").join(name);
            if let Some(file) = Path::new(name).file_name()
                && source.is_file()
            {
                std::fs::copy(&source, folder.join("defaults").join(file))?;
            }
        }
    }
    Ok(())
}

/// Removes the worktree, unlinking the checkout's submodules from it
/// first so nothing of the checkout's is touched.
fn remove_worktree(checkout: &Path, work: &Path) -> Result<(), CliError> {
    if work.exists() {
        unlink_all(work)?;
        let _ = git(
            checkout,
            &["worktree", "remove", "--force", &work.display().to_string()],
        );
        if work.exists() {
            std::fs::remove_dir_all(work)?;
        }
    }
    let _ = git(checkout, &["worktree", "prune"]);
    Ok(())
}

fn unlink_all(folder: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            std::fs::remove_file(entry.path())?;
        } else if kind.is_dir() {
            unlink_all(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(source: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, link)
}

#[cfg(not(unix))]
fn symlink(_source: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("needs macOS or Linux"))
}

fn git(directory: &Path, args: &[&str]) -> Result<String, CliError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()?;
    if !output.status.success() {
        return Err(CliError::Usage(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// This computer's address on its network: the one a packet out would
/// leave from. Nothing is sent.
fn network_address() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("192.0.2.1", 9)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_patch_adds_the_local_only_switch_and_its_header() {
        for (release, patch) in LOCAL_ONLY_PATCHES {
            assert!(
                patch.contains("+++ b/libraries/AP_HAL/utility/SITL_Network.h"),
                "{release}"
            );
            assert!(patch.contains("+        {\"local-only\","), "{release}");
            assert!(
                patch.contains("+            SITLNetwork::local_only() = true;"),
                "{release}"
            );
        }
    }

    #[test]
    fn waf_links_without_chained_fixups_on_macos_only() {
        let step = waf("python3", Path::new("."), &["waf", "configure"]);
        let ldflags = step
            .get_envs()
            .find(|(name, _)| *name == "LDFLAGS")
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned());
        if cfg!(target_os = "macos") {
            let ldflags = ldflags.expect("LDFLAGS for Xcode's linker");
            assert!(
                ldflags
                    .split_whitespace()
                    .any(|flag| flag == "-Wl,-no_fixup_chains"),
                "{ldflags}"
            );
        } else {
            assert_eq!(ldflags, None, "the toolchain's own flags elsewhere");
        }
    }

    #[test]
    fn a_folder_that_is_not_an_ardupilot_checkout_is_refused() {
        let folder = tempfile::tempdir().unwrap();
        let error = build(folder.path(), folder.path(), "multicopter", false).unwrap_err();
        let expected = if cfg!(windows) {
            "Building a simulator needs macOS or Linux"
        } else {
            "is not an ArduPilot checkout"
        };
        assert!(error.to_string().contains(expected), "{error}");
    }
}
