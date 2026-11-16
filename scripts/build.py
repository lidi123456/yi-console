#!/usr/bin/env python3
"""Build local components or select installer packages on GitHub Actions."""

import argparse
import json
import os
import shlex
import shutil
import subprocess
import sys
import time
import tempfile
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LOCAL = ("frontend", "yi-agentd", "yi-relay", "yi-web", "plugins")
DAEMONS = [
    dict(os="ubuntu-latest", target="x86_64-unknown-linux-musl", name="linux-x86_64", cross=True),
    dict(os="ubuntu-latest", target="aarch64-unknown-linux-musl", name="linux-aarch64", cross=True),
    dict(os="macos-latest", target="x86_64-apple-darwin", name="macos-x86_64"),
    dict(os="macos-latest", target="aarch64-apple-darwin", name="macos-aarch64"),
    dict(os="windows-latest", target="x86_64-pc-windows-msvc", name="windows-x86_64", ext=".exe"),
]
DESKTOP = [
    dict(os="ubuntu-22.04", target="x86_64-unknown-linux-gnu", daemon="linux-x86_64"),
    dict(os="macos-latest", target="aarch64-apple-darwin", daemon="macos-aarch64"),
    dict(os="macos-latest", target="x86_64-apple-darwin", daemon="macos-x86_64"),
    dict(os="windows-latest", target="x86_64-pc-windows-msvc", daemon="windows-x86_64", ext=".exe"),
]
CI_PACKAGES = tuple(row["daemon"] for row in DESKTOP) + ("android", "daemons")
GROUPS = {
    "windows": ("windows-x86_64",),
    "macos": ("macos-aarch64", "macos-x86_64"),
    "linux": ("linux-x86_64",),
    "desktop": tuple(row["daemon"] for row in DESKTOP),
}


def expand(packages, ci=False):
    choices = CI_PACKAGES if ci else (*LOCAL, "android")
    groups = GROUPS if ci else {"backend": ("yi-agentd", "yi-relay", "yi-web")}
    selected = set()
    for package in packages:
        if package == "all":
            # Native Android builds require an SDK and are always an explicit selection.
            selected.update(choices if ci else LOCAL)
        elif package in groups:
            selected.update(groups[package])
        elif package in choices:
            selected.add(package)
        else:
            raise ValueError(f"Unknown {'CI' if ci else 'local'} package: {package}. "
                             "Run with --list to see available packages.")
    return [package for package in choices if package in selected]


def ci_plan(packages, only_daemons=False, windows_only=False):
    # Keep the old workflow inputs working; new selections are mutually exclusive with them.
    if packages and (only_daemons or windows_only):
        raise ValueError("packages cannot be combined with only_daemons or windows_only")
    selected = expand(packages or ["all"], ci=True)
    desktop = [row for row in DESKTOP if row["daemon"] in selected]
    android = "android" in selected
    daemons = DAEMONS if desktop or "daemons" in selected else []
    if windows_only:
        desktop = [row for row in desktop if row["os"] == "windows-latest"]
        daemons = [row for row in daemons if row["os"] != "macos-latest"]
        android = False
    if only_daemons:
        desktop = []
        android = False
    return dict(daemons=daemons, desktop=desktop, android=android)


def run(command, *, cwd=ROOT, dry_run=False, capture=False):
    command = [str(part) for part in command]
    # npm is a .cmd shim on Windows; resolve it before passing it to CreateProcess.
    command[0] = shutil.which(command[0]) or command[0]
    print(f"[{Path(cwd).relative_to(ROOT) or '.'}] {shlex.join(command)}", flush=True)
    if dry_run:
        return ""
    result = subprocess.run(command, cwd=cwd, check=True, text=True,
                            stdout=subprocess.PIPE if capture else None)
    return result.stdout if capture else ""


