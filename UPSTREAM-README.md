# obs-lite

Lightweight screen recorder for Windows and Linux: on Windows a single ~7 MB `.exe`, on Linux a
~4 MB Flatpak, with no bundled browser. The interface follows OBS Studio, so OBS users find
everything where they expect it.

![obs-lite recording: the preview, the Sources, Audio Mixer and Controls docks, and the timer](docs/images/main-window.png)

![Start Recording, a few seconds of recording, Stop Recording and the saved file](docs/images/record.gif)

| Windows | Idle (preview + audio) | Recording 1080p60 + audio |
|---|---|---|
| Private memory | ~100 MB | ~270 MB |
| CPU (16 threads) | ~0.3 % | ~1.5 % |

Measured on Windows 11 on a laptop with an RTX 4060 GPU, encoding on its Intel integrated GPU
(Quick Sync), without a webcam; your numbers will vary. About 40 MB of the recording figure is the
queue of converted frames that keeps the encoder from dropping frames when it briefly stalls.

| Linux | Idle (preview + audio) | Recording 1080p60 + audio |
|---|---|---|
| Memory (PSS) | ~115 MB | ~225 MB |
| CPU (16 threads) | ~0.2 % | ~0.7 % |

Measured with the Flatpak on Pop!_OS 24.04 (COSMIC, Wayland) on the same laptop, recording the
whole screen with VA-API on its Intel GPU, without a webcam; memory is the proportional set size of
every process in the Flatpak's sandbox. With VA-API (Intel, AMD) each frame stays on the GPU, as on
Windows. With NVENC or a software encoder it is still copied and converted on the CPU, which costs
more.

## obs-lite and OBS Studio

obs-lite does one thing, record, and leaves out what it would need a much larger program for.

| | obs-lite | OBS Studio |
|---|---|---|
| Linux Flatpak: download / installed | 3.7 MB / 11.7 MB | 225 MB / 593 MB |
| Linux, idle: memory / CPU (one core) | 115 MB / 3.5 % | 279 MB / 5.2 % |
| Linux, recording 1080p60: memory / CPU (one core) | 225 MB / 11 % | 294 MB / 9 % |
| Record the screen or a window, with desktop audio and microphone | ✓ | ✓ |
| Replay buffer | ✓ | ✓ |
| Recording kept after a crash | ✓ (fragmented MP4, repaired at the next start) | ✓ (MKV, or hybrid MP4) |
| Microphone noise suppression | ✓ (RNNoise) | ✓ (RNNoise, Speex, NVIDIA) |
| Webcam | one, in a corner | any number, anywhere |
| Region of the source | ✓ | ✓ (crop filter) |
| Scenes, several sources, filters, transitions | — | ✓ |
| Streaming | — | ✓ |
| Plugins, browser source | — | ✓ |
| Formats | H.264 + AAC in MP4 | H.264, HEVC, AV1… in MP4, MKV, MOV… |
| Interface languages | English, Spanish | many |

The sizes are OBS Studio 33.0 from Flathub and obs-lite 0.5.0, both on the same runtime
(`org.freedesktop.Platform` 26.08), measured with `flatpak info` on 2026-10-27. Memory and CPU were
measured on 2026-10-29 on the laptop above, obs-lite built from `main`: both showing and then
recording the whole screen with its audio, with VA-API on the Intel GPU at 12 Mbit/s, a keyframe
every 2 s and no B-frames. Recording, OBS takes a little less CPU; obs-lite less memory.

> **Status: early (0.6.0).** obs-lite has been tested on Windows on a single machine
> (Windows 11, laptop with Intel and NVIDIA GPUs) and, on Linux, in a virtual machine (Ubuntu
> 26.04, GNOME) and on a laptop (Pop!_OS 24.04, COSMIC, Intel and NVIDIA GPUs); see
> [docs/TESTING.md](docs/TESTING.md) for what was and was not verified. It has
> not had any security audit or independent code review. Most of the code was written with an AI
> coding assistant (Claude Code) and then built and tested as described there.
> It is not affiliated with the OBS Project and contains no OBS code.

## Requirements

