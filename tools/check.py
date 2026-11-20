import argparse
import hashlib
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory
import tomllib
from zipfile import ZIP_DEFLATED, ZipFile


ROOT = Path(__file__).resolve().parents[1]
TARGET = ROOT / "target"
EXE = TARGET / "release" / "opnoverlay.exe"


def run(*args, env, timeout=600):
    executable = shutil.which(args[0], path=env["PATH"])
    if executable is None:
        raise FileNotFoundError(f"Command not found: {args[0]}")
    print(subprocess.list2cmdline([str(arg) for arg in args]), flush=True)
    with subprocess.Popen([executable, *args[1:]], cwd=ROOT, env=env) as process:
        try:
            status = process.wait(timeout=timeout)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
    if status != 0:
        raise subprocess.CalledProcessError(status, args)


def rust_tests(env, *args):
    env = env | {"CARGO_PROFILE_TEST_DEBUG": "0", "CARGO_INCREMENTAL": "0"}
    run("cargo", "test", "--locked", "--quiet", *args, env=env)


def build(env):
    run("cargo", "build", "--locked", "--release", env=env)


def png_dimensions(path):
    with path.open("rb") as png:
        header = png.read(24)
        if len(header) != 24 or header[:16] != b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR":
            raise ValueError(f"Invalid PNG header: {path.name}")
        width, height = struct.unpack(">II", header[16:])
        png.seek(-12, os.SEEK_END)
        if not width or not height or png.read() != b"\x00\x00\x00\x00IEND\xaeB`\x82":
            raise ValueError(f"Incomplete PNG: {path.name}")
    return width, height


def check_media(env, path, *, expect_tone=False, changing=False, separate_audio=False):
    checks = {
        "OPNOVERLAY_CHECK_FILE": str(path.resolve(strict=True)),
        "OPNOVERLAY_CHECK_VIDEO_CONTENT": "1",
    }
    if expect_tone:
        checks["OPNOVERLAY_EXPECT_SOUND"] = "1"
    if separate_audio:
        checks["OPNOVERLAY_EXPECT_SEPARATE_AUDIO"] = "1"
    if changing:
        checks["OPNOVERLAY_CHECK_VIDEO_CHANGES"] = "1"
        checks["OPNOVERLAY_EXPECT_VIDEO_MOTION"] = "1"
    rust_tests(env | checks, "decode_captured_", "--", "--ignored", "--nocapture")


def smoke(env, args):
    if args.exe is None:
        build(env)
    executable = EXE if args.exe is None else args.exe.resolve(strict=True)
    TARGET.mkdir(exist_ok=True)
    with TemporaryDirectory(prefix="capture-check-", dir=TARGET) as directory:
        test_dir = Path(directory)
        test_env = env | {"OPNOVERLAY_CONFIG_DIR": directory, "TEMP": directory, "TMP": directory}
        recording = test_dir / "recording.mp4"
        replay = test_dir / "recording-replay.mp4"
        screenshot = test_dir / "recording-shot.png"
        flags = ["replay", "shot", "synthetic-tone"]
        if args.disk_replay:
            flags.append("replay-disk")
        if args.light_profile:
            flags.append("light-profile")
        if args.dxgi:
            flags.append("dxgi")
        if args.separate_audio:
            flags.append("separate-audio")
        run(executable, "--selftest", str(args.seconds), recording, *flags, env=test_env,
            timeout=args.seconds + 25)
        width, height = png_dimensions(screenshot)
        print(f"Screenshot: {width}x{height}, {screenshot.stat().st_size} bytes", flush=True)
        for video in (recording, replay):
            check_media(test_env, video, expect_tone=True, separate_audio=args.separate_audio)
        if args.separate_audio:
            rust_tests(test_env | {"OPNOVERLAY_CHECK_FILE": str(recording)}, "trim_preserves_three_audio_tracks", "--", "--ignored", "--nocapture")
        if args.save_screenshot:
            with args.save_screenshot.open("xb") as output, screenshot.open("rb") as source:
                shutil.copyfileobj(source, output)


