"""Black-box session driver cases invoked by the Rust e2e suite."""

import json
import os
from pathlib import Path
import signal
import subprocess
from subprocess import DEVNULL, Popen as real_popen
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch


sys.path.insert(0, str(Path(__file__).resolve().parents[1]
                       / "src/plugin_assets/skills/mcp-repl/scripts"))
from session_driver import (
    CleanupError, ClientError, Session, SessionError, SessionLost,
    TaskError, TaskPending, TaskRef, ToolError,
)


REPL = FIXTURE = None
LITERAL = '"hello world" \\ literal $owner | & ; $(name)\n\U0001f642'
LITERAL += r" # && || = > unbalanced {[ and literal \u0024"
QUOTED_WORD = 'probe"\\ | &{\u2603'
MARKERS = {
    "spawns": "MCP_REPL_FIXTURE_DRIVER_SPAWNS_FILE",
    "exit": "MCP_REPL_FIXTURE_EXIT_FILE",
    "mutations": "MCP_REPL_FIXTURE_DRIVER_MUTATIONS_FILE",
    "stall": "MCP_REPL_FIXTURE_DRIVER_STALL_FILE",
    "pid": "MCP_REPL_FIXTURE_DRIVER_PID_FILE",
}


class ApplicationError(Exception):
    pass


def run_worker(session, *, fail=False):
    tools = {tool["name"] for tool in session.discover().value}
    if not {"worker_start", "worker_wait", "worker_read"} <= tools:
        raise ApplicationError("fixture lacks the worker lifecycle")
    start = session.call("worker_start", {"text": LITERAL, "fail": fail})
    started = start.value["structuredContent"]
    if started["state"] != "working" or not isinstance(started["handle"], str):
        raise ApplicationError("worker did not start")
    owned = {"handle": started["handle"]}
    wait = session.call("worker_wait", owned)
    terminal = wait.value["structuredContent"]
    if terminal["state"] != "completed":
        raise ApplicationError(terminal["error"])
    result = session.call("worker_read", owned)
    if result.value["structuredContent"] != {"state": "completed", "text": LITERAL}:
        raise ApplicationError("worker result did not match the work")
    return started["handle"], (start.request_id, wait.request_id, result.request_id)


