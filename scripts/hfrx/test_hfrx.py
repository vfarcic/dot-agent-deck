"""Stand-in-only H-FRX self-checks. No agent, socket server or network namespace."""

import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("hfrx", Path(__file__).with_name("hfrx.py"))
h = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(h)


class HfrxSafety(unittest.TestCase):
    def setUp(self):
        # Disk-backed fixture, with no dependency on the developer's HOME/config.
        self.tmp = tempfile.TemporaryDirectory(prefix="hfrx-selfcheck-", dir="/var/tmp")
        self.base = Path(self.tmp.name)
        self.root = self.base / "run"
        h.prepare(self.root, {})
        self.env = h.fixture_env(self.root)

    def tearDown(self):
        self.tmp.cleanup()

    def test_production_socket_refused_without_opening_it(self):
        production = self.base / "pretend-production.sock"
        for name in ("DOT_AGENT_DECK_SOCKET", "DOT_AGENT_DECK_ATTACH_SOCKET"):
            with self.subTest(name=name):
                env = {**self.env, name: str(production)}
                with self.assertRaisesRegex(h.Refusal, "production or non-run deck socket"):
                    h.preflight(self.root, env, {production})
        self.assertFalse(production.exists())

    def test_auth_json_refused_even_nested_or_dangling(self):
        auth = self.root / "codex-home/auth.json"
        auth.write_text("stand-in, not a credential")
        with self.assertRaisesRegex(h.Refusal, "auth.json present"):
            h.preflight(self.root, self.env, set())
        auth.unlink()
        auth.symlink_to(self.root / "absent")
        with self.assertRaises(h.Refusal):
            h.preflight(self.root, self.env, set())

    def test_foreign_config_in_both_ancestries_refused(self):
        for ancestor in (self.base, self.root, self.root / "home"):
            with self.subTest(ancestor=ancestor):
                config = ancestor / ".dot-agent-deck.toml"
                config.write_text("stand-in foreign config")
                try:
                    with self.assertRaisesRegex(h.Refusal, "foreign .dot-agent-deck.toml"):
                        h.preflight(self.root, self.env, set())
                finally:
                    config.unlink()

    def test_valid_fixture_allowed_and_environment_closed(self):
        h.preflight(self.root, self.env, set())
        for key in ("OPENAI_API_KEY", "ANTHROPIC_API_KEY", "SSH_AUTH_SOCK", "DISPLAY",
                    "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS"):
            with self.subTest(key=key), self.assertRaisesRegex(h.Refusal, "allowlist"):
                h.preflight(self.root, {**self.env, key: "stand-in"}, set())

    def test_symlink_escape_and_changed_fixture_refused(self):
        link = self.root / "escape"
        link.symlink_to(self.base)
        with self.assertRaisesRegex(h.Refusal, "symlink"):
            h.preflight(self.root, self.env, set())
        link.unlink()
        (self.root / "work/.dot-agent-deck.toml").write_text("changed")
        with self.assertRaisesRegex(h.Refusal, "fixture configuration changed"):
            h.preflight(self.root, self.env, set())

    def test_owner_only_root_and_fresh_run_id(self):
        self.root.chmod(0o755)
        with self.assertRaisesRegex(h.Refusal, "0700"):
            h.preflight(self.root, self.env, set())
        with self.assertRaisesRegex(h.Refusal, "already exists"):
            h.prepare(self.root, {})

    def test_real_config_location_is_refused_before_creating_it(self):
        # This HOME is itself a fixture; the real HOME is never traversed.
        pretend_home = self.base / "pretend-home"
        with patch.dict(os.environ, {"HOME": str(pretend_home)}):
            with self.assertRaisesRegex(h.Refusal, "real agent configuration"):
                h.prepare(pretend_home / ".codex/run", {})
        self.assertFalse(pretend_home.exists())

    def test_tmpfs_root_refused_using_mount_metadata_standin(self):
        mounts = "1 0 0:1 / / rw - tmpfs tmpfs rw\n"
        with patch.object(Path, "read_text", return_value=mounts):
            with self.assertRaisesRegex(h.Refusal, "recognized disk-backed"):
                h.disk_backed(self.root)

    def test_dry_run_cli_stops_before_any_subprocess_or_server(self):
        pins = self.base / "pins.json"
        pins.write_text("{}")
        root = self.base / "dry"
        argv = ["hfrx", "dry-run", "--root", str(root), "--pins", str(pins)]
        with patch.object(sys, "argv", argv), patch.object(h.subprocess, "Popen") as popen, \
                patch.object(h.http.server, "ThreadingHTTPServer") as server, \
                patch.object(h, "Children") as children, contextlib.redirect_stdout(io.StringIO()):
            h.main()
        popen.assert_not_called()
        server.assert_not_called()
        children.assert_not_called()
        evidence = json.loads((root / "dry-run.json").read_text())
        self.assertEqual(evidence, {"status": "preflight passed", "agents_started": 0,
                                   "subprocesses_started": 0, "network_probed": False, "measurements": 0})

    def test_pin_hash_checked_without_executing_binary(self):
        standin = self.base / "never-execute"
        standin.write_text("this is not an executable program\n")
        standin.chmod(0o700)
        pin = {"python3": {"path": str(standin), "sha256": hashlib.sha256(standin.read_bytes()).hexdigest()}}
        root = self.base / "pinned"
        with patch.object(h.subprocess, "Popen") as spawn:
            h.prepare(root, pin)
            h.pins_recheck(root)
        spawn.assert_not_called()
        standin.write_text("changed")
        with self.assertRaisesRegex(h.Refusal, "binary changed"):
            h.pins_recheck(root)

    def test_export_refuses_key_and_copies_nothing(self):
        export = self.base / "export"
        raw = self.root / "raw/trace"
        raw.write_text("sk-standinsecret123456789")
        with self.assertRaisesRegex(h.Refusal, "key-shaped"):
            h.export_scrubbed(self.root, export)
        self.assertFalse(export.exists())
        raw.write_text(h.FAKE_KEY)
        self.assertGreater(h.export_scrubbed(self.root, export), 0)
        self.assertEqual((export / "raw/trace").read_text(), h.FAKE_KEY)

    def test_fixture_key_exemption_does_not_allow_a_longer_secret(self):
        (self.root / "raw/trace").write_text(h.FAKE_KEY + "-secret-tail")
        with self.assertRaisesRegex(h.Refusal, "key-shaped"):
            h.export_scrubbed(self.root, self.base / "export")

    def test_export_checks_strace_hex_encoding(self):
        standin_secret = b"sk-standinsecret123456789"
        encoded = "".join(f"\\x{byte:02x}" for byte in standin_secret)
        (self.root / "raw/trace").write_text(f'1.0 write(1, "{encoded}", 24) = 24\n')
        with self.assertRaisesRegex(h.Refusal, "key-shaped"):
            h.export_scrubbed(self.root, self.base / "export")

    def test_connect_detection_accepts_only_exact_mock_or_run_unix(self):
        trace = self.root / "raw/connect.1"
        trace.write_text('1.0 connect(3, {sa_family=AF_INET, sin_port=htons(8123), '
                         'sin_addr=inet_addr("127.0.0.1")}, 16) = 0\n'
                         f'1.1 connect(4, {{sa_family=AF_UNIX, sun_path="{self.root}/mock.sock"}}, 30) = 0\n')
        h.check_connects([trace], self.root, 8123)
        for endpoint in ('sin_addr=inet_addr("10.0.0.1")',
                         'sun_path="/var/run/standin.sock"',
                         'sun_path=@"abstract-standin"', 'sa_family=AF_INET6'):
            with self.subTest(endpoint=endpoint):
                trace.write_text(f"1.2 connect(3, {{{endpoint}}}, 16) = -1 EPERM\n")
                with self.assertRaisesRegex(h.Refusal, "UNVERIFIED containment"):
                    h.check_connects([trace], self.root, 8123)

    def test_container_never_pulls_or_accepts_an_existing_name(self):
        owner = object.__new__(h.Children)
        owner.root = self.root
        owner.containers = []
        proc = unittest.mock.Mock()
        proc.wait.return_value = 0
        with patch.object(owner, "spawn", return_value=proc) as spawn:
            owner.container("local-fixture:pin", ["nvim"])
        argv = spawn.call_args_list[-1].args[0]
        self.assertIn("--pull=never", argv)
        self.assertIn("--network=none", argv)
        self.assertTrue(owner.containers[0].startswith("hfrx-"))
        proc.wait.return_value = 1
        with patch.object(owner, "spawn", return_value=proc) as spawn:
            with self.assertRaisesRegex(h.Refusal, "image unavailable locally"):
                owner.container("missing:pin", [])
            self.assertEqual(spawn.call_count, 1)

    def test_pidfd_cleanup_reaps_a_detached_standin_descendant(self):
        owner = h.Children(self.root, self.env)
        proc = None
        child_fd = None
        try:
            # This Python stand-in is the only process tree this test starts.
            # The detached child survives its parent, so cleanup must adopt it.
            code = ("import os, time\n"
                    "pid = os.fork()\n"
                    "if pid == 0:\n"
                    " os.setsid()\n"
                    " print(os.getpid(), flush=True)\n"
                    " time.sleep(30)\n"
                    "else:\n"
                    " time.sleep(30)\n")
            proc = owner.spawn([sys.executable, "-c", code], stdout=h.subprocess.PIPE,
                               stderr=h.subprocess.DEVNULL)
            with h.selectors.DefaultSelector() as selector:
                selector.register(proc.stdout, h.selectors.EVENT_READ)
                self.assertTrue(selector.select(3), "stand-in did not acknowledge startup")
            child_pid = int(proc.stdout.readline())
            child_fd = os.pidfd_open(child_pid)
        finally:
            survivors = owner.cleanup()
            if proc is not None:
                proc.stdout.close()
        try:
            self.assertEqual(survivors, [])
            self.assertTrue(h.selectors_ready(child_fd), "detached stand-in survived cleanup")
        finally:
            if child_fd is not None:
                os.close(child_fd)


