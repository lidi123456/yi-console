#!/usr/bin/env python3
"""Build planner and command orchestration checks, without compilers or network access.

Run with: python3 -m unittest discover -s scripts -p 'test_build.py'
"""

import contextlib
import importlib.util
import io
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("yi_build", Path(__file__).with_name("build.py"))
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)


class BuildTests(unittest.TestCase):
    def invoke(self, *arguments):
        with patch("sys.argv", ["build.py", *arguments]), contextlib.redirect_stdout(io.StringIO()):
            build.main()

    def test_all_keeps_release_matrix(self):
        plan = build.ci_plan(["all"])
        self.assertEqual(len(plan["daemons"]), 5)
        self.assertEqual(len(plan["desktop"]), 4)
        self.assertTrue(plan["android"])
        self.assertEqual(build.ci_plan([]), plan)

    def test_android_only_has_no_daemon_or_desktop_dependencies(self):
        self.assertEqual(build.ci_plan(["android"]), dict(daemons=[], desktop=[], android=True))

    def test_desktop_packages_keep_ssh_daemons(self):
        for package in ("windows", "linux", "macos-aarch64", "macos-x86_64"):
            with self.subTest(package=package):
                plan = build.ci_plan([package])
                self.assertEqual(len(plan["desktop"]), 1)
                self.assertEqual(len(plan["daemons"]), 5)
                self.assertFalse(plan["android"])

    def test_groups_and_combined_packages_are_deduplicated(self):
        plan = build.ci_plan(["macos", "macos-aarch64", "android"])
        self.assertEqual({row["daemon"] for row in plan["desktop"]},
                         {"macos-aarch64", "macos-x86_64"})
        self.assertTrue(plan["android"])
        self.assertEqual(build.expand(["backend", "yi-agentd"]),
                         ["yi-agentd", "yi-relay", "yi-web"])

    def test_legacy_options(self):
        plan = build.ci_plan([], windows_only=True)
        self.assertEqual([row["daemon"] for row in plan["desktop"]], ["windows-x86_64"])
        self.assertEqual(len(plan["daemons"]), 3)
        self.assertFalse(plan["android"])
        for windows_only in (True, False):
            plan = build.ci_plan([], only_daemons=True, windows_only=windows_only)
            self.assertEqual(plan["desktop"], [])
            self.assertFalse(plan["android"])
            self.assertEqual(len(plan["daemons"]), 3 if windows_only else 5)
        with self.assertRaises(ValueError):
            build.ci_plan(["android"], windows_only=True)

    def test_daemons_only(self):
        plan = build.ci_plan(["daemons"])
        self.assertEqual(len(plan["daemons"]), 5)
        self.assertEqual(plan["desktop"], [])
        self.assertFalse(plan["android"])

    def test_invalid_selection_does_not_start_any_command(self):
        for args in (("--ci", "windwos"), ("windows",), ("--ci", "all", "--debug"),
                     ("frontend", "--wait")):
            with self.subTest(args=args), patch.object(build, "run") as run:
                with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
                    self.invoke(*args)
                self.assertEqual(error.exception.code, 1)
                run.assert_not_called()

    def test_dry_run_never_executes_commands(self):
        with patch.object(build.subprocess, "run") as run:
            self.invoke("all", "--dry-run")
            self.invoke("--ci", "all", "--download", "out", "--dry-run")
            self.invoke("android", "--dry-run")
            run.assert_not_called()

    def test_android_is_opt_in_and_rejects_rust_target(self):
        self.assertNotIn("android", build.expand(["all"]))
        self.assertEqual(build.expand(["android"]), ["android"])
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            self.invoke("android", "--target", "aarch64-linux-android", "--dry-run")

    def test_android_refreshes_native_sources_and_collects_new_apk(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            native = root / "app/src-tauri"
            (native / "android").mkdir(parents=True)
            (native / "android/MainActivity.kt").write_text("current activity")
            (native / "icons/android/mipmap").mkdir(parents=True)
            (native / "icons/android/mipmap/icon.png").write_bytes(b"current icon")
            (native / "tauri.conf.json").write_text('{"version":"0.3.0"}')

            def run(command, **kwargs):
                if "build" in command:
                    apk = native / "gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk"
                    apk.parent.mkdir(parents=True)
                    apk.write_bytes(b"new apk")

            with patch.object(build, "ROOT", root), patch.object(build, "run", side_effect=run), \
                    patch.object(build.shutil, "which", side_effect=lambda cmd: cmd):
                self.invoke("android")
            self.assertEqual((native / "gen/android/app/src/main/java/dev/yiconsole/app/MainActivity.kt").read_text(),
                             "current activity")
            self.assertEqual((native / "gen/android/app/src/main/res/mipmap/icon.png").read_bytes(), b"current icon")
            self.assertEqual(len(list((root / "out").glob("*-unsigned.apk"))), 1)

    def test_android_does_not_collect_stale_apk(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            native = root / "app/src-tauri"
            (native / "android").mkdir(parents=True)
            (native / "android/MainActivity.kt").write_text("activity")
            (native / "icons/android").mkdir(parents=True)
            apk = native / "gen/android/app/build/outputs/apk/universal/release/app-release-unsigned.apk"
            apk.parent.mkdir(parents=True)
            apk.write_bytes(b"stale")
            os.utime(apk, (0, 0))
            with patch.object(build, "ROOT", root), patch.object(build, "run"), \
                    patch.object(build.shutil, "which", side_effect=lambda cmd: cmd), \
                    contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                self.invoke("android")
            self.assertFalse((root / "out").exists())

    def test_local_build_failure_stops_and_propagates(self):
        failure = subprocess.CalledProcessError(23, ["cargo", "build"])
        with patch.object(build.shutil, "which", side_effect=lambda cmd: cmd), \
                patch.object(build, "run", side_effect=failure) as run, \
                contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
            self.invoke("yi-relay", "frontend")
        self.assertEqual(error.exception.code, 23)
        self.assertEqual(run.call_count, 1)

    def test_windows_cross_target_prepares_driver(self):
        with patch.object(build.shutil, "which", side_effect=lambda cmd: cmd), \
                patch.object(build, "run") as run:
            self.invoke("yi-agentd", "--target", "x86_64-pc-windows-msvc")
        self.assertEqual(run.call_args_list[0].args[0][1], "scripts/prepare-computer-use.py")
        self.assertIn("x86_64-pc-windows-msvc", run.call_args_list[1].args[0])

    def test_ci_plan_writes_actions_outputs(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output), "ONLY_DAEMONS": "false",
                                         "WINDOWS_ONLY": "false"}):
                self.invoke("--plan-ci", "windows,android")
            rows = dict(line.split("=", 1) for line in output.read_text().splitlines())
            self.assertEqual(len(json.loads(rows["daemons"])), 5)
            self.assertEqual(len(json.loads(rows["desktop"])), 1)
            self.assertEqual(rows["android"], "true")

    def test_dispatch_tracks_its_own_run_and_downloads_to_invocation_directory(self):
        commands = []

        def run(command, **kwargs):
            commands.append(command)
            if "list" in command:
                return json.dumps([
                    dict(displayTitle="build (other)", databaseId=1, url="other"),
                    dict(displayTitle="build (test-id)", databaseId=42, url="ours"),
                ])
            return ""

        with patch.object(build, "run", side_effect=run), \
                patch.object(build.uuid, "uuid4") as uid:
            uid.return_value.hex = "test-id"
            self.invoke("--ci", "windows,android", "--ref", "main", "--repo", "owner/repo",
                        "--download", "out")
        self.assertIn("packages=windows-x86_64,android", commands[0])
        self.assertIn("build_id=test-id", commands[0])
        self.assertEqual(commands[-2][-4:], ["run", "watch", "42", "--exit-status"])
        self.assertEqual(commands[-1][-3:], ["42", "--dir", str(Path("out").resolve())])

    def test_failed_ci_does_not_download(self):
        commands = []

        def run(command, **kwargs):
            commands.append(command)
            if "list" in command:
                return json.dumps([dict(displayTitle="build (test-id)", databaseId=42, url="ours")])
            if "watch" in command:
                raise subprocess.CalledProcessError(1, command)
            return ""

        with patch.object(build, "run", side_effect=run), \
                patch.object(build.uuid, "uuid4") as uid, contextlib.redirect_stderr(io.StringIO()):
            uid.return_value.hex = "test-id"
            with self.assertRaises(SystemExit) as error:
                self.invoke("--ci", "all", "--ref", "main", "--download", "out")
        self.assertEqual(error.exception.code, 1)
        self.assertFalse(any("download" in command for command in commands))


if __name__ == "__main__":
    unittest.main()
