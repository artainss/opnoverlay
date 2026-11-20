# opnoverlay

[Русский](README.md) | **English**

A Windows game overlay written in Rust. Save the last few seconds, record video and audio, take screenshots, and review or trim clips directly in the overlay.

## Installation

Download the MSI or portable ZIP from the [release page](https://github.com/artainss/opnoverlay/releases/latest). The MSI upgrades an existing installation and keeps your settings and gallery. For portable use, extract all files to a writable folder and launch `opnoverlay.exe`. SHA-256 checksums are provided for both packages.

- **Alt+Z** opens and closes the overlay. **Escape** goes back or closes it.
- **Alt+F10** saves a replay. **Alt+F9** starts or stops recording. **Alt+F1** takes a screenshot.
- **Settings → Video** selects a display or window. If the saved window is closed, select a source before recording.
- **Settings → Recording** controls quality, duration and the buffer limit. **Settings → Audio** controls microphone, desktop audio and separate tracks.

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

In opnoverlay 0.8, **Settings → Recording** shows bitrate and estimates recording and replay size. The light profile uses up to 720p at 30 FPS. The gallery searches file names, the player can seek in five-second steps, and the editor can set trim points from the playhead or adjust them in 10 ms steps. Trim exports follow frame boundaries.

In 0.9, **Settings → Recording** saves named game profiles with capture source, quality, FPS, replay and microphone settings. Press OK to apply a profile. The disk replay limit can be set between 128 and 16,384 MiB. Replays can be shorter when the buffer reaches this limit.

On Windows, recordings default to a single mix. **Separate audio tracks** puts the mix first, followed by desktop or selected application audio and the microphone. Trimming preserves the tracks. First-run setup offers a three-second video test without audio and a copyable result with the capture method and encoder. The test file is automatically removed.

“Selected applications only” captures the chosen processes and their children. The microphone is independent. When an application restarts, you can confirm its new process in the mixer and keep the previous volume and mute state.

The main panel opens on the active window's display. For full-display capture, **Preserve display stretching** fills a frame with the desktop aspect ratio when the display temporarily changes resolution. Continuous capture stops when recording, replay, and preview are off. Windows controls the capture border during capture.

Video settings offer DXGI for full-display capture with the cursor disabled. Window capture and temporary display resolutions use Windows Graphics Capture. If the display mode changes during recording, the current clip is saved and capture resumes on the same display. DXGI was tested on an RTX 3050 with one monitor. Other GPUs, games and multiple monitors require separate verification.

## Building for Windows

Install Rust and Cargo at least as recent as the version specified in `Cargo.toml`, along with the Visual C++ build tools for MSVC.

```powershell
cargo build --release --locked
cargo test --locked
```

The executable is written to `target/release/opnoverlay.exe`. To use the FPS counter when running it manually, place `third-party/presentmon/PresentMon.exe` next to it. The installer and portable archive include PresentMon and its license. The installer is built separately with WiX; see `installer/build.ps1`.

For local development on Windows with Python 3.11+, use `python tools/check.py quick` for tests with a compact cache, `build` to compile, and `smoke` to briefly capture the current screen and verify the recording, replay, and PNG. `media` validates an existing MP4. Test media is deleted after the check. `release` uses WiX and cargo-about to build the MSI, portable ZIP and SHA-256 checksums. Run `python tools/check.py --help` for options.

To run the portable build, extract the ZIP to a writable folder, exit other opnoverlay instances and run `opnoverlay.exe`. The `opnoverlay.portable` marker enables local settings in `Data` and the default media folder `Media`. Windows startup is disabled for the portable build.

## Origins and license

The project is based on [ObsLite](https://github.com/25Dark25/ObsLite) by Darkk. The original MIT license and attribution are preserved in `LICENSE`. The optional FPS counter uses [PresentMon](https://github.com/GameTechDev/PresentMon); its license and source link are in `third-party/presentmon`.

The original project's README is preserved in `UPSTREAM-README.md`; its measurements and descriptions are not verified claims about opnoverlay. The installer includes the project, PresentMon, and dependency licenses in `LICENSE.txt`, `PRESENTMON-LICENSE.txt`, and `THIRD-PARTY-LICENSES.html`. Regenerate the dependency license list before distributing a build after dependencies change.