- Windows 11. Windows 10 has not been tested.
- A GPU with a working driver (NVIDIA, Intel or AMD). Its hardware H.264 encoder is used; without
  one, the Windows software encoder takes over (more CPU). Tested: Intel Quick Sync, and the
  software encoder.
- Linux: Flatpak, PipeWire and a desktop with the screen-sharing portal (GNOME, KDE Plasma and
  most Wayland desktops). Tested: GNOME 50 on Wayland with the x264 software encoder, and COSMIC
  (Pop!_OS 24.04) with Intel VA-API.

## Quick start

1. Download from [Releases](https://github.com/25Dark25/ObsLite/releases) and
   [verify](#verifying-a-release) either:
   - `obs-lite-X.Y.Z-windows-x64.msi`, the installer: it needs no administrator rights, installs
     to `%LOCALAPPDATA%\Programs\obs-lite`, adds *Obs Lite* to the Start menu and, if you tick the
     box, to the desktop. A newer `.msi` upgrades it in place; uninstall it from *Settings › Apps*.
   - `obs-lite-X.Y.Z-portable-windows-x64.zip`, the portable version: unzip it anywhere and run
     `obs-lite.exe`. There is nothing to install.
   - `obs-lite-X.Y.Z-linux-x86_64.flatpak`, for Linux. Install it with
     `flatpak install --user obs-lite-X.Y.Z-linux-x86_64.flatpak` (it fetches the runtime from
     Flathub the first time) and start *Obs Lite* from the applications menu. A newer bundle
     installs over it.
2. Press *Start Recording* (*Iniciar Grabación* in Spanish), or `Ctrl+Alt+R`. By default it
   records the first display Windows reports and desktop audio at 60 fps to your *Videos* folder.
   On Linux, the desktop first asks which screen or window to share.

## Interface

Same layout as OBS Studio and its dark *Yami* palette, with OBS's wording in Spanish or English
(it follows the system's language, on Linux its locale; change it in Settings › General):

- **Preview** on top and, below it, the **Sources**, **Audio Mixer** and **Controls** docks.
  Right-click the preview to take a screenshot.
- **Sources › +**: *Display Capture*, *Window Capture*, *Application Audio Capture* (the sound of
  one application, with its own mixer channel) or *Video Capture Device* (a webcam shown in a
  corner of the recording). **Properties**: capture the cursor, *Select Region…* to record only part
  of the source, and the webcam's corner and size.
- **Audio Mixer**: desktop audio and *Mic/Aux*, each with a meter, volume slider and mute button
  (mute also works while recording). If the channels together go over the maximum, a limiter
  lowers the volume at those moments instead of letting it distort, and the mixer says so.
- **Controls**: *Start Recording* (with a pause button next to it while recording), *Start Replay
  Buffer* (with a *Save Replay* button next to it while it runs), *Settings* and *Exit*.
- **Settings**: *General* (language, minimize to tray, hide obs-lite from capture), *Output*
  (recording path and quality, maximum replay time), *Audio* (devices, microphone noise suppression), *Video* (scaled
  output resolution and FPS) and *Hotkeys* (click a field and press the combination). *OK* applies,
  *Cancel* discards.
- **Status bar**: messages, CPU usage, dropped frames and the recording indicator with elapsed time.
- **File**: *Take Screenshot* and *Show Recordings*.
- **System tray icon**: grey when idle, in colour while recording or while the replay buffer runs.
  Its menu starts/stops and pauses recording, starts/stops the replay buffer, saves a replay, takes
  a screenshot, shows the window or exits; a left click shows the window.

## Features

- **Display or window capture** through Windows Graphics Capture (the API OBS uses), with or
  without the cursor.
- **Desktop audio + microphone** mixed together, with volume, mute, meters and device selection.
- **Application audio**: the sound of a single application (and its child processes), e.g. a game
  without Discord, as its own mixer channel; desktop audio then leaves that application out, so it
  is heard once. Needs Windows 10 version 2004 or later.
- **H.264 + AAC** to MP4, which plays everywhere: the GPU's hardware encoder (NVENC / Quick Sync /
  AMF through Media Foundation), or the Windows software encoder if the GPU has none. A keyframe
  every 2 s; BT.709 colours, signalled in the file.
- **Recordings survive a crash**: the MP4 is written in fragments, so if obs-lite or the PC stops
  mid-recording (a crash, a power cut) the file keeps all but the last fraction of a second, and
  the next time obs-lite starts it turns that file into a regular MP4. A normal stop leaves a
  regular MP4.
- **Constant frame rate** (30 or 60 fps), suitable for video editors. When the screen does not
  change the last frame is repeated; when the machine falls behind, slots are skipped and reported
  as dropped frames.
- **Pause and resume** without gaps: paused time never reaches the file, in video or audio.
- **Replay buffer**, like OBS's: keeps the last 20 s (5–600, in Settings › Output) in memory, already
  encoded, and *Save Replay* writes them to a new MP4 (`Replay 2026-10-18 17-43-13.mp4`, or
  `Repetición …` in Spanish). It can run on its own or together with a recording; a recording
  started while it runs begins at once, on a forced keyframe. Memory: what the quality preset
  encodes in that time, about 30 MB for 20 s at 1080p60 medium.
- **Hardware scaling** to 1440p, 1080p or 720p (downscale only, aspect ratio preserved).
- **Region recording**: drag a rectangle on the preview to record only that part (cropped on the GPU).
- **Webcam overlay** (picture-in-picture) in any corner, in three sizes.
- **Screenshots** as full-resolution PNG, with the region and webcam applied.
- **Microphone noise suppression** with RNNoise, like OBS's filter.
- **Configurable global hotkeys** for start/stop, pause/resume, the replay buffer, saving a replay,
  screenshot and muting Mic/Aux, from any application. Defaults: `Ctrl+Alt+R` and `Ctrl+Alt+P`.
- **Hidden from capture**: like OBS's option, the obs-lite window never appears in recordings or
  in its own preview.
- **System tray icon**, with an option to minimize to the tray.
- **Spanish and English** interface.
- **Update notice**: at start-up it asks GitHub for the latest release and, if there is a newer
  one, says so in a dialog with a link to it (and in the status bar). "Don't remind me of this
  version" stops the dialog until the next release. Nothing is downloaded; Settings › General
  turns the check off.
- Sharp preview at 5 fps: frames are scaled down to the size the preview is drawn at, averaging
  every pixel, so text stays legible.

## Linux

The same interface and recordings, with what Linux desktops allow:

- **Screen or window**: *Sources › + › Screen or Window Capture…* asks the desktop, which shows
  its sharing dialog. With *Remember this decision* ticked it asks only once; choosing the source
  again asks again. Works on Wayland and X11 (through PipeWire).
- **Global hotkeys** go through the desktop, which keeps them: obs-lite proposes its combinations
  and GNOME asks to confirm them; they can be changed in the system's keyboard settings.
- **Webcam**: the desktop asks for permission the first time a camera is chosen.
- **Encoding** with GStreamer: VA-API (Intel/AMD) or NVENC if the GPU has them, otherwise x264.
  With VA-API the desktop's frames go to the encoder without leaving the GPU, the region and the
  webcam included.
- **Not on Linux**: application audio, hiding obs-lite from capture (Wayland cannot keep a window
  out of a capture) and the update notice. So a capture of the whole screen shows obs-lite, with
  its preview inside itself; capture a window instead, or minimize obs-lite while recording.
- **Privacy**: the Flatpak has no network access. It can use the display, the GPU, the sound
  server (desktop audio and the microphone; there is no portal for desktop audio) and the *Videos*
  folder; the screen, the webcam, hotkeys and any other folder go through the desktop, which
  asks. The full list, with the reason for each, is in
  [the manifest](packaging/flatpak/io.github._25Dark25.ObsLite.yml).

## Configuration

Everything is set from the interface (*Settings* and the docks) and saved in
`%APPDATA%\obs-lite\settings.json` (on Linux,
`~/.var/app/io.github._25Dark25.ObsLite/config/obs-lite/settings.json`). Delete that file to go back to the defaults: first display,
cursor captured, desktop audio on, microphone off, 60 fps, native resolution, medium quality,
output to the *Videos* folder, hotkeys `Ctrl+Alt+R` and `Ctrl+Alt+P`, obs-lite hidden from capture,
update check on, 20 s replay buffer.
Recordings are named after their start time (`2026-10-16 13-45-02.mp4`) and screenshots likewise
(`Screenshot 2026-10-16 13-45-02.png`, or `Captura …` in Spanish).

## Known limitations

- The hardware encoder takes 0.2–1 s to start; that span is not recorded (the status bar shows
  *Preparando…*).
- Until obs-lite starts again, a recording cut short by a crash or a power cut is a fragmented
  MP4: it plays, but some players show no duration and some editors may not open it. Only the
  last recording started is repaired; for any other, `ffmpeg -i in.mp4 -c copy out.mp4` turns it
  into a regular MP4.
- While the Windows session is locked, Windows delivers no new frames.
- A window resized while recording keeps the recording's size: on Windows it is scaled down to
  fit (never up, so a smaller window stays sharp) and centred, with black around it; on Linux it
  is cropped or padded with black.
- On a laptop with two GPUs the image is captured, and so encoded, on the GPU driving the display
  (usually the integrated one).
- A PC without a GPU driver (for example a virtual machine without graphics acceleration) cannot
  record: colour conversion needs the GPU's video processor.
- One display or window source plus an optional webcam. No streaming, scenes, video filters or other
  codecs: obs-lite only records.
- Minimize to tray keeps the window minimized and removes its taskbar button (a hidden window would
  stop handling hotkeys); it still appears in Alt+Tab.
- Noise suppression downmixes the microphone to mono.
- With desktop audio set to a specific output device (not *Default*), desktop audio still includes
  the application captured on its own, so that application is heard twice. The application is
  chosen among applications with a window and found again by executable name; if it starts after
  obs-lite, its audio is picked up when the recording starts.
- Hotkeys need Ctrl or Alt (F-keys excepted), and are off while Settings is open.
- Neither the executable nor the installer has an Authenticode signature (they are signed with
  OpenPGP instead), so Windows SmartScreen may warn before running them the first time and the
  installer shows an unknown publisher.
- The installer is per user: each Windows account that wants obs-lite installs it. Uninstalling
  keeps the settings in `%APPDATA%\obs-lite`.
- Pausing a recording also pauses the replay buffer: both share one encoder and clock.
- A replay starts at a keyframe (one every 2 s), so it is up to 2 s longer than the maximum
  replay time.
- Linux: the Flatpak bundle is not updated by `flatpak update`, and with no network access there
  is no update notice; new versions are on the Releases page.
- Linux: a folder outside *Videos* can only be chosen with *Browse*, which grants access to it.
- Linux: tested on GNOME in a virtual machine without a GPU, where the window is drawn in
  software (about 6 % of a CPU core while idle and silent, more while the meters move), and on
  COSMIC with Intel VA-API. KDE Plasma, X11, AMD and recording with NVENC have not been tested.
- COSMIC (Pop!_OS 24.04): its screen-sharing portal crashes now and then (a COSMIC bug).
  obs-lite shares the screen again by itself; a recording keeps going, with the picture frozen
  for under a second. COSMIC has no global-shortcuts portal yet, so hotkeys do not work there (the
  status bar says so at start-up).

## Verifying a release

Each release zip, installer and Flatpak bundle has a detached signature (`.asc`) made with this
OpenPGP key:

```
A7FC EDEA D7E6 C343 7312  284A 442D A687 E0FF 73EE
```

1. Import the key and check that the fingerprint matches the one above **and** the one published
   in at least one other independent place (for example <https://github.com/25Dark25.gpg>):

   ```sh
   curl -sL https://github.com/25Dark25.gpg | gpg --import
   gpg --fingerprint A7FCEDEAD7E6C3437312284A442DA687E0FF73EE
   ```

2. Verify the signature of the zip (or, the same way, of the `.msi` or the `.flatpak`):

   ```sh
   gpg --verify obs-lite-0.6.0-portable-windows-x64.zip.asc obs-lite-0.6.0-portable-windows-x64.zip
   ```

   The output must say `Good signature` and show the fingerprint above.

3. Optionally, compare the file's SHA-256 with the one in the release notes:
   `sha256sum obs-lite-0.6.0-portable-windows-x64.zip`, or on Windows
   `Get-FileHash obs-lite-0.6.0-portable-windows-x64.zip -Algorithm SHA256`.

`gpg` ships with [Gpg4win](https://gpg4win.org/) and with Git for Windows.

## Development

Requires Rust 1.95 or later and the MSVC Build Tools with the Windows SDK (used to embed the icon).

```powershell
cargo build --release
.\target\release\obs-lite.exe
```

On Linux, the Flatpak is built from the repository root with `flatpak-builder` and the Flathub
remote (the build has no network, so the crates are vendored first):

```sh
cargo vendor --locked vendor > /dev/null
flatpak-builder --user --install-deps-from=flathub --force-clean --install \
  build-flatpak packaging/flatpak/io.github._25Dark25.ObsLite.yml
```

A build outside Flatpak needs the GStreamer development files, and a desktop entry named
`io.github._25Dark25.ObsLite.desktop` in `~/.local/share/applications` for global hotkeys (the
desktop identifies apps by it; `packaging/flatpak` has one). Ubuntu/Debian package names:

```sh
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev   # to build
sudo apt install gstreamer1.0-pipewire gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  gstreamer1.0-plugins-ugly gstreamer1.0-libav     # to record: VA-API, NVENC, x264, AAC, MP4
cargo build --release
```

It uses the first H.264 encoder that is installed and encodes a test picture: VA-API (Intel/AMD),
NVENC, x264, then OpenH264.

`--selftest` records the primary display plus desktop audio through the same pipeline as the app:

```powershell
.\target\release\obs-lite.exe --selftest 6 test.mp4            # 6 s at 1080p60
.\target\release\obs-lite.exe --selftest 6 test.mp4 pause 720  # pauses the middle third, 720p output
.\target\release\obs-lite.exe --selftest 6 test.mp4 replay     # also saves the last 3 s as test-replay.mp4
```

It prints captured, encoded and dropped frame counts. The installer is built from a release build
with [WiX](https://wixtoolset.org/) 6.0.2 (`installer/build.ps1`).

### How it works

```
WGC (only when the image changes) ─┐
webcam (Media Foundation) ─────────┴─► GPU canvas (region crop + webcam overlay)
                                                │
recording clock (excludes pauses) ─► video thread converts the canvas every 1/fps
                                     to NV12 (GPU video processor, scaling) into a queue
WASAPI desktop + mic + app ─► mixer paced by the same clock (fills silence)
                                                │
      H.264 encoder (hardware, or software) + AAC encoder ─► packets ─┬─► MP4 writer
                                                    replay buffer (in memory) ◄─┘ ─► MP4 on save
```

- `src/app.rs`: user interface (egui) and recording lifecycle.
- `src/media.rs`: recording clock, encoded packets, recording file and replay buffer.
- `src/capture.rs`, `src/camera.rs`, `src/audio.rs`: sources, preview and picture helpers, webcam
  overlay, audio inputs, noise suppression and mixer.
- `src/linux/`: the Linux side: ScreenCast portal (`portal.rs`), PipeWire capture
  (`capture.rs`), GStreamer encoding and MP4 (`encoder.rs`), audio (`audio.rs`), webcam through
  the Camera portal (`camera.rs`), hotkeys through the GlobalShortcuts portal (`hotkeys.rs`), system
  calls (`sys.rs`).
- `src/win/`: the Windows side: capture sessions (`capture.rs`), canvas, frame queue and encoder
  (`encoder.rs`), Media Foundation (`mf.rs`), WASAPI (`audio.rs`), webcam (`camera.rs`), hotkeys
  (`hotkeys.rs`) and system calls (`sys.rs`).
- `src/hotkeys.rs`: hotkeys as text in the settings; registering them is platform code.
- `src/config.rs`: settings and their defaults.
- `packaging/flatpak/`: the Flatpak manifest, desktop entry and AppStream metadata.
- `build.rs`: draws the icon and embeds it in the executable.

## Security

Please report vulnerabilities privately; see [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE). Each release also ships `THIRD-PARTY-LICENSES.html` with the licenses of the
crates compiled into `obs-lite.exe`, generated with [cargo-about](https://github.com/EmbarkStudios/cargo-about)
(`cargo about generate --locked --fail about.hbs -o THIRD-PARTY-LICENSES.html`).