def android_build(args):
    app = ROOT / "app"
    project = app / "src-tauri/gen/android"
    targets = args.android_target or ["aarch64"]
    keystore = args.keystore or os.environ.get("ANDROID_KEYSTORE_PATH")
    tools = None
    if keystore and not args.debug:
        keystore = Path(keystore).expanduser().resolve()
        sdk = os.environ.get("ANDROID_HOME") or os.environ.get("ANDROID_SDK_ROOT")
        if not args.dry_run:
            if not keystore.is_file():
                raise ValueError(f"Keystore not found: {keystore}")
            if not os.environ.get("ANDROID_KEYSTORE_PASSWORD"):
                raise ValueError("Set ANDROID_KEYSTORE_PASSWORD to sign the APK")
            if not sdk:
                raise ValueError("Set ANDROID_HOME to the Android SDK directory")
            candidates = [p for p in (Path(sdk) / "build-tools").iterdir()
                          if all(part.isdigit() for part in p.name.split("."))]
            if not candidates:
                raise ValueError("No Android SDK build-tools found")
            tools = max(candidates, key=lambda p: tuple(map(int, p.name.split("."))))
    tauri = ["npm", "run", "tauri", "--", "android"]
    if not project.is_dir():
        run([*tauri, "init", "--ci"], cwd=app, dry_run=args.dry_run)
    activity = project / "app/src/main/java/dev/yiconsole/app/MainActivity.kt"
    print("Sync Android MainActivity.kt and launcher icons", flush=True)
    if not args.dry_run:
        activity.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(app / "src-tauri/android/MainActivity.kt", activity)
        shutil.copytree(app / "src-tauri/icons/android", project / "app/src/main/res", dirs_exist_ok=True)
    command = [*tauri, "build", "--apk", "--ci"]
    for target in targets:
        command.extend(("--target", target))
    if args.debug:
        command.append("--debug")
    started = time.time()
    run(command, cwd=app, dry_run=args.dry_run)
    if args.dry_run:
        print("APK output: out/ (signed when --keystore and ANDROID_KEYSTORE_PASSWORD are set)")
        return
    profile = "debug" if args.debug else "release"
    # Only collect an APK produced by this invocation, never an earlier successful build.
    apks = [p for p in (project / "app/build/outputs/apk").rglob("*.apk")
            if profile in p.parts and p.stat().st_mtime >= started - 2]
    if not apks:
        raise ValueError("Android build finished without a new APK")
    version = json.loads((app / "src-tauri/tauri.conf.json").read_text())["version"]
    output = ROOT / "out"
    output.mkdir(exist_ok=True)
    for apk in apks:
        suffix = "debug" if args.debug else "unsigned"
        if keystore and not args.debug:
            suffix = "signed"
        dest = output / f"yi-console_{version}_android-{'-'.join(targets)}_{apk.stem}-{suffix}.apk"
        if keystore and not args.debug:
            zipalign = tools / ("zipalign.exe" if sys.platform == "win32" else "zipalign")
            apksigner = tools / ("apksigner.bat" if sys.platform == "win32" else "apksigner")
            with tempfile.TemporaryDirectory(prefix="yi-android-") as directory:
                aligned = Path(directory) / "aligned.apk"
                run([zipalign, "-f", "4", apk, aligned])
                run([apksigner, "sign", "--ks", keystore, "--ks-pass", "env:ANDROID_KEYSTORE_PASSWORD",
                     "--ks-key-alias", args.key_alias, "--out", dest, aligned])
            run([apksigner, "verify", "--verbose", "--print-certs", dest])
        else:
            shutil.copy2(apk, dest)
        print(f"APK output ({suffix}): {dest}")


def local_build(args, packages):
    if not args.dry_run:
        required = {"cargo"} if any(p != "frontend" for p in packages) else set()
        if "frontend" in packages or "android" in packages:
            required.update(("node", "npm"))
        if "android" in packages:
            required.add("java")
        for command in sorted(required):
            if not shutil.which(command):
                raise ValueError(f"Required command not found: {command}")
    if "yi-agentd" in packages:
        target = args.target or run(["rustc", "-vV"], capture=True, dry_run=args.dry_run)
        if "windows" in target or (not args.target and sys.platform == "win32"):
            run([sys.executable, "scripts/prepare-computer-use.py"], dry_run=args.dry_run)
    crates = [p for p in packages if p.startswith("yi-")]
    if crates:
        command = ["cargo", "build", "--locked"]
        if not args.debug:
            command.append("--release")
        if args.target:
            command.extend(("--target", args.target))
        for crate in crates:
            command.extend(("-p", crate))
        run(command, dry_run=args.dry_run)
        target_dir = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
        print(f"Rust output: {target_dir / (args.target or '') / ('debug' if args.debug else 'release')}")
    if "frontend" in packages or "android" in packages:
        if args.npm_ci or not (ROOT / "app/node_modules").is_dir():
            run(["npm", "ci"], cwd=ROOT / "app", dry_run=args.dry_run)
        if "android" not in packages:
            run(["npm", "run", "build"], cwd=ROOT / "app", dry_run=args.dry_run)
            print(f"Frontend output: {ROOT / 'app/dist'}")
    if "plugins" in packages:
        run([sys.executable, "scripts/bundle-plugins.py"], dry_run=args.dry_run)
        print(f"Plugin output (always release): {ROOT / 'plugins/dist'}")
    if "android" in packages:
        android_build(args)


