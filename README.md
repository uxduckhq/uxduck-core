# UXDuck Core

The runtime behind [UXDuck](https://uxduck.com): vehicle connections, MAVLink,
telemetry, simulations, parameters, missions, plugins, recording, the agent,
and the command-line and MCP interfaces.

UXDuck Core is licensed under [Apache-2.0](LICENSE). See [NOTICE](NOTICE) and
[third-party notices](THIRD_PARTY.md).

## Build and run

Install [Rust through rustup](https://rustup.rs). The pinned toolchain installs
the WebAssembly target needed by the plugin runtime. On Debian or Ubuntu, install
`build-essential`, `pkg-config` and `libudev-dev` first.

```sh
cargo build --locked -p uxduck
cargo run --locked -p uxduck -- --help
```

Start a headless session on macOS or Linux with:

```sh
UXDUCK_NO_TRAY=1 cargo run --locked -p uxduck -- daemon
```

In PowerShell, set `$env:UXDUCK_NO_TRAY = '1'`, then run the same Cargo command
without the environment prefix. For development-server access with `--dev`, also
set `UXDUCK_TERRAIN_URL=none` unless you supply your own terrain verification key.

Default builds include gamepads and the plugin runtime, which Cargo builds from
the included `plugins/script-host` and `packages/sdk/src/runtime.js`. For a headless
build without those features:

```sh
cargo build --locked -p uxduck --no-default-features
```

This builds the background service and CLI. Connect through the CLI, MCP, or
your own client.

See `docs/site/` for user guides and `crates/uxduck-contracts/` for RPC types and
fixtures. No account or signing credentials are required to build locally;
hosted services and signed catalogues have their own access requirements.

## Verify

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo check --locked --target wasm32-unknown-unknown -p uxduck-web
```

Simulation tests use native ArduPilot builds. On macOS or Linux, run
`scripts/sitl.sh copter plane rover blimp` before running them. It downloads and
builds ArduPilot separately; its source and licensing remain upstream's.

## Contributions and synchronization

Development currently happens in the private UXDuck workspace. This repository
receives reviewed exports from that workspace. Open issues and pull requests here;
accepted changes are applied there first and arrive in the next export. Public
commits contain public files only, with no private Git history or commit messages.

`core-export.json` records the source commit, file hashes and executable flags.
The source commit identifies an export; it does not grant access to the private
repository. The private workspace publishes a one-way export after its independent
Core build and tests pass. Public main is managed by that sync; submit changes
through issues and pull requests rather than editing it directly.
