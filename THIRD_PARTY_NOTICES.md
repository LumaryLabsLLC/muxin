# Third-party notices

Muxin includes the following third-party components. Their licenses
must ship with every copy of Muxin.

| Component | License | Notice |
|---|---|---|
| Geist and Geist Mono fonts (Vercel) | SIL Open Font License 1.1 | `assets/fonts/OFL.txt` |
| Lucide icons | ISC | `assets/icons/LICENSE.txt` |
| Phosphor icons (`assets/icons/pf-*.svg`) | MIT | `assets/icons/PHOSPHOR-LICENSE.txt` |
| Hugeicons (`assets/icons/hg-*.svg`) | MIT | `assets/icons/HUGEICONS-LICENSE.txt` |
| Solar icons by 480 Design (`assets/icons/so-*.svg`) | CC BY 4.0 | `assets/icons/SOLAR-NOTICE.txt` |
| MingCute icons (`assets/icons/mc-*.svg`) | Apache-2.0 | `assets/icons/MINGCUTE-LICENSE.txt` |
| Fluent UI System Icons, Microsoft (`assets/icons/fl-*.svg`) | MIT | `assets/icons/FLUENT-LICENSE.txt` |
| GPUI (Zed Industries) | Apache-2.0 | <https://github.com/zed-industries/zed> |
| alacritty_terminal (terminal emulation, in the app and the agent) | Apache-2.0 | <https://github.com/alacritty/alacritty> |
| portable-pty (WezTerm; terminals in `muxen-agent`) | MIT | <https://github.com/wezterm/wezterm> |
| Agent logos (`assets/icons/agent-*.svg`), SVGs from LobeHub Icons | MIT | <https://github.com/lobehub/lobe-icons> |
| Pi logo (`assets/icons/agent-pi.svg`), from Pi's press kit (Earendil) | trademark of its owner | <https://pi.dev/press-kit> |
| DigitalOcean logo (`assets/icons/brand-digitalocean.svg`), SVG from Simple Icons | CC0-1.0 (the SVG); trademark of DigitalOcean | <https://simpleicons.org> |
| xterm.js 5.5.0 (the share server's join page, `share-server/web/vendor/`) | MIT | `share-server/web/vendor/XTERM-LICENSE` |
| webrtc-rs (`webrtc`, `rtc`: sharing sessions peer to peer) | MIT or Apache-2.0 | <https://github.com/webrtc-rs/webrtc> |
| Rust crates listed in `Cargo.lock` | MIT and/or Apache-2.0 (mostly), see each crate | Generate the full list before each release, e.g. with `cargo about generate` |

The agent logos are trademarks of their owners (Anthropic, OpenAI, Google,
SST, Sourcegraph, Earendil), as is DigitalOcean's. Muxin shows them only to identify which CLI a session
runs, or that a machine is a DigitalOcean droplet; check each owner's brand guidelines before using them in marketing.

Before shipping a release, regenerate the crate list and include the license
texts in the app bundle (for example `Muxin.app/Contents/Resources/`).