class DriverTests(unittest.TestCase):
    def setUp(self):
        if REPL is None or FIXTURE is None:
            self.skipTest("run the harness with mcp-repl and fixture paths")
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.paths = {key: Path(directory.name) / key for key in MARKERS}
        self.env = dict(os.environ)
        for key, variable in MARKERS.items():
            self.env[variable] = str(self.paths[key])

    def session(self, *args, **options):
        return Session([FIXTURE, *args], repl=REPL, env=self.env, **options)

    def assert_reaped(self, session, *, clean=True):
        self.assertEqual(session.returncode, 0)
        self.assertIsNone(session.cleanup_error)
        self.assertEqual(self.paths["spawns"].read_text(), "1")
        if clean:
            self.assertEqual(self.paths["exit"].read_text(), "clean")

    def test_stateful_worker(self):
        with self.session("--session-driver") as session:
            if os.name == "posix":
                fixture_pid = int(self.paths["pid"].read_text())
                self.assertEqual(os.getpgid(fixture_pid), session._child.pid)
            handle, ids = run_worker(session)
            self.assertIn("$owner", handle)
            self.assertEqual(ids, tuple(range(ids[0], ids[0] + 3)))
        self.assert_reaped(session)

        self.paths["spawns"].unlink()
        self.paths["exit"].unlink()
        with self.session("--session-driver") as replacement:
            with self.assertRaises(ToolError):
                replacement.call("worker_wait", {"handle": handle})
        self.assert_reaped(replacement)

    def test_application_failure(self):
        with self.session("--session-driver") as session:
            with self.assertRaisesRegex(ApplicationError, "fixture application failure"):
                run_worker(session, fail=True)
        self.assert_reaped(session)

    def test_stateless_fixture(self):
        with self.session() as session:
            tools = {tool["name"] for tool in session.discover().value}
            self.assertIn("inspect_args", tools)
            self.assertNotIn("worker_start", tools)
            args = {"text": LITERAL, "nested": {"$key": [LITERAL, True, None]}}
            result = session.call("inspect_args", args)
            self.assertEqual(json.loads(result.value["content"][0]["text"]), args)
            with self.assertRaises(ToolError) as failed:
                session.call("fail", {})
            self.assertIs(failed.exception.response["isError"], True)
            self.assertEqual(failed.exception.request_id, result.request_id + 1)
            resource = session.read("fixture://guide")
            self.assertEqual(resource.value["contents"][0]["text"], "fixture resource body")
            self.assertEqual(resource.request_id, result.request_id + 2)
        self.assert_reaped(session)

    def test_local_token_rejections_keep_session(self):
        with self.session() as session:
            first = session.discover()
            for character in ("$", "\r", "\n", "\0"):
                with self.subTest(character=repr(character)):
                    with self.assertRaises(ValueError):
                        session.call("add" + character, {})
                    with self.assertRaises(ValueError):
                        session.read("fixture://guide" + character)
            with self.assertRaises(ValueError):
                session.read("--out")
            self.assertEqual(session.discover().request_id, first.request_id + 1)
            session.close()
            session.close()
        self.assert_reaped(session)

    def test_mcp_task_settlement(self):
        with self.session(protocol="final") as session:
            task = session.start_task("slow_add", {"a": 20, "b": 22})
            with self.assertRaises(ValueError):
                session.start_task("slow_add", {"a": 1, "b": 2})
            with self.assertRaises(ValueError):
                session.wait_task(TaskRef(task.request_id, task.task_id), timeout=5)
            settled = session.wait_task(task, timeout=10)
            self.assertEqual(settled.request_id, task.request_id + 1)
            self.assertEqual(settled.value["status"], "completed")
            self.assertEqual(settled.value["result"]["content"][0]["text"], "42")
            failed = session.start_task("fail_slowly", {})
            with self.assertRaises(TaskError) as error:
                session.wait_task(failed, timeout=10)
            self.assertEqual(error.exception.response["taskId"], failed.task_id)
            self.assertIn(
                "fixture task failure",
                json.dumps(error.exception.response),
            )
        self.assert_reaped(session)
        with self.assertRaises(ValueError):
            session.wait_task(task, timeout=5)

    def test_wait_timeout_and_cancel_keep_owned_session(self):
        with self.session("--session-driver", protocol="final") as session:
            task = session.start_task("task_pending", {})
            with self.assertRaisesRegex(TaskPending, "still running") as pending:
                session.wait_task(task, timeout=0.1)
            self.assertEqual(pending.exception.request_id, task.request_id + 1)
            with self.assertRaises(ValueError):
                session.start_task("task_pending", {})
            cancelled = session.cancel_task(task)
            self.assertEqual(cancelled.value["taskId"], task.task_id)
            self.assertEqual(cancelled.value["status"], "cancelled")
            with self.assertRaises(ValueError):
                session.cancel_task(task)
            next_task = session.start_task("slow_add", {"a": 1, "b": 2})
            self.assertEqual(session.wait_task(next_task, timeout=10).value["status"], "completed")
        self.assert_reaped(session)

    def test_disconnect_does_not_replay(self):
        with self.session("--session-driver") as session:
            started = session.call("worker_start", {"text": "pending", "fail": False})
            handle = started.value["structuredContent"]["handle"]
            with self.assertRaises(SessionLost) as lost:
                session.call("worker_disconnect", {})
            self.assertEqual(lost.exception.request_id, started.request_id + 1)
            self.assertTrue(lost.exception.sent)
            with self.assertRaises(SessionLost) as closed:
                session.call("worker_wait", {"handle": handle})
            self.assertFalse(closed.exception.sent)
        self.assertEqual(self.paths["mutations"].read_text(), "1")
        self.assertFalse(self.paths["exit"].exists())
        self.assert_reaped(session, clean=False)

    def test_request_deadline_reaches_orderly_shutdown(self):
        with self.session("--session-driver", request_timeout=2) as session:
            with self.assertRaisesRegex(SessionLost, "request may still be running") as error:
                session.call("worker_stall", {})
            self.assertTrue(error.exception.sent)
            self.assertEqual(error.exception.response["exitStatus"], 4)
        self.assertEqual(self.paths["stall"].read_text(), "started")
        self.assert_reaped(session)

    @unittest.skipUnless(os.name == "posix", "POSIX SIGINT delivery")
    def test_interrupt_reaches_repl_and_server_exits_cleanly(self):
        call = self.paths["pid"].with_name("call")
        cancel = call.with_name("cancel")
        self.env["MCP_REPL_FIXTURE_CALL_FILE"] = str(call)
        self.env["MCP_REPL_FIXTURE_CANCEL_FILE"] = str(cancel)

        with self.assertRaises(KeyboardInterrupt):
            with self.session("--tools-only", "--cancel-first-call") as session:
                wait = session._state.wait

                def interrupt_after_call(timeout):
                    if call.exists():
                        raise KeyboardInterrupt
                    return wait(timeout)

                with patch.object(session._state, "wait", side_effect=interrupt_after_call):
                    session.call("add", {"a": 1, "b": 2})

        self.assertEqual(json.loads(cancel.read_text()), json.loads(call.read_text())["id"])
        self.assert_reaped(session)


