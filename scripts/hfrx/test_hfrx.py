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
        # Self-check files may live on a CI overlay. Production preflight still
        # requires disk-backed storage; exercise its metadata rule separately.
        self.disk_check = h.disk_backed
        self.disk_standin = patch.object(h, "disk_backed")
        self.disk_standin.start()
        self.addCleanup(self.disk_standin.stop)
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
        """Scenario: Refuse fixture exports into agent config directories through dot components or symlinks before creating an export."""
        # This HOME is itself a fixture; the real HOME is never traversed.
        pretend_home = self.base / "pretend-home"
        with patch.dict(os.environ, {"HOME": str(pretend_home)}):
            with self.assertRaisesRegex(h.Refusal, "real agent configuration"):
                h.prepare(pretend_home / ".codex/run", {})
        self.assertFalse(pretend_home.exists())
        pretend_home.mkdir()
        for name in (".codex", ".claude"):
            config = pretend_home / name
            config.mkdir()
            detour = self.base / f"detour-{name}"
            detour.mkdir()
            link = self.base / f"link-{name}"
            link.symlink_to(config, target_is_directory=True)
            for index, parent in enumerate((config, detour / ".." / "pretend-home" / name,
                                            link, link / "..")):
                destination = parent / f"export-{index}"
                with self.subTest(destination=destination), patch.dict(os.environ, {"HOME": str(pretend_home)}):
                    with self.assertRaises(h.Refusal):
                        h.export_scrubbed(self.root, destination)
                    self.assertFalse(destination.resolve().exists())
            dangling = self.base / f"dangling-{name}"
            dangling.symlink_to(self.base / f"absent-{name}", target_is_directory=True)
            with self.assertRaisesRegex(h.Refusal, "symlink"):
                h.export_scrubbed(self.root, dangling / "export")
        safe_detour = self.base / "safe-detour"
        safe_detour.mkdir()
        self.assertGreater(h.export_scrubbed(self.root, safe_detour / ".." / "safe-export"), 0)
        self.assertTrue((self.base / "safe-export/environment.json").is_file())
        with self.assertRaisesRegex(h.Refusal, "outside the run"):
            h.export_scrubbed(self.root, self.base / "safe-detour/../run/export")
        self.assertFalse((self.root / "export").exists())

    def test_tmpfs_root_refused_using_mount_metadata_standin(self):
        mounts = "1 0 0:1 / / rw - tmpfs tmpfs rw\n"
        with patch.object(Path, "read_text", return_value=mounts):
            with self.assertRaisesRegex(h.Refusal, "recognized disk-backed"):
                self.disk_check(self.root)
        with patch.object(Path, "read_text", return_value="1 0 0:1 / / rw - ext4 /dev/standin rw\n"):
            self.disk_check(self.root)

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

    def test_export_reconstructs_fragmented_trace_secrets_before_creating_destination(self):
        def hexed(data):
            return "".join(f"\\x{byte:02x}" for byte in data)

        case = 0
        for prefix, suffix in ((hexed(b"sk-"), hexed(b"standinsecret123456789")),
                               (hexed(b"sk-"), "standinsecret123456789"),
                               (h.FAKE_KEY, hexed(b"-secret-tail"))):
            for shape in (f'1.0 writev(9, [{{iov_base="{prefix}", iov_len=3}}, '
                          f'{{iov_base="{suffix}", iov_len=21}}], 2) = 24\n',
                          f'1.0 write(9, "{prefix}", 3) = 3\n'
                          f'1.1 write(9, "{suffix}", 21) = 21\n',
                          f'1.0 write(9, "{prefix}", 3) = 3\n'
                          '1.05 write(10, "\\n", 1) = 1\n'
                          f'1.1 write(9, "{suffix}", 21) = 21\n'):
                with self.subTest(shape=shape):
                    (self.root / "raw/trace").write_text(shape)
                    destination = self.base / f"export-{case}"
                    case += 1
                    with self.assertRaisesRegex(h.Refusal, "key-shaped"):
                        h.export_scrubbed(self.root, destination)
                    self.assertFalse(destination.exists())

    def test_export_enforces_aggregate_budget_before_creating_destination(self):
        (self.root / "raw/trace").write_text('1.0 write(9, "safe", 4) = 4\n')
        for name, bound, message in (("EXPORT_TOTAL_BYTES", 8, "aggregate"),
                                      ("EXPORT_FILE_BYTES", 8, "file too large"),
                                      ("EXPORT_FILE_COUNT", 0, "file-count"),
                                      ("EXPORT_LITERAL_COUNT", 0, "literal budget"),
                                      ("EXPORT_STREAM_COUNT", 0, "stream budget")):
            with self.subTest(name=name), patch.object(h, name, bound):
                with self.assertRaisesRegex(h.Refusal, message):
                    h.export_scrubbed(self.root, self.base / "export")
                self.assertFalse((self.base / "export").exists())

    def test_export_accepts_exact_fixture_key_split_across_writes(self):
        prefix, suffix = h.FAKE_KEY[:3], h.FAKE_KEY[3:]
        (self.root / "raw/trace").write_text(f'1.0 write(9, "{prefix}", 3) = 3\n'
                                            f'1.1 write(9, "{suffix}", {len(suffix)}) = {len(suffix)}\n')
        self.assertGreater(h.export_scrubbed(self.root, self.base / "export"), 0)

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

    def test_container_operations_are_unavailable_until_ownership_is_implemented(self):
        self.assertFalse(hasattr(h.Children, "container"))
        self.assertIn("captured container ID", h.plan()["container_rows"])

    def test_cleanup_ignores_repeated_signals_and_retains_errors(self):
        """Scenario: Repeated stop signals do not interrupt cleanup, and failed adoption or an unsettled child set records survivors as unknown."""
        owner = h.Children(self.root, self.env)
        original_adopted = owner.adopted

        def interrupted_adoption():
            os.kill(os.getpid(), h.signal.SIGTERM)
            os.kill(os.getpid(), h.signal.SIGINT)
            original_adopted()

        with patch.object(owner, "adopted", side_effect=interrupted_adoption):
            self.assertEqual(owner.cleanup(), [])
        artifact, = self.root.glob(f"cleanup-children-{os.getpid()}-*.json")
        report = json.loads(artifact.read_text())
        self.assertEqual(report["survivors"], [])
        self.assertEqual(report["errors"], [])
        owner = h.Children(self.root, self.env)
        with patch.object(owner, "adopted", side_effect=OSError("stand-in adoption failure")):
            self.assertIsNone(owner.cleanup(), "failed adoption must report survivors unknown")
        reports = [json.loads(path.read_text()) for path in self.root.glob(f"cleanup-children-{os.getpid()}-*.json")]
        self.assertEqual(len(reports), 2)
        self.assertTrue(any(report["errors"] for report in reports))
        unknown, = [report for report in reports if report["errors"]]
        self.assertIsNone(unknown["survivors"])
        self.assertEqual(unknown["survivors_status"], "unknown")
        owner = h.Children(self.root, self.env)
        ticks = iter(range(0, 100, 4))
        with patch.object(owner, "adopted"), patch.object(os, "waitpid", return_value=(0, 0)), \
                patch.object(h.time, "monotonic", side_effect=lambda: next(ticks)):
            self.assertIsNone(owner.cleanup(), "empty captured set cannot prove all descendants exited")
        reports = [json.loads(path.read_text()) for path in self.root.glob(f"cleanup-children-{os.getpid()}-*.json")]
        self.assertEqual(len(reports), 3)
        self.assertEqual(sum(report["survivors_status"] == "unknown" for report in reports), 2)

    def test_spawn_failure_does_not_leak_pty_descriptors(self):
        owner = h.Children(self.root, self.env)
        try:
            for failure in (OSError, h.StopRequested):
                master, slave = h.pty.openpty()
                try:
                    with self.subTest(failure=failure), \
                            patch.object(h.pty, "openpty", return_value=(master, slave)), \
                            patch.object(owner, "spawn", side_effect=failure("stand-in spawn failure")) as spawn:
                        with self.assertRaisesRegex(failure, "stand-in spawn failure"):
                            h.x2_sample(self.root, owner, "0.160.0", 0, "arrival")
                        self.assertNotIn("preexec_fn", spawn.call_args.kwargs)
                        self.assertEqual(spawn.call_args.args[0][2], "_pty")
                    for fd in (master, slave):
                        with self.assertRaises(OSError):
                            os.fstat(fd)
                finally:
                    for fd in (master, slave):
                        try:
                            os.close(fd)
                        except OSError:
                            pass
        finally:
            owner.cleanup()

    def test_supervisor_records_source_digest_and_bounds_inner_wait(self):
        """Scenario: Record source provenance and bound the supervisor wait, then select a ten-sample single-release pilot and refuse an unavailable required namespace before measurement startup."""
        args = unittest.mock.Mock(root=str(self.root), method="arrival", samples=1,
                                  reviewed_commit="a" * 40, release=None, require_namespace=False)
        with patch.object(h, "Children") as children:
            owner = children.return_value
            owner.spawn.return_value.wait.return_value = 0
            owner.cleanup.return_value = []
            h.run_x2(args)
        authorization = json.loads((self.root / "run-authorization.json").read_text())
        self.assertEqual(authorization["executing_script_sha256"],
                         hashlib.sha256(Path(h.__file__).read_bytes()).hexdigest())
        owner.spawn.return_value.wait.assert_called_once_with(timeout=82)
        for probe_ok in (True, False):
            for tools_present in (True, False):
                root = self.base / f"pilot-{probe_ok}-{tools_present}"
                h.prepare(root, {})
                args = h.argparse.Namespace(root=str(root), method="arrival", samples=10,
                                            reviewed_commit="a" * 40, release="0.164.0", require_namespace=True)
                pins = dict.fromkeys(("unshare", "ip", "python3", "true")) if tools_present else {}
                with self.subTest(probe_ok=probe_ok, tools_present=tools_present), \
                        patch.object(h, "pins_recheck", return_value=pins), patch.object(h, "Children") as children:
                    owner = children.return_value
                    owner.cleanup.return_value = []
                    owner.spawn.return_value.wait.return_value = 0 if probe_ok else 1
                    if not (probe_ok and tools_present):
                        with self.assertRaisesRegex(h.Refusal, "required network namespace"):
                            h.run_x2(args)
                        self.assertFalse((root / "run-authorization.json").exists())
                        self.assertFalse((root / "codex-home/config.toml").exists())
                        self.assertEqual(owner.spawn.call_count, int(tools_present))
                    else:
                        h.run_x2(args)
                        authorization = json.loads((root / "run-authorization.json").read_text())
                        self.assertEqual(authorization["releases"], ["0.164.0"])
                        self.assertEqual(authorization["samples_per_release"], 10)
                        self.assertTrue(authorization["require_namespace"])
                        self.assertEqual(authorization["network"], "network-namespace")
                        self.assertEqual(owner.spawn.call_args.args[0][-1], "0.164.0")
                        self.assertEqual(owner.spawn.return_value.wait.call_args.kwargs["timeout"], 370)
        # The inner subprocess must also respect the release selector. Its
        # provider, thread and sample launches are stand-ins throughout.
        with patch.object(h, "Children") as children, patch.object(h, "pins_recheck", return_value={
                "python3": {}, "strace": {}, "codex-0.164.0": {}}), \
                patch.object(h.http.server, "ThreadingHTTPServer") as server, \
                patch.object(h.threading, "Thread"), patch.object(h, "check_connects"), \
                patch.object(h, "x2_sample", side_effect=lambda root, owner, version, sample, method: {
                    "status": "observed", "release": version, "arrival_interval_ns": sample}):
            server.return_value.server_address = ("127.0.0.1", 8123)
            children.return_value.cleanup.return_value = []
            h.run_x2_inner(self.root, "arrival", 10, "detection-only", ("0.164.0",))
        summary = json.loads((self.root / "X2-arrival.json").read_text())
        self.assertEqual(len(summary["results"]), 10)
        self.assertEqual(set(summary["distribution"]), {"0.164.0"})

    def test_post_exec_pty_setup_never_uses_a_python_preexec_callback(self):
        with patch.object(sys, "argv", ["hfrx", "_pty", "/standin-strace", "arg"]), \
                patch.object(os, "setsid") as setsid, patch("fcntl.ioctl") as ioctl, \
                patch.object(os, "execv", side_effect=OSError("stand-in exec failure")) as execute:
            with self.assertRaisesRegex(OSError, "stand-in exec failure"):
                h.main()
        setsid.assert_called_once_with()
        ioctl.assert_called_once_with(0, h.termios.TIOCSCTTY, 0)
        execute.assert_called_once_with("/standin-strace", ["/standin-strace", "arg"])

    def test_post_exec_launcher_gives_the_standin_a_controlling_tty(self):
        owner = h.Children(self.root, self.env)
        master, slave = h.pty.openpty()
        try:
            code = "import os; print('TTY-OWNED' if os.tcgetpgrp(0) == os.getpgrp() else 'NO-TTY', flush=True)"
            owner.spawn([sys.executable, str(Path(h.__file__)), "_pty", sys.executable, "-c", code],
                        stdin=slave, stdout=slave, stderr=slave)
            with h.selectors.DefaultSelector() as selector:
                selector.register(master, h.selectors.EVENT_READ)
                self.assertTrue(selector.select(3), "stand-in did not acknowledge its controlling tty")
            self.assertIn(b"TTY-OWNED", os.read(master, 4096))
        finally:
            os.close(slave)
            os.close(master)
            owner.cleanup()

    def test_pidfd_cleanup_reaps_a_detached_standin_descendant(self):
        """Scenario: Clean up detached Python descendants when the parent exits on TERM and when parent and child resist TERM until the final KILL phase."""
        for resist_term in (False, True):
            with self.subTest(resist_term=resist_term):
                self.check_detached_standin_cleanup(resist_term)

    def check_detached_standin_cleanup(self, resist_term):
        owner = h.Children(self.root, self.env)
        proc = None
        child_fd = None
        try:
            # This Python stand-in is the only process tree this test starts.
            # The detached child survives its parent, so cleanup must adopt it.
            code = ("import os, signal, time\n"
                    + ("signal.signal(signal.SIGTERM, signal.SIG_IGN)\n" if resist_term else "") +
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
            send_signal = h.signal.pidfd_send_signal

            def signal_then_wait_for_parent(fd, sig):
                send_signal(fd, sig)
                if resist_term and proc is not None and sig == h.signal.SIGKILL and fd == owner.handles.get(proc.pid):
                    # Deterministically put adoption after the final pre-KILL
                    # scan and before cleanup observes the dead parent's pidfd.
                    proc.wait(timeout=3)

            with patch.object(h.signal, "pidfd_send_signal", side_effect=signal_then_wait_for_parent):
                survivors = owner.cleanup()
            if proc is not None:
                proc.stdout.close()
        try:
            self.assertEqual(survivors, [])
            self.assertTrue(h.selectors_ready(child_fd), "detached stand-in survived cleanup")
        finally:
            if child_fd is not None:
                # Failure cleanup signals this captured fixture identity only.
                if not h.selectors_ready(child_fd):
                    h.signal.pidfd_send_signal(child_fd, h.signal.SIGKILL)
                    os.waitpid(child_pid, 0)
                os.close(child_fd)


class HfrxEvidence(unittest.TestCase):
    def test_x1_missing_frontier_is_not_a_no_writer_sample(self):
        """Scenario: Count full post-frontier byte windows separately by population, refusing malformed duration and silence/orphan fields rather than qualifying them."""
        population = {name: "stand-in" for name in h.POPULATION_FIELDS}
        missing = {"population": population, "available": True, "frontier": "missing", "report": "none"}
        good = {"population": population, "available": True, "frontier": "taken", "report": "Clean",
                "window_ns": 10_000_000_000, "standin_silent": True, "post_frontier_bytes": b"",
                "orphan_sighup": False, "orphan_sigcont": False, "frontier_latency_ns": 123}
        result = h.x1_summary([missing, {"population": population, "available": False}, good])
        self.assertEqual(result["runs"], 3)
        self.assertEqual(result["missing_frontier"], 1)
        self.assertEqual(result["unknown_report"], 1)
        self.assertEqual(result["unavailable"], 1)
        self.assertEqual(result["qualified_windows"], 1)
        self.assertIn("none in 1 for this setup", result["zero_in_n"])
        self.assertEqual(result["frontier_latency_ns"], [123])
        with self.assertRaisesRegex(h.Refusal, "full 10-second"):
            h.x1_summary([{**good, "window_ns": 10}])
        unknown = h.x1_summary([{**good, "report": "none"}])
        self.assertEqual(unknown["qualified_windows"], 1)
        self.assertEqual(unknown["unknown_report"], 1)
        self.assertIn("including unknown", unknown["qualified_population"])
        with self.assertRaisesRegex(h.Refusal, "populations separately"):
            h.x1_summary([good, {**good, "population": {**population, "release": "other"}}])
        for value in (float("nan"), float("inf"), 10_000_000_000.5, True, None, "10000000000", -1, 2**63):
            with self.subTest(window_ns=value), self.assertRaises(h.Refusal):
                h.x1_summary([{**good, "window_ns": value}])
        for field in ("standin_silent", "orphan_sighup", "orphan_sigcont"):
            for value in (0, 1, "false", [], None):
                with self.subTest(field=field, value=value), self.assertRaises(h.Refusal):
                    h.x1_summary([{**good, field: value}])
        with self.assertRaisesRegex(h.Refusal, "full 10-second"):
            h.x1_summary([{**good, "standin_silent": False}])
        orphan = h.x1_summary([{**good, "orphan_sighup": True, "orphan_sigcont": True}])
        self.assertEqual((orphan["orphan_sighup"], orphan["orphan_sigcont"]), (1, 1))

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
            # Until trace parsing is ordered, complete and cycle-bound, even a
            # plausible trace must not be offered as measurement evidence.
            with self.assertRaisesRegex(h.Refusal, "cycle-bound"):
                h.syscall_pairs([trace])

    def test_instrumented_method_refused_before_starting_any_process(self):
        with patch.object(h, "Children") as children, patch.object(h, "preflight"), \
                patch.object(h, "pins_recheck", return_value={}), patch.object(h, "write_json"):
            children.return_value.spawn.return_value.wait.return_value = 0
            with self.assertRaisesRegex(h.Refusal, "cycle-bound"):
                h.run_x2(unittest.mock.Mock(method="instrumented", root="/not-opened",
                                           reviewed_commit="a" * 40))
        children.assert_not_called()

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
        self.assertEqual(summary["attribution"], {"Owned": 2})
        self.assertEqual(summary["held_input_discarded_at_restart"], {"false": 2})
        self.assertEqual(summary["pending_aggregates"][0]["pending_median_method"], "fixture-exact")
        with self.assertRaisesRegex(h.Refusal, "missing required"):
            h.first_summary([{}])
        row = generation(10, 7, "EditorUnconfirmed")
        for update in ({"pending_count": True}, {"pending_count": 2.0},
                       {"duration_ns": float("nan")}, {"duration_ns": True},
                       {"pending_median_ns": float("inf")}, {"attribution": "typo"},
                       {"cause": []}, {"attribution": []}, {"pending_median_method": []},
                       {"held_input_discarded_at_restart": 0}, {"pending_median_method": ""},
                       {"pending_resolutions": {"Armed": 2.0, "Unarmed": 0, "Tool": 0, "ExitNoDecision": 0}}):
            with self.subTest(update=update), self.assertRaises(h.Refusal):
                h.first_summary([{**row, **update}])
        with self.assertRaisesRegex(h.Refusal, "duplicate"):
            h.first_summary([row, row])
        zero = {**row, "pending_count": 0, "pending_median_ns": None,
                "pending_max_ns": None, "pending_median_method": "none",
                "pending_resolutions": dict.fromkeys(row["pending_resolutions"], 0)}
        self.assertEqual(h.first_summary([zero])["pending_resolutions"]["Armed"], 0)

    def test_plan_labels_blocked_rows_and_every_mfirst_field(self):
        plan = h.plan()
        for name in ("X1", "X3-gate", "X3-natural", "X4", "M-FIRST"):
            self.assertEqual(plan["rows"][name]["status"], "needs-implementation")
        self.assertEqual(plan["rows"]["X2-arrival"]["status"], "runnable-now")
        self.assertEqual(plan["rows"]["X2-instrumented"]["status"], "needs-implementation")
        self.assertEqual(set(plan["M-FIRST"]), set(h.FIRST_FIELDS))


if __name__ == "__main__":
    unittest.main()