def ci_build(args, packages):
    plan = ci_plan(packages)
    print("CI packages: " + ", ".join(packages))
    print("Desktop installers also bundle all five daemon targets for SSH deployment."
          if plan["desktop"] else "No desktop installers selected.")
    ref = args.ref
    if not ref and not args.dry_run:
        try:
            ref = run(["git", "symbolic-ref", "--quiet", "--short", "HEAD"], capture=True).strip()
        except subprocess.CalledProcessError:
            raise ValueError("Cannot determine the current branch; use --ref <pushed-branch-or-tag>") from None
    ref = ref or "<current-branch>"
    build_id = uuid.uuid4().hex
    gh = ["gh"] + (["--repo", args.repo] if args.repo else [])
    run([*gh, "workflow", "run", "build.yml", "--ref", ref,
         "-f", f"packages={','.join(packages)}", "-f", f"build_id={build_id}"],
        dry_run=args.dry_run)
    if args.dry_run:
        if args.wait or args.download:
            run([*gh, "run", "watch", "<run-id>", "--exit-status"], dry_run=True)
        if args.download:
            run([*gh, "run", "download", "<run-id>", "--dir", args.download], dry_run=True)
        return
    # Match a unique title instead of accidentally watching someone else's concurrent dispatch.
    for _ in range(30):
        rows = json.loads(run([*gh, "run", "list", "--workflow", "build.yml", "--event",
                               "workflow_dispatch", "--limit", "50", "--json",
                               "databaseId,displayTitle,url"], capture=True))
        match = next((row for row in rows if row["displayTitle"] == f"build ({build_id})"), None)
        if match:
            break
        time.sleep(2)
    else:
        raise ValueError("Build dispatched, but its run did not appear within 60 seconds. "
                         "Check GitHub Actions for the run before retrying.")
    print(match["url"], flush=True)
    run_id = str(match["databaseId"])
    if args.wait or args.download:
        run([*gh, "run", "watch", run_id, "--exit-status"])
    if args.download:
        run([*gh, "run", "download", run_id, "--dir", args.download])


def main():
    parser = argparse.ArgumentParser(
        description=__doc__,
        epilog="Examples: build.py backend frontend; build.py --ci all --download out; "
               "build.py --ci windows android --ref main --dry-run")
    parser.add_argument("packages", nargs="*", help="package names, groups, or all (space/comma separated)")
    parser.add_argument("--ci", action="store_true", help="build installers on GitHub Actions (requires gh auth login)")
    parser.add_argument("--list", action="store_true", help="list local and CI package names")
    parser.add_argument("--dry-run", action="store_true", help="print commands without building or dispatching")
    parser.add_argument("--debug", action="store_true", help="use the local Rust debug profile (default: release)")
    parser.add_argument("--target", help="local Rust target triple; linker/toolchain must already be installed")
    parser.add_argument("--npm-ci", action="store_true", help="reinstall frontend dependencies with npm ci")
    parser.add_argument("--android-target", action="append", choices=("aarch64", "armv7", "x86_64", "i686"),
                        help="local APK architecture; repeat for a universal APK (default: aarch64)")
    parser.add_argument("--keystore", help="local release APK signing keystore (or ANDROID_KEYSTORE_PATH)")
    parser.add_argument("--key-alias", default=os.environ.get("ANDROID_KEY_ALIAS", "app"),
                        help="APK signing key alias (default: ANDROID_KEY_ALIAS or app)")
    parser.add_argument("--ref", help="pushed CI branch or tag (default: current branch; local edits are not uploaded)")
    parser.add_argument("--repo", help="CI GitHub repository OWNER/REPO (default: current remote)")
    parser.add_argument("--wait", action="store_true", help="wait for the CI run and propagate failure")
    parser.add_argument("--download", metavar="DIR", help="wait and download CI artifacts into DIR")
    parser.add_argument("--plan-ci", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    packages = [part.strip() for value in args.packages for part in value.split(",") if part.strip()]
    try:
        if args.plan_ci:
            plan = ci_plan(packages, os.environ.get("ONLY_DAEMONS") == "true",
                           os.environ.get("WINDOWS_ONLY") == "true")
            with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
                for key, value in plan.items():
                    output.write(f"{key}={json.dumps(value, separators=(',', ':'))}\n")
            return
        if args.list:
            print("Local: " + ", ".join(LOCAL) + "; groups: backend, all; opt-in: android")
            print("CI: " + ", ".join(CI_PACKAGES) + "; groups: " + ", ".join(GROUPS) + ", all")
            return
        if not packages:
            parser.print_help()
            return
        if args.ci and (args.debug or args.target or args.npm_ci or args.android_target or args.keystore):
            raise ValueError("--debug, --target, --npm-ci, --android-target and --keystore are local-build options")
        if not args.ci and (args.ref or args.repo or args.wait or args.download):
            raise ValueError("--ref, --repo, --wait and --download require --ci")
        selected = expand(packages, ci=args.ci)
        if not args.ci and "android" in selected and args.target:
            raise ValueError("Use --android-target for Android ABI selection, rather than --target")
        if args.ci:
            if args.download:
                args.download = str(Path(args.download).expanduser().resolve())
            ci_build(args, selected)
        else:
            local_build(args, selected)
    except (ValueError, OSError) as error:
        parser.exit(1, f"error: {error}\n")
    except subprocess.CalledProcessError as error:
        parser.exit(error.returncode, f"error: command failed (exit {error.returncode})\n")


if __name__ == "__main__":
    os.environ["GIT_OPTIONAL_LOCKS"] = "0"
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
