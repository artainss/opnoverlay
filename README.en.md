# opnoverlay

[Русский](README.md) | **English**

A personal Windows overlay written in Rust for screen recording, instant replays, screenshots, and audio controls. Version 0.5 can be built locally; the project is under active development.

## Current status

The application supports screen and window recording with hardware H.264 encoding to MP4, instant replays stored in memory or temporary files, PNG/JPEG screenshots, settings, and a gallery. The gallery includes a video and image viewer, video trimming with the option to restore the original, and deletion to the recycle bin.

Compatibility with specific games, fullscreen modes, and anti-cheat systems requires further testing. Per-application audio capture and performance metrics also need more real-world testing. The optional FPS counter uses PresentMon; Windows tracing may require administrator privileges or membership in the Performance Log Users group.

Default shortcuts:

- **Alt+Z** — open or close the overlay.
- **Escape** — go back or close the overlay.
- **Alt+F1** — take a screenshot.
- **Alt+F9** — start or stop recording.
- **Alt+F10** — save an instant replay.

Shortcuts can be changed in settings. New configurations default to a 45-second replay, “Maximum detail” quality, source resolution, and 60 FPS. Previously saved settings remain unchanged.

## Building for Windows

Install Rust and Cargo at least as recent as the version specified in `Cargo.toml`, along with the Visual C++ build tools for MSVC.

```powershell
cargo build --release --locked
cargo test --locked
```

The executable is written to `target/release/opnoverlay.exe`. To use the FPS counter when running it manually, place `third-party/presentmon/PresentMon.exe` next to it. The installer and portable archive include PresentMon and its license. The installer is built separately with WiX; see `installer/build.ps1`.

## Origins and license

The project is based on [ObsLite](https://github.com/25Dark25/ObsLite) by Darkk. The original MIT license and attribution are preserved in `LICENSE`. The optional FPS counter uses [PresentMon](https://github.com/GameTechDev/PresentMon); its license and source link are in `third-party/presentmon`.

The original project's README is preserved in `UPSTREAM-README.md`; its measurements and descriptions are not verified claims about opnoverlay. The installer includes the project, PresentMon, and dependency licenses in `LICENSE.txt`, `PRESENTMON-LICENSE.txt`, and `THIRD-PARTY-LICENSES.html`. Regenerate the dependency license list before distributing a build after dependencies change.
