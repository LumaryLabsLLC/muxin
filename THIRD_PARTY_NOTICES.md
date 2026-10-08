# Third-party notices

Muxen includes the following third-party components. Their licenses
must ship with every copy of Muxen.

| Component | License | Notice |
|---|---|---|
| Geist and Geist Mono fonts (Vercel) | SIL Open Font License 1.1 | `assets/fonts/OFL.txt` |
| Lucide icons | ISC | `assets/icons/LICENSE.txt` |
| GPUI (Zed Industries) | Apache-2.0 | <https://github.com/zed-industries/zed> |
| alacritty_terminal (terminal emulation, in the app and the agent) | Apache-2.0 | <https://github.com/alacritty/alacritty> |
| portable-pty (WezTerm; terminals in `muxen-agent`) | MIT | <https://github.com/wezterm/wezterm> |
| Agent logos (`assets/icons/agent-*.svg`), SVGs from LobeHub Icons | MIT | <https://github.com/lobehub/lobe-icons> |
| Rust crates listed in `Cargo.lock` | MIT and/or Apache-2.0 (mostly), see each crate | Generate the full list before each release, e.g. with `cargo about generate` |

The agent logos are trademarks of their owners (Anthropic, OpenAI, Google,
SST, Sourcegraph). Muxen shows them only to identify which CLI a session
runs; check each owner's brand guidelines before using them in marketing.

Before shipping a release, regenerate the crate list and include the license
texts in the app bundle (for example `Muxen.app/Contents/Resources/`).
