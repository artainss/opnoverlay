# Testing

## Automated

Run on every pull request by CI (Windows and Ubuntu, Rust 1.95 and stable), together with clippy,
a release build and `cargo deny`. `mf::software_encoder_encodes_the_canvas` is Windows-only. CI also builds the MSI, validates it (ICE) and runs `installer/test.ps1`,
which installs it for the runner's user and checks the files, the *Obs Lite* Start menu shortcut,
that there is no desktop shortcut by default, the *Installed apps* entry and its icon, a clean
uninstall, then an install with the desktop shortcut and an upgrade that keeps it:

```powershell
cargo test --locked
```

| Test | Checks |
|---|---|
| `media::a_pause_is_left_out_of_the_file_but_not_out_of_the_replay_buffer` | A pause in the middle of a recording, resumed between keyframes: the file goes on at the keyframe asked for, its video and audio times following on with no gap or overlap, its length (the timer) stopped during the pause; the replay buffer keeps all of it. |
| `media::audio_made_before_a_pause_and_coming_after_it_is_recorded` | Audio comes a little after the video of its time: what was made before the pause is recorded even when it comes after it, what was made during the pause is not, even when it comes after the resume. |
| `media::replay_keeps_the_last_seconds_from_a_keyframe` | The replay buffer keeps at least its length, from a keyframe, at most one keyframe interval more, and audio from the first frame on. |
| `media::replay_shorter_than_its_length_keeps_everything` | A replay buffer that has run less than its length keeps every packet. |
| `media::replay_starts_at_the_first_keyframe` | Packets before the first keyframe are not kept (they would not play). |
| `media::a_recording_starts_at_the_next_keyframe_with_the_audio_since_then` | A recording started while encoding asks for a keyframe once, starts on it and keeps only the audio from then on. |
| `media::a_recording_without_frames_is_an_error` | Stopping a recording before any frame reports an error instead of an empty file. |
| `media::a_saved_replay_is_interleaved_by_time` | A saved replay writes video and audio in time order, relative to its first keyframe, from another thread. |
| `win::encoder::a_resized_window_is_scaled_down_to_fit_and_centred` | Windows only: where a resized window goes on the recording: scaled down to fit, never up, centred. |
| `win::mf::hardware_encoders_give_their_memory_back` | Windows, ignored by default (needs a GPU with a hardware H.264 encoder; `cargo test --release -- --ignored`): five rounds of a new D3D device and a hardware encoder on it, 30 frames each, then everything dropped; the process's GPU memory must not grow. |
| `mp4::a_fragmented_file_becomes_a_regular_one_with_every_sample_in_place` | A fragmented MP4, with fragments laid out as Media Foundation and as GStreamer write them, becomes a regular one in place: one full index at the end, no fragment boxes left, and every sample's bytes, duration, keyframe flag and composition offset read back through the new index. |
| `mp4::a_file_cut_short_is_repaired_up_to_its_last_whole_fragment` | A file whose writing stopped inside its last fragment's data is refused as a finished file, and repaired into a regular MP4 with the whole fragments before it. |
| `app::a_recording_cut_short_is_repaired_at_start_up` | At start-up the last recording, if it was cut short, becomes a regular MP4 and the status bar says so with a link; a finished or missing one is left alone without a message. |
| `mp4::a_regular_file_is_left_alone` | A file that is not fragmented is not touched. |
| `mp4::data_past_4_gib_is_indexed_with_64_bit_offsets` | Data past 4 GiB gets 64-bit chunk offsets (`co64`). |
| `mp4::a_duration_too_long_for_32_bits_moves_the_box_to_version_1` | A track longer than 32 bits of its timescale allows gets a version 1 header, keeping its other fields. |
| `mf::software_encoder_encodes_the_canvas` | Without a hardware encoder, the canvas still goes through the GPU colour conversion and the Windows software H.264 encoder (one packet per frame, keyframe first). Skipped where there is no GPU video processor, such as CI. |
| `audio::mixes_with_gain_and_pads_silence` | Inputs are mixed with their gain; a short input is padded with silence. |
| `audio::inputs_adding_up_past_full_scale_are_limited_not_clipped` | Two inputs that add up to 1.4 are brought within full scale with one steady gain, so the wave keeps its shape (a 50 Hz tone: the gain must not wobble from cycle to cycle). |
| `audio::a_mix_within_full_scale_is_untouched` | The limiter leaves a mix that fits exactly as it is. |
| `audio::drops_oldest_samples_beyond_latency_bound` | A backed-up input drops its oldest samples instead of drifting. |
| `config::bitrate_scales_with_pixel_rate` | Quality presets scale with resolution and frame rate, within limits. |
| `config::resolution_scales_down_only_and_keeps_aspect` | Output scaling never upscales and keeps even dimensions and aspect ratio. |
| `config::missing_fields_fall_back_to_defaults` | An old or partial `settings.json` still loads. |
| `config::settings_saved_with_a_utf8_bom_still_load` | A `settings.json` saved with a UTF-8 BOM (e.g. by Windows PowerShell) still loads. |
| `app::mute_silences_regardless_of_volume` | Mute wins over any volume. |
| `app::volume_uses_a_squared_curve` | The volume slider maps to gain as `volume²`. |
| `app::no_preview_pictures_for_a_window_that_is_not_shown` | No preview pictures are made while the window is minimized (Windows says so) or has not been drawn for 3 s (Wayland only stops asking for frames); a shown idle window draws once a second, so it never counts as hidden. |
| `app::a_silent_idle_window_repaints_rarely` | The window draws once a second when idle and silent, and at 15 fps while a meter moves or a recording runs. |
| `app::exit_waits_for_the_recording_being_saved` | Closing the app while a recording is being saved waits for the MP4 to be finalized. |
| `i18n::every_spanish_variant_is_spanish` | The UI is Spanish for every Spanish Windows locale and English otherwise. |
| `i18n::every_spanish_locale_is_spanish` | The same for POSIX locales (`es_ES.UTF-8`, `es`…), as read on Linux. |
| `linux::encoder::a_region_is_copied_and_what_is_missing_is_black` | Linux only: a region (or a shrunk window) is copied into the canvas, padded with black. |
| `linux::encoder::the_webcam_is_pasted_in_place` | Linux only: the webcam picture lands in its rectangle. |
| `linux::encoder::packet_times_discount_the_encoders_offset` | Linux only: packet times ignore the 1000-hour offset GStreamer video encoders add, so audio is not dropped from files. |
| `linux::encoder::aac_codec_data_describes_lc_48k_stereo` | Linux only: the AAC stream description given to the muxer before any audio is AAC-LC, 48 kHz, stereo. |
| `linux::encoder::frames_on_the_gpu_are_recorded_and_cropped` | Linux only, skipped without VA-API (CI): colour-bar frames on the GPU are converted and encoded there into a file that finishes: 320×240; an odd 321×241, cropped to 320×240; and a 32×64 region, which decodes to the cyan bar it lies on. |
| `linux::encoder::the_webcam_is_put_on_frames_on_the_gpu` | Linux only, skipped without VA-API: a red webcam picture given once, before the frames, is in its place on the last decoded picture, and the bars show beside it; stopping takes under 5 s (10 s before the fix: the webcam's input never ended). |
| `linux::encoder::the_recorded_part_is_cropped_from_what_the_frame_has` | Linux only: the crop for frames on the GPU: none for a whole frame, the even size for an odd one, the region, what is left of it when a window shrank into it, and nothing when it shrank past it (before the fix, the whole window was recorded). |
| `linux::encoder::a_webcam_smaller_than_the_compositor_takes_is_still_recorded` | Linux only, skipped without VA-API: a 12×8 webcam picture (a small webcam in a small region) is recorded in its place; before the fix, VA-API's compositor refused it and the recording stopped. |
| `linux::encoder::a_first_frame_that_does_not_get_through_fails_at_once` | Linux only: starting a recording on the GPU waits for the first frame's surface, and fails at once when the conversion refuses the frame, or after 3 s when nothing comes back, so frames are taken in memory instead of recording without video. |
| `linux::encoder::the_webcam_is_placed_at_the_recordings_size` | Linux only: the webcam's place, worked out at the captured size, is scaled to the recording's. |
| `linux::capture::a_recording_takes_frames_on_the_gpu` | Linux only: frames are asked for on the GPU when recording, with or without a region and the webcam, and not for the preview alone or again after the GPU failed to encode them. |
| `linux::gpu::the_desktops_formats_are_offered_in_va_apis_layout_and_linear` | Linux only: the desktop is asked for its four formats in the layout VA-API reads, each also linear (COSMIC's portal fails when offered one layout), and nothing when VA-API reads none of them. |
| `hotkeys::a_pressed_combination_becomes_text` | A combination pressed in the settings becomes "Ctrl+Alt+R". |
| `hotkeys::keys_that_would_break_typing_are_rejected` | A hotkey needs Ctrl or Alt unless it is an F-key. |
| `hotkeys::a_modifier_alone_is_not_a_hotkey` | Pressing Ctrl or Alt alone does not end hotkey capture. |
| `win::hotkeys::text_round_trips_to_a_registrable_hotkey` | Windows only: "Ctrl+Alt+R" in the settings becomes the right registered hotkey. |
| `win::hotkeys::invalid_text_is_rejected` | Windows only: malformed hotkeys in the settings are ignored. |
| `linux::hotkeys::hotkeys_become_portal_triggers` | Linux only: "Ctrl+Alt+R" becomes the shortcut text the desktop expects ("CTRL+ALT+r"); unknown modifiers are rejected. |
| `capture::bgra_rows_with_padding_become_packed_rgba` | Screenshot read-back handles padded GPU rows. |
| `capture::png_round_trips` | Screenshots are valid PNG with the right pixels. |
| `capture::crop_keeps_only_the_region` | Screenshots are cropped to the region. |
| `capture::preview_downscale_averages_every_pixel` | The preview averages every source pixel when scaling down, and is an exact copy at full size. |
| `capture::preview_downscale_matches_the_straightforward_average` | The faster preview scaling gives exactly the same pixels as adding each source pixel one by one, at odd sizes and with padded rows. |
| `config::region_is_clipped_to_the_source_with_even_sides` | A region survives the source changing size and keeps even sides for H.264. |
| `app::a_selection_on_the_preview_maps_to_source_pixels` | A rectangle dragged on the preview becomes the right region, and a click selects nothing. |
| `audio::denoiser_keeps_the_stream_length_in_whole_frames` | Noise suppression neither loses nor invents samples. |
| `audio::denoiser_keeps_silence_silent` | Noise suppression adds nothing to silence. |
| `audio::only_sound_after_silence_wakes_the_ui` | An input wakes the window only when it goes from silence (below −60 dB) to sound. |
| `camera::overlay_sits_in_the_corner_with_the_camera_aspect` | The webcam overlay's position and size. |
| `camera::resize_keeps_flat_colour_and_identity` | The webcam scaling. |
| `linux::camera::denied_access_says_how_to_allow_it` | Linux only: when the desktop gives no camera access, the error says to choose the camera again or allow it in the privacy settings, instead of "Portal request was cancelled". |
| `linux::camera::row_padding_is_dropped` | Linux only: camera pictures with padded rows are packed, and short buffers are rejected. |
| `linux::sys::document_portal_paths_are_recognised` | Linux only: a folder granted through the folder picker (`/run/user/UID/doc/ID/NAME/…`) is recognised, so its real path can be shown; other paths are left alone. |
| `linux::sys::a_granted_folder_shows_without_a_trailing_slash` | Linux only: that real path is shown as `/home/…/Descargas`, not `/home/…/Descargas/`. |
| `tray::idle_icon_is_grey_with_the_same_alpha` | The idle tray icon. |
| `update::versions_compare_numerically` | Only a strictly newer plain version counts as an update. |
| `update::release_is_read_from_the_github_response` | The tag and page are read from GitHub's response; an error response gives nothing. |
| `app::saved_message_survives_a_source_that_is_gone` | When the recorded window is closed, the "saved" message and file link stay, and the source is deselected. |
| `app::desktop_audio_leaves_out_the_application_captured_on_its_own` | With an application captured on its own, desktop audio leaves it out; not with no application, nor with a chosen output device. |
| `app::a_new_release_opens_the_dialog_unless_skipped` | A newer release opens the update dialog unless the user chose not to be reminded of that version; the status-bar link shows either way. |
| `app::update_dialog_buttons_stay_inside_it_in_both_languages` | In Spanish and English, nothing in the update dialog starts left of its title: the buttons fit. |

## Results

Capture and encoding need a GPU and a desktop session, which CI runners do not have, so they were
checked on real machines. What was checked, and what was not:

2026-10-16, Windows 11, RTX 4060 Laptop GPU, Rust 1.99, on commits `96413f0` and `b6cdad1` (with the
settings BOM fix):

| Step | Result |
|---|---|
| Self-test, 6 s and 15 s at 1080p60, screen in motion | 60 fps exactly, 0 dropped frames, AAC stereo 48 kHz |
| Self-test with pause, 720p | 3.65 s, 1280×720, 60 fps |
| Interface flow | Valid file; status bar shows the saved path |
| Hotkeys: record, pause 2 s, resume, stop | 5.63 s file (pause excluded) |
| Window shrunk, grown, then closed | Recording stopped on close; valid file (7.03 s, 626×392, 30 fps); status bar kept *Grabación guardada en …*; source deselected |
| Every desktop and microphone device (2 + 2) | No errors; signal on the active devices |
| Exit while recording, and right after *Detener* (while saving) | File finalized in both cases (3.67 s, 1080p30); app exits in 0.2 s |
| Settings *Aceptar* / *Cancelar* | *Cancelar* kept 30 fps after picking 60; *Aceptar* wrote `fps: 30` to `settings.json` at once and the next recording was 30 fps |
| Release workflow artifact (`b6cdad1`), `--selftest 3` | Hashes match the run log; 2.73 s, 1080p60, 0 dropped frames |
| Resource use | Idle ~85 MB private / 0.3 % CPU; recording 1080p60 ~180 MB / 1.1 % CPU |

2026-10-16, Windows 11, RTX 4060 Laptop GPU, Rust 1.99, `features` branch:

| Step | Result |
|---|---|
| Language | English and Spanish switch live; source label "Pantalla 1" / "Display 1" follows |
| Hide from capture | A GDI screenshot of the desktop shows what is behind the window |
| Screenshot | Context menu and `Ctrl+Alt+S`: 1920×1080 PNG with correct colours |
| Hotkeys | `Ctrl+Alt+S` and `Ctrl+Alt+M` assigned by pressing them; both worked; a stray "Ctrl+ControlLeft" capture was found and fixed (test added) |
| Region | 840×504 region: recording 840×504 at 60 fps, screenshot 840×504 with the selected content |
| Noise suppression | Built-in microphone, quiet room: noise floor about −76 dBFS off, −105 dBFS on |
| Tray | Taskbar button removed when minimized; hotkey recording while minimized (3.82 s); tray menu start/stop (5.44 s); left click restores |
| Webcam | Added from *+ › Video Capture Device*; overlay in the chosen corner of preview and recording. Static window recorded with a 400×300 region and the webcam: 400×300, 60 fps, the camera corner changes every frame while the rest does not |
| Black frames | Before the fix, counted with ffmpeg: 1 per 15 s on the screen without webcam (also on `main`), 5 per 15 s with it, up to 27 per 10 s in a debug build. After: 0 in 5 webcam recordings and 5 self-tests (6–15 s each), 0 lagged frames |
| Preview sharpness | Preview drawn at about 1170 px wide on a 1920×1080 display: text in it is legible (it was 640 px, one pixel per block). Idle CPU 4.2 → 7.5 % of one core (0.3 → 0.5 % of 16 threads), private memory 95 → 98 MB (105 MB maximized) |
| Update notice | A build labelled 0.2.0 queried the real GitHub API at start-up and showed "Nueva versión 0.2.1 disponible" in the status bar |
| Application audio | A PowerShell window looping a 440 Hz tone: captured on its own at −14 dBFS while VSCodium, captured at the same time, gave silence; recording with desktop audio off had the tone (mean −14.1 dB, ffmpeg volumedetect); mixer channel and source row shown |

2026-10-18, Windows 11, laptop with Intel UHD and RTX 4060 GPUs, new encoding engine (capture
and so encoding on the Intel GPU, *Intel Quick Sync Video H.264 Encoder MFT*):

| Step | Result |
|---|---|
| Self-test 6 s 1080p60 | 350 frames, 60 fps, H.264 High, AAC-LC 48 kHz stereo, keyframes at 0/2/4 s, BT.709 signalled, 0 black, 0 decode errors |
| Self-test with pause, 720p | 1280×720, 3.88 s, 233 frames, 0 black |
| Window grown and shrunk while recording | Cropped / padded with black as documented; 0 black frames, 0 decode errors |
| Region | Region past the right edge clipped to 520×360; 0 black frames |
| Black frames, 15 s with webcam | 890 frames, 0 black |
| Application audio with a region | Tone at −14.1 dB mean, desktop audio off |
| Audio/video sync, 10 min | 35 993 frames, 0 lagged; offset 0.211 s at the start and 0.207 s at the end (the same 0.2 s as 0.2.1, so it comes from the test signal); video 599.88 s, audio 599.87 s; private memory 225 → 229 MB |
| Software encoder | 1080p60: 359 frames, 0 lagged, about one CPU core; 720p with pause: 239 frames; correct colours |
| Resource use | Idle 98 MB / 0.5 % CPU; recording 1080p60 270 MB / 1.5 % CPU (0.2.1: ~350 MB / 1.3 %) |
| Replay buffer | Self-test with `replay`: 3 s asked, 4.83 s saved (from the keyframe before), audio −14.1 dB. In the app: two replays of 20.87 s and 20.90 s, first frame a keyframe, 0 decode errors; a recording started while it ran lasted 6.00 s for 6 s between hotkeys (forced keyframe), and another, paused 2 s, lasted 5.03 s with a 440 Hz tone at −14.1 dB; private memory 280 MB with both running. A bug where stopping a recording without the replay buffer left the MP4 unfinished was found here and fixed before commit. Not tested: the new tray menu items |

2026-10-18, installer (WiX 6.0.2), same machine:

| Step | Result |
|---|---|
| `installer/test.ps1` | All 23 checks passed (install, uninstall, desktop shortcut, upgrade 0.2.1 → 0.2.2 keeping it, clean uninstall) |
| Installer, interactive | Welcome, *Shortcuts* (box unticked), *Ready*, *Completed*; no UAC prompt; installed to `%LOCALAPPDATA%\Programs\obs-lite`; MSI 3.1 MB. Started from the Start menu shortcut, whose icon is the obs-lite one |
| `wix msi validate` | Passes; warnings ICE69 (shortcut and executable in different components of the same feature) and ICE91 (per-user folder), expected for a per-user package |

2026-10-18, release preparation for 0.3.0 (`main` plus the version bump), same machine:

| Step | Result |
|---|---|
| Automated tests and clippy | 33 passed; clippy clean |
| Self-test 6 s 1080p60, 440 Hz tone playing | 352 frames, 0 lagged, 5.87 s, audio −14.0 dB |
| Self-test with pause, 720p and `replay` | 1280×720, 3.87 s, 0 lagged, audio −14.1 dB; replay 3.85 s with audio |
| `installer/test.ps1` on the 0.3.0 MSI | All checks passed |

2026-10-22, Linux: Ubuntu 26.04.1 LTS (GNOME on Wayland, PipeWire, GStreamer 1.28.2) in a Hyper-V
VM without a GPU, so x264 in software; desktop audio through the VM's dummy output's monitor:

| Step | Result |
|---|---|
| Portal | The sharing dialog appears once ("Remember this decision" ticked); later sessions start without it (restore token), in under 0.1 s |
| Self-test 8 s with `replay`, test pattern moving and a 440 Hz tone | 1024×768, 60 fps, 479 frames, 0 lagged; H.264 High (x264) + AAC-LC; audio −33.2 dB; 0 decode errors; first frame a keyframe; replay 3.92 s with audio. About 3 s of CPU for 8 s of recording; 117 MB peak memory |
| Interface flow | The app starts with the preview of the shared screen, the desktop's sharing indicator and the tray icon (red while recording); *Start/Stop Recording* saved a 7.6 s file (456 frames at 60 fps, AAC) to `~/Vídeos` |
| Bug found and fixed | Audio was missing from files: video packet times carried GStreamer encoders' 1000-hour offset, so all audio fell before the file's start. Times are now running times (test added; it fails without the fix) |

Later the same day, hotkeys and webcam, same VM; a desktop entry for `io.github._25Dark25.ObsLite`
installed in `~/.local/share/applications`, as the Flatpak will have:

| Step | Result |
|---|---|
| Automated tests and clippy | 41 passed; clippy clean on 1.95 and stable (Linux and Windows); 38 passed on Windows |
| Hotkeys | GNOME's *Add Keyboard Shortcuts* dialog listed the proposed combinations; `Ctrl+Alt+R` started a recording with another window focused, `Ctrl+Alt+P` paused and resumed it, `Ctrl+Alt+R` stopped it: 24.65 s file (1479 frames, AAC, 0 decode errors), pauses excluded. Without the desktop entry the portal refuses ("An app id is required") and the status bar says so |
| Webcam | A virtual camera (`v4l2loopback` fed with a GStreamer test pattern). Before access is granted, *Video Capture Device* shows one "Camera" entry and nothing is asked; choosing it brings GNOME's *Allow Obs Lite to Use the Camera?* dialog; then the overlay shows in the preview and in an 8.45 s recording (507 frames, AAC) and the desktop's camera indicator is on. After a restart the camera comes back without asking and the menu lists it by name; choosing it by name works. Without the desktop entry GNOME asks generically ("Allow Apps…") and the camera works too |
| Bugs found and fixed | GNOME refused the camera dialog ("Only the focused app is allowed to show a system access dialog") because the window did not carry the app id; it does now. The camera list showed each camera twice; it is deduplicated |

Not verified on Linux: VA-API and NVENC encoders (no GPU in the VM), a real microphone (the VM has
none), a real webcam, removing the webcam (the camera indicator should go off), denying camera
access, X11 sessions, desktops other than GNOME.

Mix limiter, same VM: a 440 Hz tone at 0.7 on the desktop (a null sink) and a 660 Hz tone at 0.7 on
a virtual microphone (a second null sink's monitor), so the mix reaches 1.4. Distortion is the
energy away from the two tones in seconds 1–5 of the file's audio:

| Step | Result |
|---|---|
| Automated tests and clippy | 43 passed on Linux, 40 on Windows; clippy clean on 1.95 and stable (both) |
| Before (`main`) | Both meters at −3.1 dB and no notice; distortion −15.3 dB, clipping harmonics at 220, 860, 1540 and 1740 Hz |
| After | *Mix at its limit* shown while recording; distortion −28.3 dB, no clipping harmonics: what is left sits next to the tones, the gain following peaks that change as the two captures drift in phase |
| Reference: each tone at 0.35 | No notice; distortion −34.6 dB (the floor of capture and AAC) |

2026-10-23, Flatpak, same VM: runtime `org.freedesktop.Platform` 26.08 (GStreamer 1.28.6, x264 from
its codecs-extra extension), built with `flatpak-builder` 1.4.8 and installed from the bundle:

| Step | Result |
|---|---|
| Build | 3 min; the app is 11.6 MB installed, the bundle 3.6 MB; `desktop-file-validate` and `appstreamcli validate` pass; permissions as listed in the manifest |
| Install and start | *Obs Lite* in the dock with its icon after the session reloads Flatpak's exports (only needed once, after installing Flatpak itself) |
| Sharing dialog | Shown the first time; the next starts restored the source without it |
| Recording | 8.7 s, 1024×768 H.264 High at 60 fps + AAC-LC, 0 decode errors, in `~/Vídeos` |
| Hotkeys | `Ctrl+Alt+R` started and stopped a recording with another window focused |
| Webcam | Overlay in the preview (camera access had already been granted to this app id, so no dialog) |
| Tray | Icon grey when idle and red while recording; its menu shows all actions |
| Browse | The folder picker (portal) opened; *Descargas* chosen; the next recording (4.6 s) landed in `~/Descargas`. The document portal maps the granted path to `/home/dark/Descargas` (checked from inside the sandbox) |
| Idle, preview and desktop audio | Native build: 24 % of one core, 38 MB anonymous memory. Flatpak: 33 %, 43 MB anonymous plus 84 MB of runtime libraries (file-backed, shared with other apps on the same runtime). Most of the CPU is Mesa drawing the window in software (no GPU in the VM) |
| Recording 1024×768 at 60 fps | Flatpak: 58 % of one core (x264 in software and the window), 186 MB PSS |
| Bugs found and fixed | The tray icon failed in the sandbox: `tray-icon` 0.26.0 asked for a D-Bus name Flatpak does not allow; 0.26.1 does not in a sandbox. *File › Show Recordings* and clicking the "saved" message did nothing on Linux (they ran Windows' `explorer`); they now go through the desktop's OpenURI portal. A folder chosen with *Browse* showed as `/run/user/1000/doc/…`; the setting and messages now show its real path |

Later, through the interface of the installed Flatpak, with *Descargas* chosen earlier:

| Step | Result |
|---|---|
| "Saved" message | Shows `/home/dark/Descargas/2026-10-23 16-35-32.mp4`, the real path |
| Clicking it | Files opens *Descargas* with the new recording selected |
| *File › Show Recordings* | Files opens *Descargas* |
| Recording Path setting | Shows the real path and cannot be typed into; *Browse* still changes it |
| Bug found and fixed | The setting showed `/home/dark/Descargas/`, with a trailing slash (test added; it fails without the fix) |

Not verified with the Flatpak: the camera permission dialog for a fresh install; a machine with a
GPU.

2026-10-23, idle CPU, with the preview of the screen and desktop audio on and nothing playing,
30 s per run after 12 s of start-up, `main` against the change that draws a silent, idle window
once a second and scales the preview faster:

| Machine | Before | After |
|---|---|---|
| Windows 11 laptop (the one above), 1080p display | 7.4 % of one core (two runs) | 3.9–4.3 % |
| Ubuntu VM without GPU (window drawn in software) | 37 % | 5.6 % |

Private memory did not change in the VM (55 MB) and rose 3–4 MB on Windows (99 to 102 MB). Most of
what is left is the preview: each picture is read back from the GPU (about 2.6 ms at 1080p) and
scaled down (5.4 ms before, 3.0 ms after, measured in a release build). A meter still reacts at
once when sound starts (checked with a tone after 10 s of silence) and falls back to −inf after.

2026-10-23, release preparation for 0.4.0 (`main` plus the version bump), Windows on the same
laptop, Linux in the same VM with the 0.4.0 Flatpak built from the release branch and installed from
its bundle:

| Step | Result |
|---|---|
| Automated tests and clippy | 43 passed on Windows, 49 on Linux (one timing test ignored on both); clippy clean on 1.95 and stable on both |
| Self-test 6 s 1080p60, 440 Hz tone playing (Windows) | Quick Sync; 352 frames, 0 lagged, 5.87 s, audio −14.1 dB, 0 decode errors |
| Self-test with pause, 720p and `replay` (Windows) | 1280×720, 3.88 s, 233 frames, 0 lagged; replay 3.85 s; audio −14.1 dB |
| Interface flow (Windows) | 5.48 s at 1080p60 with the tone at −14.1 dB; clicking the "saved" message opened Explorer with the file selected; *File › Show Recordings* opened *Videos* |
| Hotkeys (Windows) | `Ctrl+Alt+R`, `Ctrl+Alt+P` twice and `Ctrl+Alt+R` with VSCodium focused, 2 s apart: 4.67 s file, pause excluded |
| Replay buffer (Windows) | Started with the button; meter at −11 dB with the tone; saved replay 17.6 s, first frame a keyframe, audio −14.0 dB, 0 decode errors |
| Installer | Not run on the test machine: its installed 0.3.0 makes `installer/test.ps1` stop, and a local 0.4.0 MSI would stand in the way of the released one. The CI `installer` job (install, upgrade, uninstall) passed on `main` |
| Release workflow, run by hand on `main` (`c09b40a`) | `windows`, `flatpak` and `checksums` jobs passed; `sha256sum --check SHA256SUMS` passes for the zip, the MSI and the bundle; the CI-built bundle installs in the VM and has `LICENSE` and the Linux `THIRD-PARTY-LICENSES.html` (GStreamer included) |
| Flatpak version | `flatpak info` shows 0.4.0; `appstreamcli validate` passes |
| Self-test inside the Flatpak, 8 s with `replay`, tone playing | x264; 1280×720 at 60 fps, 479 frames, 0 lagged, audio −13.5 dB; replay 3.90 s; 0 decode errors |
| After `flatpak permission-reset` | The sharing dialog names the app ("Obs Lite quiere compartir su pantalla"). The camera, configured but no longer allowed, asked while that dialog had the focus and got "cancelled"; choosing it again showed *Camera* in the menu and *Allow Obs Lite to Use the Camera?*; *Deny* showed an error, and allowing it afterwards (as the privacy settings do) brought the overlay back |
| Bug found and fixed | That error read "Portal request was cancelled"; it now says to choose the camera again or allow it in the privacy settings (#26, test added) |
| Recording to the folder chosen earlier with *Browse* | 4.67 s, 1280×720, in `~/Descargas`, 0 decode errors |

Not repeated for 0.4.0: the Windows checks whose code did not change since 0.3.0 (the preview
scaling did change, but gives the same pixels and has a test); the mix limit on Windows (it needs
two sound inputs; checked in the VM); the tray menu on Windows (unchanged); the Flatpak on a machine
with a GPU.

2026-10-23, after 0.4.0's checks: dependency updates and no preview pictures while the window is not
shown. Idle CPU with the screen preview and desktop audio on, `main` against the change:

| Check | Result |
|---|---|
| Hotkeys with `global-hotkey` 0.9 (Windows) | `Ctrl+Alt+R`, `Ctrl+Alt+P` twice and `Ctrl+Alt+R` with VSCodium focused: 4.60 s file, pause excluded |
| Windows, minimized | 1.9–2.6 % of one core before, 1.1–1.6 % after (six runs) |
| Windows, shown | The same before and after (the screen was busy, so about 21 % in both) |
| Linux VM (Flatpak), minimized with Super+H | The preview stopped 3.3 s after minimizing and came back 0.4 s after the window was shown again. CPU 4.0 % minimized against 4.1 % before the change: on Linux the preview pictures cost little, and the window stops being drawn either way |
| Linux, frames reaching the capture | About 2 a second while idle, as GNOME sends a new one only when the screen changes; asking PipeWire for at most 5 a second gave 4.7 and more CPU, so it was not done |

2026-10-24, the update dialog for 0.4.1, on the Windows laptop with a build numbered 0.3.9 so that
the real check against GitHub found 0.4.0:

| Check | Result |
|---|---|
| Start-up | The dialog said "Ya está disponible obs-lite 0.4.0. Tienes la 0.3.9." and the status bar showed the link |
| *Descargar* | Opened the v0.4.0 release page in the browser and closed the dialog |
| *Cerrar* | Closed it; it came back at the next start |
| *No recordar esta versión* | Closed it and saved `"skipped_version": "0.4.0"`; at the next start no dialog, the status-bar link still there |
| English and Spanish, after the layout fix | Title, text and buttons line up in both (in English the buttons used to spill past the left edge) |

Not tested: opening Settings before GitHub answers (the dialog then waits for Settings to close).

2026-10-24, release preparation for 0.4.1 (`main` plus the version bump). Since 0.4.0 only the
update dialog and the version changed:

| Step | Result |
|---|---|
| Automated tests and clippy | 46 passed on Windows, 52 on Linux (one timing test ignored on both); clippy clean on 1.95 and stable on both |
| Self-test 6 s 1080p60 (Windows) | Quick Sync; 350 frames, 0 lagged, 5.85 s, first frame a keyframe, audio −35 dB (whatever was playing), 0 decode errors |
| Self-test with pause, 720p and `replay` (Windows) | 1280×720, 3.88 s, 233 frames, 0 lagged; replay 3.87 s; 0 decode errors |
| Self-test 6 s with `replay` (Linux VM, release build outside the Flatpak) | x264; 1280×720, 5.88 s, 353 frames, 0 lagged; replay 3.82 s; 0 decode errors |
| Finding, not new in 0.4.1 | In the VM, with nothing moving on the screen, the self-test failed with "no frames captured": GNOME sent no frame in 6 s. With the pointer moving it passed (the run above). The capture code is the same as in 0.4.0 |

Not repeated for 0.4.1: the interface flow, hotkeys, the replay buffer from the interface, the
installer on the test machine (the CI `installer` job covers it) and the Flatpak bundle itself
(the CI `flatpak` job builds it), as none of that changed.

2026-10-25, application audio and desktop audio together, on the Windows laptop: `ffplay` playing
a 1 kHz tone, captured as application audio, while PowerShell played a 440 Hz tone; 4.9 s
recordings from the interface with desktop audio on its default device, 0.4.1 against this change:

| Application channel | 0.4.1: 1 kHz / 440 Hz | Change: 1 kHz / 440 Hz |
|---|---|---|
| On | −30.8 / −31.2 dBFS (the application twice) | −33.7 / −31.1 dBFS |
| Muted | −33.7 / −31.1 dBFS (still heard, through desktop audio) | −88.3 / −31.1 dBFS (gone) |

The application's two copies in 0.4.1 arrive slightly apart, so a steady tone partly cancels
(+2.9 dB rather than +6 dB). With `ffplay` started after obs-lite, its channel muted, the change
recorded 1 kHz at −97.1 dBFS and 440 Hz at −31.1 dBFS: desktop audio switches when the recording
starts. Not tested: a chosen output device (left as before by design).

2026-10-25, recordings written in fragments, made regular when they stop. Windows on the laptop
(Media Foundation), Linux in the VM (GStreamer), self-tests:

| Check | Windows | Linux |
|---|---|---|
| Normal stop, 8 s with `replay` | 7.88 s, 473 frames; replay 3.87 s; one index at the end, old ones as padding | 7.97 s, 478 frames; replay 3.90 s; the same layout |
| Killed (`taskkill /F`, `kill -9`) partway | Plays: 5.72 s, 342 frames, 0 decode errors | Plays: 12.5 s, 733 frames, 0 decode errors, last fragment cut |
| Packets of a 30 s fragmented file and of its regular copy (`ffmpeg -f framemd5`) | 3194 and 3194, identical (data and times) | 850 and 850 (8 s), identical |
| Conversion time; size | 1.3 ms for 30 s; +7 KB (0.07 %) | — |
| Warnings from `ffprobe -v warning` on the regular files | None | None |

Windows' own media stack (`WMPlayer.OCX`) reads the regular files' durations; for a killed
recording it reports 0 s, as there is no total duration in a fragmented file. With mp4mux's own
`first-moov-then-finalise` mode, a recording killed partway did not open, so it is not used. Not
tested: a recording over 4 GiB end to end (the 64-bit offsets have a unit test), and opening the
files in video editors (none on the test machines).

2026-10-25, repairing a recording cut short, from the interface: a recording started with the
button, obs-lite killed after about 6–8 s, then started again:

| | Windows (`taskkill /F`) | Linux VM (`kill -9`) |
|---|---|---|
| Left by the kill | Fragmented, 19 fragments | Fragmented, 34 fragments, the last box cut |
| After starting again | Regular: 5.72 s, 342 frames, no `ffprobe` warnings | Regular: 8.34 s, 480 frames, 0 decode errors |
| Status bar | "Se recuperó la grabación interrumpida '…'" | The same |

Not tested: a power cut itself (the kill stands in for it; with a power cut the disk may also lose
what was not yet flushed).

2026-10-25, a window resized while recording, on the Windows laptop: an `ffplay` window with a
moving test pattern, recorded at its first size (946×532), then resized every 3 s with
`SetWindowPos`; one frame from each size, this change against 0.4.1:

| Window | Change | 0.4.1 |
|---|---|---|
| 1600×500 | The whole window, scaled down, bands above and below | Only its left part |
| 640×720 | The whole window, scaled down, bands at the sides | Its top part, black on the right |
| 480×270 | Centred at its own size | In the top-left corner |
| Back to 960×540 | As at the start | As at the start |

The recording kept 60 fps throughout (940 frames in 15.67 s), with no decode errors. Not
tested: a GPU whose video processor cannot scale (the code then falls back to cropping), and
dragging a window's edge, which resizes it on every frame.

2026-10-25, memory, Windows laptop, private memory of the process (Rust's own heap measured with
a counting allocator in a build made for it):

| State | Private memory | Rust heap |
|---|---|---|
| Idle, window shown | about 100 MB | 4.4 MB |
| Recording 1080p60 | about 260–275 MB | 4.4 MB |
| Replay buffer of 20 s | about 340 MB | up to 11 MB |

Nearly all of it is the GPU driver, OpenGL, Media Foundation and Windows capture, not obs-lite's
own data. Five 8 s recordings in a row, from the interface:

| | Before | After the fix |
|---|---|---|
| 8 s after each recording | 248, 363, 486, 608, 710 MB | 124, 110, 102, 108, 102 MB |
| Idle after the five, a minute later | 676 MB | 102 MB |

Bug found and fixed: each hardware encoder kept the recording's D3D device, with its textures,
until `ShutdownObject` (never called): about 120 MB per recording. The ignored test above grew
37 MB per round without the fix (147 MB over four) and fails; with `ShutdownObject` it passes.
Releasing the encoder's D3D manager alone did not help, so it is not done.

2026-10-25, release preparation for 0.5.0 (`main` plus the version bump). Since 0.4.1: recordings
written in fragments and repaired after a crash, a resized window scaled on Windows, the encoder
leak fixed and application audio no longer repeated; each was tested from the interface in its
own entry above.

| Step | Result |
|---|---|
| Automated tests and clippy | 54 passed on Windows (two ignored), 59 on Linux (one ignored); clippy clean on 1.95 and stable on both |
| Self-test 6 s 1080p60 (Windows) | Quick Sync; 352 frames, 0 lagged, 5.87 s, regular MP4, 0 decode errors, no `ffprobe` warnings |
| Self-test with pause, 720p and `replay` (Windows) | 1280×720, 3.88 s, 233 frames; replay 3.85 s; both regular MP4, 0 decode errors |
| Self-test killed after 6 s (Windows, `taskkill /F`) | Fragmented MP4 that plays: 5.72 s, 342 frames, 0 decode errors |
| Self-test 8 s with `replay` (Linux VM) | x264; 472 frames, 0 lagged, 7.87 s; replay 3.78 s; both regular MP4, 0 decode errors |
| Self-test killed partway (Linux VM, `kill -9`) | Fragmented MP4, last fragment cut, that plays: 12.5 s, 733 frames, 0 decode errors |

Not repeated for 0.5.0: the checks of 0.4.1 whose code did not change (hotkeys, the tray menu, the
webcam, the installer on the test machine and the Flatpak bundle itself; CI's `installer` and
`flatpak` jobs build and test them).

Not tested at all: NVIDIA (with this engine) and AMD encoders, Windows 10, multiple monitors,
HDR displays.

2026-10-27, Linux on a laptop with Intel UHD (Raptor Lake) and RTX 4060 GPUs: Pop!_OS 24.04 with
COSMIC on Wayland, NVIDIA driver 595.104.02, 1920×1080 at 144 Hz. The 0.5.0 Flatpak could not record:
the VA-API and NVENC encoders take only NV12, and obs-lite gave them I420.

| Step | Result |
|---|---|
| Each encoder in the Flatpak (GStreamer 1.28), 120 frames 1080p60 of a test pattern | `vah264enc`, `vah264lpenc` and `nvh264enc` fail to link with I420 and encode with NV12; `x264enc` encodes with both |
| Automated tests and clippy | 60 passed (one ignored) on 1.95 and stable; clippy clean on both for Linux; the new test fails with I420 (`vah264lpenc does not take I420`) |
| Self-test 10 s 1080p60, Flatpak built from the change | `vah264enc` (Intel); 581 frames, 0 lagged, 9.68 s, H.264 High + AAC-LC, 0 decode errors |

Not verified: recording with NVENC through obs-lite (VA-API comes first when both are present; only
the 120-frame test above used NVENC); AMD. The system's GStreamer 1.24 `nvh264enc` (Ubuntu 24.04)
fails with this driver ("Selected preset not supported") with either format.

2026-10-27, same laptop, an encoder that is installed but fails at its first picture:

| Step | Result |
|---|---|
| The system's GStreamer 1.24 without the VA-API encoders | `nvh264enc` fails ("Selected preset not supported"); `openh264enc` chosen in 0.63 s |
| Trying one 1080p picture with all encoders (system GStreamer) | `vah264lpenc` chosen in 62 ms the first time, 13–14 ms the next two |
| Automated tests and clippy | 61 passed (one ignored) on 1.95 and stable; clippy clean on both for Linux; the new test fails when the encoders are not tried |
| Self-test 10 s 1080p60, Flatpak built from the change | `vah264enc`; 562 frames, 0 lagged, 9.38 s, 0 decode errors |

2026-10-27, same laptop, COSMIC's screen-sharing portal crashing (xdg-desktop-portal-cosmic 1.11.0:
"create_frame sent before the previous frame was destroyed", then the process exits):

| Step | Result |
|---|---|
| What the app sees (`dbus-monitor`) | The stream ends and the portal's backend leaves the bus; no `Closed` signal reaches the session |
| Self-test 60 s 1080p60, Flatpak built from the change, mouse moving | The portal crashed at 46.05 s; capture back at 46.65 s from the remembered choice, without the dialog. 59.45 s file, 3567 frames, 0 lagged, 0 decode errors |
| A shared window closed, from the interface | "The source is no longer available…" and nothing else: capture not started again |
| Automated tests and clippy | 60 passed (one ignored) on 1.95 and stable; clippy clean on both for Linux |

Not verified: stopping sharing from the desktop (COSMIC has no control for it; GNOME has one),
GNOME and other desktops with this change.

2026-10-27, same laptop, a recording that cannot be written:

| Step | Result |
|---|---|
| Automated tests | 66 passed (one ignored) on 1.95 and stable. New: a file that cannot be created is tried once and fails the recording at once; a recording or replay whose writes fail is still finished; an MP4 written to `/dev/full` (every write fails, as on a full disk) reports it on a later write and finishes in under 5 s. Each fails on the old code |
| Self-test 8 s to `/dev/full`, Flatpak built from the change | "No space left on the resource" after 8.2 s; the first build of the change took 68 s, waiting for an end of stream that never came, which is fixed |
| Self-test 8 s 1080p60 to a normal file | `vah264enc`; 436 frames, 0 lagged, 7.27 s, 0 decode errors |

Not verified: the interface stopping the recording at once (no way to check the window from here);
an encoder failing mid-recording on Linux (seen only with the system's GStreamer 1.24
`vah264lpenc` on random noise, outside obs-lite); any of it on Windows.

2026-10-27, fixes from a full-code audit (#53–#60), on the same laptop; Flatpak built from `main`
with all of them:

| Step | Result |
|---|---|
| Automated tests and clippy | 80 passed (one ignored) on 1.95 and stable on Linux; clippy clean on both for Linux and, cross-checked from Linux, for Windows. New tests each fail on the old code where there was one: output names, typed paths, the instance lock, settings written whole, the MP4 sync order and an interrupted conversion, the Linux language, the Flatpak folder check, the Escape and tray rules in Settings, duplicate hotkeys, the camera mode, the self-test length, and a stall in the mixer (50 ms of delay stayed before; within 10 ms after) |
| Instance lock | With obs-lite open, `flock -n` on `instance.lock` from outside fails; after closing it succeeds |
| Mixer, 440 Hz tone on the desktop, self-test 15 s | 14.67 s, no gaps (`silencedetect` −40 dB, 5 ms), energy above 2 kHz at −83 dB against −31.5 dB for the tone |
| Camera overlay | Shows in the preview (with `on-disconnect=error`) |
| A typed folder outside the Flatpak (`~/Documentos/prueba`) | Recording refused with the message to choose the folder with *Browse* |
| Escape while waiting for a hotkey | Ends the wait; Settings stays open |
| A hotkey already set for another action | Taken from it: the other shows "(none)" |

Not verified: anything on Windows (no machine available), notably the audio capture rewrite
(#57), the encoder error handling (#52) and the webcam mode and format changes (#58); unplugging
a camera or an audio device; clearing hotkeys on a desktop; a real power cut during a conversion.

2026-10-28, recording on the GPU with VA-API, same laptop (Intel iHD 26.3.5 in the Flatpak runtime,
GStreamer 1.28; the system has GStreamer 1.24 and iHD 24.1). The build was run inside the Flatpak
runtime (`flatpak run --command=`) and natively:

| Step | Result |
|---|---|
| Self-test 8 s 1080p60, runtime | `vah264enc`, frames on the GPU; 444 frames, 60 fps, BT.709 (limited range) signalled, 0 decode errors; colours right |
| Self-test with `preview shot 720`, runtime | 26 preview pictures of 640×360; a 1280×720 screenshot with the right colours |
| Self-test with `pause replay`, runtime | 5.50 s for 9 s with 3 s paused, 330 frames; replay 3.42 s; 0 decode errors in both |
| Self-test with `preview shot`, natively | `vah264lpenc`, frames on the GPU; 359 frames, 0 decode errors; preview and a 1920×1080 screenshot |
| CPU and memory, self-test 40 s 1080p60 measured for 20 s, 3 runs each, runtime | 11.6 / 11.2 % of a core (third run dropped: the portal crashed during it; the recording went on, 2370 frames, 0 lagged) and 140 MB, against 42.6 / 41.6 / 43.6 % and 165 MB for `main` |
| The preview made from the desktop's frame instead of the recording's | Intel's driver crashed (VEBOX) or the heap was corrupted within seconds, with one VA-API display or two: fixed by scaling the recording's surface |
| Automated tests and clippy | 84 passed (one ignored) on 1.95 and stable natively, and the three new ones also inside the runtime; clippy clean on both for Linux and Windows. The new GPU test fails on the code before the preroll fix (a pipeline whose two sinks waited on each other) |

Not verified: AMD; GNOME, KDE and other desktops (they may send other formats or layouts, which
fall back to frames in memory); a window capture changing size while recording (scaled to fit
with borders on the GPU, where frames in memory are cropped or padded); the interface (only the
self-test was run).

2026-10-29, the region and the webcam on the GPU, same laptop, the build run inside the Flatpak
runtime. COSMIC's portal crashed every 10–20 s that day (also with `main`), and Steam was open, so
CPU was measured in 5 s windows, dropping those with a crash, and compared only within the session:

| Step | Result |
|---|---|
| Self-test 6 s with `preview shot` and `region`, `webcam`, and both at 720p | 960×540, 1920×1080 and 960×540 files, 348–385 frames, 0 lagged, 0 decode errors; the region is the middle of the screen, the magenta stand-in webcam is in the bottom-right corner, with and without the region; the screenshots are the whole 1920×1080 screen, as the app crops and pastes on them |
| Preview pictures | 26 in 6 s (5 a second) once whole frames were kept 10 times a second; 19–21 with 5 |
| CPU, `preview region webcam` | 20.7 / 22.5 / 27.1 % of a core and 185 MB on the GPU, against 40.7 / 44.5 % and 152 MB with the same build taking frames in memory. Recording alone on the GPU 12.4–14.3 %; the same in memory, and `main`, 81–104 % that day |
| Per thread, recording on the GPU | The encoder's thread about 9 % of a core, converting 3 %, PipeWire 1.5 %; the preview adds about 2 % |
| Automated tests and clippy | 87 passed (one ignored) on 1.95 and stable natively, and the GPU ones also inside the runtime; clippy clean on both for Linux and Windows |

Not verified: a real webcam (only the stand-in picture of the self-test; the app gives the encoder
the same scaled pictures as before); the interface; AMD; other desktops. With a region, memory is
higher on the GPU (185 against 152 MB): whole frames are kept there for the preview, where in
memory only the region is.

2026-10-29, fixes from a review of the GPU path by five agents (four reviewers, one trying to refute
each finding; 7 findings, 3 confirmed), same laptop:

| Step | Result |
|---|---|
| Confirmed: an error in the crop, the webcam or the conversion of the first frame went unnoticed | The recording ran without video until stopped, with no fallback. Now the first surface is waited for (at most 3 s) before recording starts, and frames are taken in memory if it does not come |
| Confirmed: a window that shrank past the region recorded the whole window | Now nothing new is recorded while it is past it: the last picture stays (in memory the picture is black) |
| Confirmed: a webcam picture under 16 px a side (a small region) stopped the recording | A larger copy goes to the compositor, at the same place |
| Rejected | A capture that ends cleanly before its first frame (speculative); the capture's frame rate on desktops sending frames only on damage (older than these changes); black frames from linear DMA-BUFs of another GPU (speculative on this path) |
| Automated tests and clippy | 89 passed (one ignored) on 1.95 and stable natively, 3 runs; the GPU tests 5 runs in parallel inside the runtime; clippy clean on both for Linux and Windows. Two tests recording with the webcam at once had written to the same file, which made them fail or hang: each now has its own |
| Self-test 8 s, plain and with `region webcam preview shot pause replay` | 457 and 322 frames, 0 lagged, 0 decode errors |

Not verified: a driver on which the first frame really fails (the waiting was tested with pipelines
that refuse the frame or give nothing back).

2026-10-29, the figures for the README, same laptop, the Flatpak built from `main` (`a5c1179`),
little else running. Each program shown the whole screen (1920×1080) with its audio, then
recording it at 1080p60 with VA-API on the Intel GPU, 12 Mbit/s, a keyframe every 2 s, no
B-frames; CPU and memory of every process in the Flatpak's sandbox (`measure.py`), in 8–10 s
windows, dropping those with a crash of COSMIC's portal:

| | Idle (preview + audio) | Recording |
|---|---|---|
| obs-lite | 3.4–3.6 % of a core (0.21–0.22 % of 16 threads), 115 MB, 4 windows | 11.0–11.2 % (0.69–0.70 %), 214–227 MB, 7 windows (2 dropped) |
| OBS Studio 33.0 | 5.0–5.3 %, 279 MB, 4 windows | 8.7–9.0 % (0.54–0.56 %), 293–294 MB, 6 windows |

The recording obs-lite made from the interface: 2 min 41 s, 1920×1080, 60 fps, 9663 frames, 0
decode errors.

2026-10-29, pausing a recording without pausing the replay buffer (audit finding 12), same laptop,
the build run inside the Flatpak runtime:

| Step | Result |
|---|---|
| Self-test 9 s with `pause` (the middle 3 s), twice | 5.45 / 5.65 s files, 0 decode errors; every video frame 1/60 s after the one before, no gap at the pause; audio: one step of 384 or 1408 samples instead of 1024 where the file goes on (the audio and video cut points fall up to one audio packet apart). The first build left 50 ms of audio out before the pause (made before it but coming after it), now kept |
| Self-test 9 s with `pause replay` | File 5.45 s; replay 4.35 s; 0 decode errors |
| Automated tests and clippy | 90 passed (one ignored) on 1.95 and stable; clippy clean on both for Linux and Windows |

Not verified: Windows (the change is in the shared outputs, and the Windows encoder now keeps
encoding while paused, as on Linux); pausing from the interface (the self-test pauses the same way).

2026-10-29, release preparation for 0.6.0 (`main` plus the version bump), same laptop, the
Flatpak built from it (metainfo release 0.6.0):

| Step | Result |
|---|---|
| Self-test 8 s 1080p60 | `vah264enc`, frames on the GPU; 448 frames, 0 lagged, 7.47 s, 0 decode errors |
| Self-test 8 s with `pause replay` | 5.00 s file, 3.60 s replay, 0 lagged, 0 decode errors |
| Self-test 8 s with `region webcam preview shot` | 960×540, 489 frames, 0 lagged, 0 decode errors; 38 preview pictures; a 1920×1080 screenshot |
| Automated tests and clippy | 90 passed (one ignored) on 1.95 and stable; clippy clean on both for Linux and Windows |
| Flathub's linter | The metainfo passes `flatpak-builder-lint appstream`; the Flathub manifest passes `manifest` and builds offline with `org.flatpak.Builder` |

Not verified for this release: any of the Windows changes since 0.5.0 on Windows (no machine
available; only CI and cross-compiled clippy), notably the audio capture rewrite, the encoder
error handling, the webcam mode changes and pausing without pausing the replay buffer.

2026-10-29, the 0.6.0 release build on a fresh Windows 11 (a VirtualBox virtual machine): it did
not start, "VCRUNTIME140.dll was not found". The build linked the Visual C++ runtime as a DLL,
which only other programs install. Now the runtime is linked into the exe (`crt-static`), and the
CI installer job and the release build check with `dumpbin /dependents` that the exe imports no
`vcruntime*`, `msvcp*` or `api-ms-win-crt-*` DLL; the check failed on the build before the fix,
listing VCRUNTIME140.dll and six `api-ms-win-crt-*` DLLs.

Same virtual machine (VirtualBox, VBoxSVGA with 3D acceleration, no hardware encoder), the build
with the runtime inside: it starts and captures. The self-test used the Windows software encoder
("H264 Encoder MFT") and encoded 2 frames in 6 s at 1920×968, 335 slots lagged; a recording from
the interface lost about 70 % of its frames and was green, with nothing in it. The conversion to
NV12 on the GPU (`VideoProcessorBlt`) had its error ignored, so frames it did not write went to
the encoder empty; now that error stops the recording and says so. Not yet known: whether
VirtualBox's driver reports the error or writes nothing without one.
