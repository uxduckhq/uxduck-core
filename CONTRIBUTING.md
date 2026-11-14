# Contributing to UXDuck Core

Thank you for helping. Bug reports, ideas, documentation fixes and pull
requests are all welcome.

## How a pull request lands

UXDuck Core is developed in UXDuck's main repository, alongside the parts of
UXDuck that are not open source, and published here. So pull requests here are
not merged with GitHub's merge button. This is what happens instead:

1. A maintainer reviews your pull request here, and CI runs on it as usual.
2. When it is ready, they import it into the main repository with you as the
   commit's author. A comment on your pull request says when that happens.
3. Once it is merged there, the next publish to this repository, at least once
   a day, adds it to `main` as a commit credited to you. Your pull request then
   closes with a link to that commit.

The commit keeps your GitHub account as its author, so it counts on your
profile. Its title may be shortened, and a maintainer may make small changes
while importing it. Each commit on `main` here corresponds to one merged
change in the main repository.

## Before you open a pull request

- For anything bigger than a small fix, open an
  [issue](https://github.com/uxduckhq/uxduck-core/issues) first, so we can agree
  on the approach before you spend time on it.
- Keep a pull request to one change. Add a test for a fix where you can.
- Every file in this repository can be changed except `core-export.json`, which
  the publisher writes. New files can go in any directory that already has files.
- Run the checks CI runs:

  ```sh
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --locked -- -D warnings
  cargo test --workspace --locked
  ```

  The [CI workflow](.github/workflows/ci.yml) lists the rest, including the
  WebAssembly and headless builds.

## Reporting a security issue

Please do not open a public issue. [SECURITY.md](SECURITY.md) explains how to
report it privately.

## License

UXDuck Core is licensed under the [Apache License 2.0](LICENSE). By opening a
pull request, you agree that your contribution is licensed under it too.

## Code of conduct

Everyone taking part is expected to follow the
[code of conduct](CODE_OF_CONDUCT.md).