def fake_peer(mode):
    stdin = sys.stdin.buffer
    stdout = sys.stdout.buffer

    def send(data):
        stdout.write(data)
        stdout.flush()

    def expect(line):
        if stdin.readline() != (line + "\n").encode("utf-8"):
            sys.exit(91)

    def hold_pipes():
        real_popen(
            [sys.executable, "-c", "import time; time.sleep(4)"],
            stdin=DEVNULL, stdout=sys.stdout, stderr=sys.stderr,
            creationflags=getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0),
        )

    if mode in ("startup-stall", "blocked-write", "inherited-kill"):
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        if os.name == "nt":
            signal.signal(signal.SIGBREAK, signal.SIG_IGN)
    if mode == "interrupted" and os.name == "posix":
        signal.signal(signal.SIGINT, lambda *_: sys.exit(0))
    if mode == "startup-stall":
        stdin.read()
        return
    if mode == "startup-banner":
        expect("builtin tools")
        send(b"connected: an older mcp-repl build\n")
        stdin.read()
        return
    if mode == "startup-error":
        send(b'{"error":"fixture startup failure","kind":"usage","exitStatus":2}\n')
        sys.exit(2)
    expect("builtin tools")
    send(b'[{"name":"probe"}]\n')
    if mode == "inherited-kill":
        hold_pipes()
    if mode in ("blocked-write", "interrupted", "inherited-kill"):
        threading.Event().wait(30)
        return
    if mode == "quoted":
        word = r'"probe\"\\ | &{' + "\u2603" + '"'
        expect("builtin call " + word + " {}")
        send(b'{"content":[]}\n')
        expect("builtin read " + word)
        send(b'{"contents":[]}\n')
    elif mode == "unknown-task":
        expect('builtin call "probe" {} &')
        send(b'{"task":{"taskId":"old"}}\n')
        expect("builtin wait last --timeout 1")
        send(b'{"error":"Task not found: old (code -32602)","kind":"server","exitStatus":3}\n')
        expect('builtin call "probe" {} &')
        send(b'{"task":{"taskId":"new"}}\n')
        expect("builtin cancel last")
        send(b'{"taskId":"new","status":"cancelled"}\n')
    elif mode == "abandon-task":
        expect('builtin call "probe" {} &')
        send(b'{"task":{"taskId":"old"}}\n')
        expect("builtin wait last --timeout 1")
        send(b'{"error":"no record for that handle","kind":"server","exitStatus":3}\n')
        expect('builtin call "probe" {} &')
        send(b'{"task":{"taskId":"new"}}\n')
        expect("builtin cancel last")
        send(b'{"taskId":"new","status":"cancelled"}\n')
    elif mode == "cooperative-cancel":
        expect('builtin call "probe" {} &')
        send(b'{"task":{"taskId":"old"}}\n')
        expect("builtin cancel last")
        send(b'{"taskId":"old","status":"working"}\n')
        expect("builtin wait last --timeout 1")
        send(b'{"taskId":"old","status":"cancelled"}\n')
        expect('builtin call "probe" {} &')
        send(b'{"task":{"taskId":"new"}}\n')
        expect("builtin cancel last")
        send(b'{"taskId":"new","status":"cancelled"}\n')
    elif mode == "quit-error":
        expect("builtin quit")
        send(b'{"error":"quit rejected","kind":"transport","exitStatus":4}\n')
        stdin.read()
        return
    elif mode == "quit-error-stall":
        expect("builtin quit")
        send(b'{"error":"quit rejected","kind":"server","exitStatus":3}\n')
        threading.Event().wait(30)
        return
    elif mode == "inherited-quit":
        hold_pipes()
    else:
        if not stdin.readline():
            return
        if mode == "exit":
            sys.exit(19)
        if mode == "partial":
            send(b'{"content":[]}')
            sys.exit(0)
        if mode == "stderr-flood":
            sys.stderr.buffer.write(b"x" * (1024 * 1024) + b"stderr tail\n")
            sys.stderr.buffer.flush()
        data = {
            "oversized": b"x" * 32768,
            "malformed": b"this is not JSON\n",
            "invalid-utf8": b"\xff\n",
            "nonfinite": b'{"content":[],"structuredContent":NaN}\n',
            "wrong-shape": b"[]\n",
            "extra": b'{"content":[]}\n[]\n',
            "close-error": b'{"content":[]}\n',
            "stderr-flood": b'{"content":[]}\n',
            "client-error": b'{"error":"fixture command failure","kind":"usage","exitStatus":2}\n',
        }[mode]
        send(data)
    for line in stdin:
        if line != b"builtin quit\n":
            sys.exit(92)
        send(b'{"exit":true}\n')
        sys.exit(18 if mode == "close-error" else 0)


