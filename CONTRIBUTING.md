# Contributing to Muxin

Thanks for helping. Bug reports, fixes and new features are all welcome.

## Before you start

- **Bugs:** open an issue with what you did, what you expected and what
  happened. Say which OS the app runs on and which OS the machine runs on.
  `muxin status --json` and the app's log help.
- **Bigger changes:** open an issue first, so we can agree on the approach
  before you spend time on it.
- **Security problems:** don't open an issue; see [SECURITY.md](SECURITY.md).

## Build and test

You need a recent stable Rust (edition 2024), and:

- on macOS, Xcode (GPUI compiles Metal shaders);
- on Linux, GPUI's libraries (Ubuntu/Debian: `sudo apt install libxkbcommon-dev
  libxkbcommon-x11-dev libwayland-dev libvulkan-dev libfontconfig-dev
  libx11-xcb-dev libxcb1-dev`);
- on Windows, Visual Studio's C++ build tools and the Windows SDK.

```sh
cargo test --workspace     # what CI runs, on macOS, Linux and Windows
cargo run -- --demo        # the window, with simulated machines
cargo run -- status --demo --json
```

Demo mode (`--demo`) needs no machines and touches nothing real, so use it to
try changes to the window. For the agent on other platforms,
`scripts/build-agents.sh` builds every one (it needs zig and cargo-zigbuild).

Don't run a blanket `cargo update`: `Cargo.lock` pins `libc` for Linux (see the
README's "Develop" section).

## Pull requests

- Keep each one to one change, and match the code around it: its naming, its
  comments and how much it says.
- Add or update tests for what you change, and run `cargo test --workspace`.
- Write the commit message as a plain sentence that says what changed, with a
  few lines after it on why when that isn't obvious.
- Text people see (the window, errors, the docs) is short and plain: one idea
  per sentence, everyday words.

## License

Muxin is under the [Apache License 2.0](LICENSE). By sending a contribution,
you agree that it's under the same license (as its section 5 says), and that
you have the right to send it.
