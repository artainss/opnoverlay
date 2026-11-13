# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through
[GitHub private vulnerability reporting](https://github.com/25Dark25/ObsLite/security/advisories/new).
**Do not open a public issue.**

Please include the obs-lite version (or commit), your Windows version or Linux distribution and
desktop, the steps to reproduce and what an attacker gains. obs-lite is maintained by one person on
a best-effort basis, so there is no guaranteed response time; you will get an answer in the
advisory thread.

## Supported versions

Only the latest release receives fixes.

## Scope

obs-lite is a local desktop application. On Windows it runs with the privileges of the user who
starts it and installs nothing. Its only network connection is an optional update check at
start-up: one HTTPS request to `api.github.com` for the latest release, which is shown as a link
and never downloaded (Settings › General turns it off). It reads two inputs it does not fully
control, `%APPDATA%\obs-lite\settings.json` and that update response, and writes MP4 and PNG files
to the folder chosen in Settings. At start-up it also repairs the last recording named in
`settings.json` if it was cut short: it reads and rewrites that file, and may shorten it.

On Linux it runs in a Flatpak sandbox without network access. Its permissions are listed, with the
reason for each, in `packaging/flatpak/io.github._25Dark25.ObsLite.yml`: the display, the GPU,
the sound server (desktop audio and microphone), the *Videos* folder and the tray. The screen, the
webcam, global hotkeys and other folders go through desktop portals, which ask the user. It reads
its `settings.json`, repairs the last recording and writes MP4 and PNG files in the same way, and
reads the token the screen-sharing portal gives to restore the last choice.

In scope:

- Code in this repository and the release files published on its Releases page.
- A crafted `settings.json` that causes memory corruption or code execution.
- A crafted MP4 at the last recording's path that causes memory corruption when it is repaired.
- Recording something other than the selected source, recording after *Stop Recording*, or
  writing recordings or settings anywhere other than the configured paths.
- Flatpak permissions broader than the ones listed in the manifest, or used for something else.
- Release files whose signature or checksum does not match, or problems in the CI workflows that
  could let someone alter a release build.

Out of scope:

- An attacker who can already run code as the same user or as an administrator: they can read
  recordings and settings directly.
- That recordings are unencrypted files containing whatever was on screen; that is what the program
  is for.
- Bugs in Windows, Linux desktops and their portals, Flatpak, GPU drivers or third-party crates,
  unless obs-lite uses them incorrectly. Report those upstream; tell us if obs-lite needs to update
  or work around them.

## Audit status

obs-lite has not been audited. Most of the code was written with an AI coding assistant and has had
no independent review. It uses `unsafe` Rust to call Windows APIs directly (Direct3D 11, Media
Foundation, COM and a few Win32 functions), all in `src/win/`, plus one `getrusage` call in
`src/linux/sys.rs`. Dependencies are
checked for RustSec advisories and licenses with `cargo deny` on every pull request.