class HfrxEvidence(unittest.TestCase):
    def test_x1_missing_frontier_is_not_a_no_writer_sample(self):
        missing = {"available": True, "frontier": "missing", "report": "none"}
        good = {"available": True, "frontier": "taken", "report": "Clean",
                "window_ns": 10_000_000_000, "standin_silent": True, "post_frontier_bytes": b"",
                "orphan_sighup": False, "orphan_sigcont": False, "frontier_latency_ns": 123}
        result = h.x1_summary([missing, {"available": False}, good])
        self.assertEqual(result["runs"], 3)
        self.assertEqual(result["missing_frontier"], 1)
        self.assertEqual(result["unknown_report"], 1)
        self.assertEqual(result["unavailable"], 1)
        self.assertEqual(result["qualified_windows"], 1)
        self.assertIn("none in 1 for this setup", result["zero_in_n"])
        self.assertEqual(result["frontier_latency_ns"], [123])
        with self.assertRaisesRegex(h.Refusal, "full 10-second"):
            h.x1_summary([{**good, "window_ns": 10}])

    def test_master_offsets_survive_chunk_splits_and_have_no_bound_label(self):
        raw, clocks = io.BytesIO(), io.StringIO()
        capture = h.Capture(raw, clocks)
        for chunk, mono in [(b"p\x1b[?20", 100), (b"04h\x1b[?202", 200), (b"6h", 200)]:
            capture.feed(chunk, mono, mono + 1000)
        result = h.arrival_result(capture.tokens, 0)
        self.assertEqual(result["enable_offset"], 1)
        self.assertEqual(result["frame_offset"], 9)
        self.assertEqual(result["arrival_interval_ns"], 0)
        self.assertIn("no elapsed-time bound", result["label"])
        self.assertEqual(raw.getvalue(), b"p" + h.ENABLE + h.FRAME)

    def test_missing_markers_are_unverified_not_zero(self):
        self.assertEqual(h.arrival_result([], 0)["status"], "UNVERIFIED")
        self.assertEqual(h.cross_clock_order(1, 1_000_001), "unknown-within-1ms")
        self.assertEqual(h.cross_clock_order(1, 1_000_002), "before")
        self.assertEqual(h.cross_clock_order(1_000_002, 1), "after")

    def test_instrumented_pairs_exclude_startup_and_keep_clock_source(self):
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            trace = Path(directory) / "trace.123"
            trace.write_text('10.0 write(1, "\\x1b\\x5b\\x3f\\x32\\x30\\x30\\x34\\x68", 8) = 8\n'
                             '10.1 ioctl(0, TCFLSH, TCIFLUSH) = 0\n'
                             '11.0 write(1, "\\x1b\\x5b\\x3f\\x32\\x30\\x30\\x34\\x6c", 8) = 8\n'
                             '12.0 writev(1, [{iov_base="\\x1b\\x5b\\x3f\\x32", iov_len=4}, '
                             '{iov_base="\\x30\\x30\\x34\\x68", iov_len=4}], 2) = 8\n'
                             '12.001 ioctl(0, TCFLSH, TCIFLUSH) = 0\n')
            pairs = h.syscall_pairs([trace])
        self.assertEqual(len(pairs), 1)
        self.assertEqual(pairs[0]["pid"], "123")
        self.assertEqual(pairs[0]["interval_ns"], 1_000_000)

    def test_mfirst_keeps_censoring_and_pending_armed_is_not_a_stop(self):
        def generation(duration, stop, cause):
            return {"platform": "Linux", "generation": str(duration), "duration_ns": duration,
                    "first_stop_ns": stop, "cause": cause, "attribution": "Owned",
                    "held_input_discarded_at_restart": False, "pending_count": 2,
                    "pending_median_ns": 1, "pending_max_ns": 2, "pending_median_method": "fixture-exact",
                    "pending_resolutions": {"Armed": 2, "Unarmed": 0, "Tool": 0, "ExitNoDecision": 0}}
        summary = h.first_summary([generation(5, None, None), generation(10, 7, "EditorUnconfirmed")])
        self.assertEqual(summary["generations"], 2)
        self.assertEqual(summary["median_ns"], 7)
        self.assertEqual(summary["causes"]["censored"], 1)
        self.assertEqual(summary["pending_resolutions"]["Armed"], 4)
        self.assertEqual(summary["km"][0]["censored"], 1)
        with self.assertRaisesRegex(h.Refusal, "missing required"):
            h.first_summary([{}])

    def test_plan_labels_blocked_rows_and_every_mfirst_field(self):
        plan = h.plan()
        for name in ("X1", "X3-gate", "X3-natural", "X4", "M-FIRST"):
            self.assertEqual(plan["rows"][name]["status"], "needs-implementation")
        self.assertEqual(plan["rows"]["X2-arrival"]["status"], "runnable-now")
        self.assertEqual(plan["rows"]["X2-instrumented"]["status"], "runnable-now")
        self.assertEqual(set(plan["M-FIRST"]), set(h.FIRST_FIELDS))


if __name__ == "__main__":
    unittest.main()
