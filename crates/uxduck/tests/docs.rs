//! The docs name only commands and flags `uxduck` has (S47, ADR-0019 W7):
//! every `uxduck …` line in a code block of `docs/site/*.md` is checked
//! against `uxduck … --help`, so renaming a flag fails here until the docs
//! say the new name.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "a test")]

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_uxduck");

/// `uxduck <path> --help`, or `None` when there is no such command.
fn help(path: &[String]) -> Option<String> {
    let output = Command::new(BIN)
        .args(path)
        .arg("--help")
        .output()
        .expect("run uxduck");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The subcommands a help text lists under "Commands:".
fn subcommands(help: &str) -> Vec<String> {
    help.lines()
        .skip_while(|line| !line.starts_with("Commands:"))
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
        .collect()
}

/// A command line's words, quotes kept together.
fn words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    for c in line.chars() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            (_, c) => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Every `uxduck …` line in the docs' code blocks, with where it is.
fn command_lines() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/site");
    let mut lines = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("docs/site") {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let mut in_code = false;
        for line in text.lines() {
            if line.trim_start().starts_with("```") {
                in_code = !in_code;
                continue;
            }
            let line = line.trim();
            if in_code && (line.starts_with("uxduck ") || line == "uxduck") {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                lines.push((name, line.to_owned()));
            }
        }
    }
    lines
}

#[test]
fn every_command_and_flag_the_docs_name_exists() {
    let lines = command_lines();
    assert!(!lines.is_empty(), "the docs name no commands");
    for (file, line) in lines {
        let mut path: Vec<String> = Vec::new();
        let mut current = help(&path).expect("uxduck --help");
        let mut flags = Vec::new();
        let mut positional = false;
        for word in words(&line).into_iter().skip(1) {
            if word.starts_with("--") {
                flags.push(word.split('=').next().unwrap().to_owned());
            } else if !positional && flags.is_empty() && subcommands(&current).contains(&word) {
                path.push(word);
                current = help(&path)
                    .unwrap_or_else(|| panic!("{file}: `{line}`: no `uxduck {}`", path.join(" ")));
            } else {
                positional = true;
            }
        }
        for flag in flags {
            assert!(
                current.contains(&flag),
                "{file}: `{line}`: `uxduck {}` has no {flag}",
                path.join(" ")
            );
        }
    }
}

/// `--help` checks CLI flags, but cannot catch a wrong RPC field such as
/// `vehicleId` where `telemetry/get` requires `id`.
#[test]
fn rpc_examples_have_valid_methods_and_parameters() {
    for (file, line) in command_lines() {
        let tokens = words(&line);
        if tokens.get(1).map(String::as_str) != Some("rpc")
            || tokens.get(2).map(String::as_str) != Some("call")
        {
            continue;
        }
        let method = tokens.get(3).expect("RPC method");
        let params = if let Some(index) = tokens.iter().position(|word| word == "--json") {
            serde_json::from_str::<serde_json::Value>(tokens.get(index + 1).expect("JSON argument"))
                .unwrap_or_else(|error| panic!("{file}: `{line}`: invalid JSON: {error}"))
        } else {
            serde_json::json!({})
        };
        serde_json::from_value::<uxduck_contracts::rpc::ClientRequest>(serde_json::json!({
            "method": method,
            "params": params,
        }))
        .unwrap_or_else(|error| panic!("{file}: `{line}`: invalid RPC parameters: {error}"));
    }
}

#[test]
fn the_check_reads_commands_as_written() {
    assert_eq!(
        words(r#"uxduck vehicle note "copter 1" "Replaced motor 3" --when yesterday"#),
        [
            "uxduck",
            "vehicle",
            "note",
            "copter 1",
            "Replaced motor 3",
            "--when",
            "yesterday"
        ]
    );
    assert_eq!(
        subcommands("Usage\n\nCommands:\n  daemon  Run it\n  mcp     Serve\n\nOptions:\n"),
        ["daemon", "mcp"]
    );
}