def release(env, out):
    for command in ("cargo-about", "wix", "powershell.exe"):
        if shutil.which(command, path=env["PATH"]) is None:
            raise FileNotFoundError(f"{command} not found. Add its directory using --tool-dir.")
    rust_tests(env)
    build(env)
    powershell = ("powershell.exe", "-NoProfile", "-File")
    run(*powershell, "installer/check-runtime.ps1", env=env)
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["package"]["version"]
    name = f"opnoverlay-{version}-windows-x64.msi"
    portable_name = f"opnoverlay-{version}-windows-x64-portable.zip"
    out.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(prefix="package-", dir=out) as directory:
        stage = Path(directory)
        shutil.copy2(EXE, stage)
        shutil.copy2(ROOT / "LICENSE", stage)
        run("cargo", "about", "generate", "--locked", "--fail", "about.hbs", "-o",
            stage / "THIRD-PARTY-LICENSES.html", env=env)
        package = stage / name
        run(*powershell, "installer/build.ps1", "-BinDir", stage, "-Out", package,
            "-Version", version, env=env)
        run("wix", "msi", "validate", package, env=env)
        portable = stage / portable_name
        with ZipFile(portable, "w", compression=ZIP_DEFLATED) as archive:
            archive.writestr("opnoverlay.portable", "")
            for filename in ("opnoverlay.exe", "LICENSE", "THIRD-PARTY-LICENSES.html"):
                archive.write(stage / filename, filename)
            for filename in ("README.md", "README.en.md"):
                archive.write(ROOT / filename, filename)
            presentmon = ROOT / "third-party" / "presentmon"
            archive.write(presentmon / "PresentMon.exe", "PresentMon.exe")
            archive.write(presentmon / "LICENSE.txt", "PRESENTMON-LICENSE.txt")
        with ZipFile(portable) as archive:
            if bad_file := archive.testzip():
                raise ValueError(f"Invalid file in portable ZIP: {bad_file}")
        manifest = stage / f"opnoverlay-{version}-SHA256.txt"
        with manifest.open("w", encoding="ascii", newline="\n") as checksums:
            for artifact in (package, portable):
                with artifact.open("rb") as file:
                    digest = hashlib.file_digest(file, "sha256").hexdigest()
                print(f"{digest}  {artifact.name}", file=checksums)
                print(f"{digest}  {out / artifact.name}")
        for artifact in (package, portable, manifest):
            artifact.replace(out / artifact.name)


def main():
    parser = argparse.ArgumentParser(description="Build and check opnoverlay locally on Windows.")
    parser.add_argument("--tool-dir", type=Path, action="append", default=[], help="Prepend a tool directory to PATH.")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("quick", help="Run Rust tests with a compact build cache.")
    commands.add_parser("build", help="Build the optimized Windows executable.")
    capture = commands.add_parser("smoke", help="Capture the current monitor briefly and validate MP4, replay and PNG.")
    capture.add_argument("--exe", type=Path, help="Use an already built executable.")
    capture.add_argument("--seconds", type=int, choices=range(5, 61), default=6, metavar="5..60")
    capture.add_argument("--disk-replay", action="store_true")
    capture.add_argument("--light-profile", action="store_true", help="Use the 720p / 30 FPS light profile.")
    capture.add_argument("--dxgi", action="store_true", help="Use DXGI Desktop Duplication for the monitor capture.")
    capture.add_argument("--separate-audio", action="store_true", help="Verify the mix, game, and microphone tracks.")
    capture.add_argument("--save-screenshot", type=Path, help="Keep the screenshot at a new path.")
    media = commands.add_parser("media", help="Decode an existing MP4 and check its video and audio.")
    media.add_argument("path", type=Path)
    media.add_argument("--expect-tone", action="store_true", help="Verify that the test tone is present.")
    media.add_argument("--changing", action="store_true", help="Require a visibly changing image.")
    media.add_argument("--separate-audio", action="store_true", help="Verify three tracks using both test tones.")
    package = commands.add_parser("release", help="Run Rust checks and package an MSI and portable ZIP.")
    package.add_argument("--out", type=Path, default=TARGET / "packages")
    args = parser.parse_args()

    if os.name != "nt":
        parser.error("this tool requires Windows")
    try:
        tools = [str(path.resolve(strict=True)) for path in args.tool_dir]
        env = os.environ | {"CARGO_TARGET_DIR": str(TARGET), "PATH": os.pathsep.join([*tools, os.environ["PATH"]])}
        if args.command == "quick":
            rust_tests(env)
        elif args.command == "build":
            build(env)
        elif args.command == "smoke":
            smoke(env, args)
        elif args.command == "media":
            check_media(env, args.path, expect_tone=args.expect_tone, changing=args.changing, separate_audio=args.separate_audio)
        else:
            release(env, args.out.resolve())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Check failed: {error}\n")


if __name__ == "__main__":
    main()