class PipeTests(unittest.TestCase):
    def session(self, mode, **options):
        popen = fake_popen(mode)
        patcher = patch("session_driver.subprocess.Popen", side_effect=popen)
        patcher.start()
        self.addCleanup(patcher.stop)
        return Session([sys.executable], repl=sys.executable, **options)

    def test_bad_output_ends_session(self):
        for mode in ("oversized", "malformed", "invalid-utf8", "nonfinite",
                     "wrong-shape", "partial", "exit", "extra"):
            with self.subTest(mode=mode), patch(
                "session_driver.subprocess.Popen", side_effect=fake_popen(mode),
            ):
                session = Session([sys.executable], repl=sys.executable, max_frame_bytes=256)
                expected = (SessionLost, CleanupError) if mode == "extra" else SessionLost
                with self.assertRaises(expected):
                    with session:
                        session.call("probe", {})
                self.assertEqual(session.returncode, 19 if mode == "exit" else 0)
                if mode == "exit":
                    with self.assertRaisesRegex(CleanupError, "status 19"):
                        session.close()
                    session.close()
                elif mode != "extra":
                    self.assertIsNone(session.cleanup_error)

    def test_stderr_does_not_block_stdout(self):
        with self.session("stderr-flood", stderr_bytes=256) as session:
            self.assertEqual(session.call("probe", {}).value["content"], [])
        self.assertEqual(len(session.stderr_tail), 256)
        self.assertTrue(session.stderr_tail.endswith("stderr tail\n"))

    def test_client_error_is_correlated(self):
        with self.session("client-error") as session:
            with self.assertRaises(ClientError) as error:
                session.call("probe", {})
            self.assertEqual(error.exception.request_id, 2)
            self.assertEqual(error.exception.response["exitStatus"], 2)
        self.assertEqual(session.returncode, 0)

    def test_quoted_tool_and_uri_tokens(self):
        with self.session("quoted") as session:
            self.assertEqual(session.call(QUOTED_WORD, {}).value, {"content": []})
            self.assertEqual(session.read(QUOTED_WORD).value, {"contents": []})

    def test_unknown_task_releases_reference(self):
        with self.session("unknown-task") as session:
            old = session.start_task("probe", {})
            with self.assertRaisesRegex(ClientError, "Task not found"):
                session.wait_task(old, timeout=1)
            with self.assertRaises(ValueError):
                session.cancel_task(old)
            new = session.start_task("probe", {})
            self.assertEqual(session.cancel_task(new).value["taskId"], "new")

    def test_abandon_task_releases_reference_without_writing(self):
        with self.session("abandon-task") as session:
            task = session.start_task("probe", {})
            with self.assertRaisesRegex(ClientError, "no record") as error:
                session.wait_task(task, timeout=1)
            session.abandon_task(task)
            with self.assertRaises(ValueError):
                session.abandon_task(task)
            new = session.start_task("probe", {})
            self.assertEqual(new.request_id, error.exception.request_id + 1)
            self.assertEqual(session.cancel_task(new).value["taskId"], "new")

    def test_nonterminal_cancel_keeps_task_for_wait(self):
        with self.session("cooperative-cancel") as session:
            task = session.start_task("probe", {})
            self.assertEqual(session.cancel_task(task).value["status"], "working")
            with self.assertRaises(ValueError):
                session.start_task("probe", {})
            with self.assertRaises(TaskError) as error:
                session.wait_task(task, timeout=1)
            self.assertEqual(error.exception.response["status"], "cancelled")
            new = session.start_task("probe", {})
            self.assertEqual(session.cancel_task(new).value["taskId"], "new")

    def test_startup_error_is_decoded(self):
        session = self.session("startup-error")
        with self.assertRaisesRegex(ClientError, "fixture startup failure"):
            with session:
                self.fail("startup error should prevent entering the session")
        self.assertEqual(session.returncode, 2)

    def test_startup_deadline(self):
        session = self.session("startup-stall", startup_timeout=0.5)
        with self.assertRaisesRegex(SessionLost, "deadline expired"):
            with session:
                self.fail("unresponsive startup should prevent entering the session")
        self.assertEqual(session.returncode, 0)

    def test_old_version_fails_before_startup(self):
        session = self.session("old-version")
        with patch("session_driver.MIN_REPL_VERSION", "0.4.1"):
            with self.assertRaisesRegex(SessionError, r"need mcp-repl 0\.4\.1.*0\.4\.0"):
                with session:
                    self.fail("an old binary should prevent entering the session")
        self.assertIsNone(session.returncode)

    def test_source_copy_skips_version_probe(self):
        with patch("session_driver.subprocess.run", side_effect=AssertionError("version probe")):
            with self.session("quoted") as session:
                session.call(QUOTED_WORD, {})
                session.read(QUOTED_WORD)

    def test_startup_banner_error_is_clear(self):
        with self.assertRaisesRegex(SessionLost, "startup stdout is not JSON.*piped --json"):
            with self.session("startup-banner"):
                self.fail("a banner does not match the startup framing")

    def test_fault_before_write_reports_unsent(self):
        with self.session("client-error") as session:
            with session._state:
                session._fault = "unsolicited stdout frame"
            with self.assertRaises(SessionLost) as error:
                session.call("probe", {})
            self.assertFalse(error.exception.sent)

    def test_blocked_write_and_forced_cleanup(self):
        with self.assertRaisesRegex(CleanupError, "forced mcp-repl shutdown"):
            with self.session("blocked-write", request_timeout=0.5, shutdown_timeout=0.25) as session:
                started = time.monotonic()
                with self.assertRaisesRegex(SessionLost, "deadline expired") as error:
                    session.call("probe", {"data": "x" * (2 * 1024 * 1024)})
                self.assertTrue(error.exception.sent)
                self.assertLess(time.monotonic() - started, 6)
        self.assertIsNotNone(session.returncode)
        if os.name == "posix":
            self.assertEqual(session.returncode, -signal.SIGKILL)
        self.assertTrue(session._reaped)
        session.close()

    def test_keyboard_interrupt_closes_the_session(self):
        with self.assertRaises(KeyboardInterrupt):
            with self.session("interrupted", shutdown_timeout=0.25) as session:
                with patch.object(session._state, "wait", side_effect=KeyboardInterrupt):
                    session.call("probe", {})
        with self.assertRaises(SessionLost):
            session.call("probe", {})
        self.assertIsNotNone(session.returncode)
        if os.name == "posix":
            self.assertEqual(session.returncode, 0)
        self.assertTrue(session._reaped)

    def test_close_resumes_interrupted_reap(self):
        session = self.session("blocked-write", shutdown_timeout=0.25)
        session.__enter__()
        with patch.object(session._child, "wait", side_effect=KeyboardInterrupt):
            with self.assertRaises(KeyboardInterrupt):
                session._stop(interrupt=True)
        self.assertTrue(session._closed)
        self.assertFalse(session._reaped)
        with self.assertRaisesRegex(CleanupError, "forced mcp-repl shutdown"):
            session.close()
        self.assertTrue(session._reaped)
        session.close()

    def test_quit_with_inherited_pipes(self):
        with self.session("inherited-quit") as session:
            pass
        self.assertTrue(session._reaped)
        self.assertIsNone(session.cleanup_error)

    @unittest.skipUnless(os.name == "nt", "Windows forced-kill behavior")
    def test_windows_forced_kill_with_inherited_pipes(self):
        with self.assertRaisesRegex(CleanupError, "forced mcp-repl shutdown"):
            with self.session("inherited-kill", request_timeout=0.5, shutdown_timeout=0.25) as session:
                with self.assertRaises(SessionLost):
                    session.call("probe", {})
        self.assertTrue(session._reaped)
        session.close()

    def test_nonzero_exit_after_quit(self):
        with self.assertRaisesRegex(CleanupError, "status 18"):
            with self.session("close-error") as session:
                session.call("probe", {})
        self.assertEqual(session.returncode, 18)

    def test_cleanup_error_keeps_quit_failure(self):
        with self.assertRaisesRegex(CleanupError, "quit rejected") as error:
            with self.session("quit-error") as session:
                pass
        cause = error.exception.cause
        self.assertIs(session.cleanup_error, error.exception)
        self.assertIs(error.exception.__cause__, cause)
        self.assertIsInstance(cause, SessionLost)
        self.assertEqual(cause.request_id, 2)
        self.assertTrue(cause.sent)
        self.assertEqual(cause.response["exitStatus"], 4)

    def test_forced_cleanup_keeps_quit_failure(self):
        with self.assertRaisesRegex(CleanupError, "forced mcp-repl shutdown") as error:
            with self.session("quit-error-stall", shutdown_timeout=0.25):
                pass
        self.assertIsInstance(error.exception.cause, ClientError)
        self.assertEqual(error.exception.cause.response["exitStatus"], 3)


def fake_popen(mode):
    def launch(argv, **kwargs):
        if argv[1:] == ["--version"]:
            version = "0.4.0" if mode == "old-version" else "0.4.1"
            return real_popen(
                [sys.executable, "-c", "print('mcp-repl " + version + "')"], **kwargs,
            )
        return real_popen([sys.executable, "-B", __file__, "fake", mode], **kwargs)
    return launch


if __name__ == "__main__":
    if sys.argv[1] == "fake":
        fake_peer(sys.argv[2])
    else:
        REPL, FIXTURE = sys.argv[1:3]
        unittest.main(argv=[sys.argv[0]], verbosity=2)
