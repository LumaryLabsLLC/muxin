# Third-party notices

OpenRunner includes the following third-party components. Their licenses
must ship with every copy of OpenRunner.

| Component | License | Notice |
|---|---|---|
| Geist and Geist Mono fonts (Vercel) | SIL Open Font License 1.1 | `assets/fonts/OFL.txt` |
| Lucide icons | ISC | `assets/icons/LICENSE.txt` |
| GPUI (Zed Industries) | Apache-2.0 | <https://github.com/zed-industries/zed> |
| Rust crates listed in `Cargo.lock` | MIT and/or Apache-2.0 (mostly), see each crate | Generate the full list before each release, e.g. with `cargo about generate` |

Before shipping a release, regenerate the crate list and include the license
texts in the app bundle (for example `OpenRunner.app/Contents/Resources/`).
