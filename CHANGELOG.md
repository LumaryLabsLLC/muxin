# Changelog

Notable changes, newest first, in Keep a Changelog sections. The notes
are the "What's new" text from the GitHub releases.

## [0.5.6] - 2026-10-16

### Fixed

- New cloud machines work where DigitalOcean has no basic sizes in some datacenters, New York among them. The dialog lists only cities with sizes, picks a nearby one, and always has a size ready.

## [0.5.5] - 2026-10-16

### Added

- Mac menus: Command-H hides Muxin, Option-Command-H hides the other apps, and Command-M minimizes. An Edit menu, a Window menu, and a Help menu with the docs, keyboard shortcuts, what's new, report a problem, and ask a question. The Muxin menu has Check for Updates.

## [0.5.4] - 2026-10-16

### Fixed

- The macOS 26 icon fills its tile. It had shown small, on a grey tile, in Spotlight, Finder and the Dock. If the old icon remains after updating, run `killall Dock`.

## [0.5.3] - 2026-10-16

### Added

- An update notice in the sidebar. About shows how to update the installed copy. Muxin checks GitHub when it opens and twice a day. Settings › General › Updates turns the check off. `muxin updates` checks from a terminal.
- On Linux Wayland (GNOME), the window has minimize, maximize and close, moves when the top bar is dragged, maximizes on a double-click, and resizes from its edges.

## [0.5.2] - 2026-10-16

### Added

- Linux packages for x86_64 and aarch64: a .deb, an .rpm, an Arch package and an AppImage. Each one installs what Muxin needs, adds it to the app menu and puts `muxin` on PATH.
- An AUR package, `yay -S muxin-bin`.

### Changed

- The AppImage runs with FUSE 3, so current Arch and Fedora do not need fuse2.
- Settings › Command line knows when a package or Homebrew installed the command. An AppImage's command runs the AppImage itself.

## [0.5.1] - 2026-10-15

### Changed

- Renamed Muxen to Muxin. The command is `muxin`, and `muxen` still works. Settings, machines, sessions and cloud sign-ins carry over. On a Mac, the old Muxen app goes to the Trash the first time Muxin opens.

### Added

- A Homebrew cask: `brew install --cask lumarylabsllc/tap/muxin`.

[0.5.6]: https://github.com/LumaryLabsLLC/muxin/releases/tag/v0.5.6
[0.5.5]: https://github.com/LumaryLabsLLC/muxin/releases/tag/v0.5.5
[0.5.4]: https://github.com/LumaryLabsLLC/muxin/releases/tag/v0.5.4
[0.5.3]: https://github.com/LumaryLabsLLC/muxin/releases/tag/v0.5.3
[0.5.2]: https://github.com/LumaryLabsLLC/muxin/releases/tag/v0.5.2
[0.5.1]: https://github.com/LumaryLabsLLC/muxin/releases/tag/v0.5.1
