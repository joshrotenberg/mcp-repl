"""Black-box session driver cases invoked by the Rust e2e suite."""

from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import queue
import signal
import socket
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
import session_driver
from session_driver import (
    CleanupError, ClientError, Session, SessionError, SessionLost,
    TaskError, TaskPending, TaskRef, ToolError, accept, cancel, confirm_url, decline, sample,
)


REPL = FIXTURE = None
LITERAL = '"hello world" \\ literal $owner | & ; $(name)\n\U0001f642'
LITERAL += r" # && || = > unbalanced {[ and literal \u0024"
QUOTED_WORD = 'probe"\\ | &{\u2603'
FORM_ANSWER = "bridge-private-answer"
SAMPLE_TEXT = "bridge-private-sample"
SAMPLE_RESULT = {
    "model": "fixture-model", "role": "assistant",
    "content": {"type": "text", "text": SAMPLE_TEXT},
}
MARKERS = {
    "spawns": "MCP_REPL_FIXTURE_DRIVER_SPAWNS_FILE",
    "exit": "MCP_REPL_FIXTURE_EXIT_FILE",
    "mutations": "MCP_REPL_FIXTURE_DRIVER_MUTATIONS_FILE",
    "stall": "MCP_REPL_FIXTURE_DRIVER_STALL_FILE",
    "pid": "MCP_REPL_FIXTURE_DRIVER_PID_FILE",
}


# A `wait --timeout` that ran out, as each mcp-repl release reports it. The
# structured envelopes use wording no legacy matcher accepts, so only their
# fields can make them pending. Then 0.5.0, then the wording before it.
PENDING_ENVELOPES = (
    {"error": "the wait for this task ran out; fields decide", "kind": "timeout",
     "exitStatus": 4, "taskId": "t1", "pending": True},
    {"error": "task 1 (t1) did not settle before --timeout; the host's answers were not sent",
     "kind": "timeout", "exitStatus": 4, "taskId": "t1", "pending": True,
     "answersSent": False},
    {"error": "Transport error: task t1 did not settle within 1s (--timeout)",
     "kind": "transport", "exitStatus": 4},
    {"error": "task t1 was still running after 1s (--timeout)",
     "kind": "transport", "exitStatus": 4},
)
LEGACY_TEXT = "task t1 did not settle within 1s (--timeout)"
# Exit status 4 envelopes that do not describe this task still pending.
NOT_PENDING_ENVELOPES = {
    "pending-other-task": {"error": LEGACY_TEXT, "kind": "timeout", "exitStatus": 4,
                           "taskId": "t2", "pending": True},
    "pending-unmarked-timeout": {"error": LEGACY_TEXT, "kind": "timeout",
                                 "exitStatus": 4, "taskId": "t1"},
    "pending-not-true": {"error": LEGACY_TEXT, "kind": "timeout", "exitStatus": 4,
                         "taskId": "t1", "pending": "true"},
    "pending-transport": {"error": "Transport error: connection closed",
                          "kind": "transport", "exitStatus": 4},
    # A transport envelope with structured fields is not a legacy one, even
    # with the legacy wording.
    "pending-transport-flagged": {"error": "Transport error: " + LEGACY_TEXT,
                                  "kind": "transport", "exitStatus": 4, "pending": True},
    "pending-transport-other-task": {"error": LEGACY_TEXT, "kind": "transport",
                                     "exitStatus": 4, "taskId": "t2"},
    "pending-transport-same-task": {"error": LEGACY_TEXT, "kind": "transport",
                                    "exitStatus": 4, "taskId": "t1"},
    # The reconnect a wait deadline interrupted is a connection failure.
    "pending-reconnect": {"error": "Transport error: the connection was lost while waiting "
                                   "for task t1, and the reconnect did not finish within 1s "
                                   "(--timeout)",
                          "kind": "transport", "exitStatus": 4},
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


class _FixtureTests(unittest.TestCase):
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


class DriverTests(_FixtureTests):
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
            with self.assertRaisesRegex(TaskPending, "did not settle|still running") as pending:
                session.wait_task(task, timeout=0.1)
            self.assertEqual(pending.exception.request_id, task.request_id + 1)
            self.assertEqual(str(pending.exception), pending.exception.response["error"])
            self.assertNotIn("Transport error", str(pending.exception))
            self.assertEqual(pending.exception.response["kind"], "timeout")
            self.assertIs(pending.exception.response["pending"], True)
            self.assertEqual(pending.exception.response["taskId"], task.task_id)
            self.assertEqual(pending.exception.response["exitStatus"], 4)
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


@unittest.skipUnless(os.name == "posix", "the host input bridge requires Unix")
class HostInputTests(_FixtureTests):
    def setUp(self):
        super().setUp()
        self.env["MCP_REPL_FIXTURE_HOST_INPUT"] = "1"
        self.env["RUST_LOG"] = "trace"

    def test_host_form_url_and_sampling(self):
        cases = [
            ("host_form", accept({"apiKey": FORM_ANSWER}), "form accept"),
            ("host_form", decline(), "form decline"),
            ("host_form", cancel(), "form cancel"),
            ("host_url", decline(), "url decline"),
            ("host_url", confirm_url(), "url accept"),
            ("host_sample", sample(SAMPLE_RESULT), "sampled by fixture-model"),
            ("host_sample", sample(dict(SAMPLE_RESULT, model="mcp-repl/operator"),
                                   origin="operator"), "sampled by mcp-repl/operator"),
        ]
        answers = iter(answer for _, answer, _ in cases)
        requests = queue.Queue()

        def handle(request):
            requests.put(request)
            return next(answers)

        with self.session(host_input=handle) as session:
            first = session.discover().request_id
            for index, (tool, _, expected) in enumerate(cases, 1):
                reply = session.call(tool, {})
                request = requests.get(timeout=5)
                self.assertEqual(reply.request_id, first + index)
                self.assertEqual(reply.value["content"][0]["text"], expected)
                self.assertEqual(request.kind, "sampling" if tool == "host_sample" else tool[5:])
                self.assertEqual(request.server["name"], "mcp-repl-fixture")
                self.assertGreater(request.deadline, time.monotonic())
                if request.kind == "form":
                    self.assertEqual(request.params["message"], "Provide a synthetic value")
                    self.assertEqual(request.params["schema"]["required"], ["apiKey"])
                    self.assertEqual(request.params["credentialFields"], ["apiKey"])
                elif request.kind == "url":
                    self.assertTrue(request.params["url"].startswith("https://example.com/"))
                    self.assertIn("confirm_url", request.actions)
                else:
                    self.assertEqual(request.params["maxTokens"], 24)
                status = session.next_host_input_status(timeout=5)
                self.assertEqual((status.session, status.id), (request.session, request.id))
                self.assertEqual(status.status, "accepted")
            self.assertEqual(session.host_input_errors, ())
        self.assert_reaped(session)
        for secret in (FORM_ANSWER, SAMPLE_TEXT, "fixture-url-token"):
            self.assertNotIn(secret, session.stderr_tail)
        self.assertNotIn(FORM_ANSWER, repr(cases))

    def test_host_handler_exception_declines(self):
        def handle(request):
            raise ValueError(FORM_ANSWER)

        with self.session(host_input=handle) as session:
            reply = session.call("host_form", {})
            self.assertEqual(reply.value["content"][0]["text"], "form decline")
            status = session.next_host_input_status(timeout=5)
            self.assertEqual(status.status, "accepted")
            error, = session.host_input_errors
            self.assertEqual((error.session, error.id), (status.session, status.id))
            self.assertEqual(error.reason, "handler_exception")
        self.assert_reaped(session)
        self.assertNotIn(FORM_ANSWER, repr(session.host_input_errors) + session.stderr_tail)

    def test_host_invalid_answers_are_declined_locally(self):
        cases = [
            ("host_form", accept("x")),
            ("host_form", accept({})),
            ("host_form", accept({1: FORM_ANSWER, "apiKey": FORM_ANSWER})),
            ("host_form", confirm_url()),
            ("host_sample", sample(SAMPLE_RESULT, origin="Model")),
            ("host_sample", sample(dict(SAMPLE_RESULT, role="user"))),
            ("host_sample", sample(SAMPLE_RESULT, origin="operator")),
            ("host_sample", sample(dict(SAMPLE_RESULT, model="mcp-repl/canned"))),
        ]
        answers = iter(answer for _, answer in cases)
        with self.session(host_input=lambda request: next(answers)) as session:
            for index, (tool, answer) in enumerate(cases, 1):
                with self.subTest(tool=tool, answer=answer):
                    if tool == "host_sample":
                        with self.assertRaises(ToolError) as failed:
                            session.call(tool, {})
                        self.assertNotIn(SAMPLE_TEXT, repr(failed.exception.response))
                    else:
                        reply = session.call(tool, {})
                        self.assertEqual(reply.value["content"][0]["text"], "form decline")
                    status = session.next_host_input_status(timeout=5)
                    self.assertEqual(status.status, "accepted")
                    self.assertEqual(len(session.host_input_errors), index)
                    error = session.host_input_errors[-1]
                    self.assertEqual((error.session, error.id), (status.session, status.id))
                    self.assertEqual(error.reason, "invalid_answer")
            self.assertIsNone(session.next_host_input_status(timeout=0))
        self.assert_reaped(session)
        for secret in (FORM_ANSWER, SAMPLE_TEXT):
            self.assertNotIn(secret, repr(session.host_input_errors) + session.stderr_tail)

    def test_host_rejected_answers_get_one_decline(self):
        cases = [
            (accept({"apiKey": {"private": FORM_ANSWER}}), "malformed"),
            (accept({"apiKey": 42}), "invalid_payload"),
        ]
        answers = iter(answer for answer, _ in cases)
        with self.session(host_input=lambda request: next(answers), host_input_timeout=10) as session:
            for index, (_, reason) in enumerate(cases, 1):
                reply = session.call("host_form", {})
                self.assertEqual(reply.value["content"][0]["text"], "form decline")
                rejected = session.next_host_input_status(timeout=5)
                accepted = session.next_host_input_status(timeout=5)
                self.assertEqual((rejected.status, rejected.reason), ("rejected", reason))
                self.assertEqual(accepted.status, "accepted")
                key = (rejected.session, rejected.id)
                self.assertEqual(key, (accepted.session, accepted.id))
                self.assertEqual(len(session.host_input_errors), index)
                error = session.host_input_errors[-1]
                self.assertEqual((error.session, error.id), key)
                self.assertEqual(error.reason, "answer_rejected")
        self.assert_reaped(session)
        self.assertNotIn(FORM_ANSWER, repr(session.host_input_errors) + session.stderr_tail)

    def test_host_handler_timeout_declines(self):
        finished = threading.Event()

        def handle(request):
            request.cancelled.wait(5)
            finished.set()
            return accept({"apiKey": FORM_ANSWER})

        with self.session(host_input=handle, host_input_timeout=1) as session:
            reply = session.call("host_form", {})
            self.assertIn(reply.value["content"][0]["text"], ("form decline", "form cancel"))
            self.assertTrue(finished.wait(5))
            status = session.next_host_input_status(timeout=5)
            error, = session.host_input_errors
            self.assertEqual((error.session, error.id), (status.session, status.id))
            self.assertEqual(error.reason, "handler_timeout")
            if status.status == "rejected":
                self.assertEqual(status.reason, "stale")
            else:
                self.assertEqual(status.status, "accepted")
        self.assert_reaped(session)
        self.assertNotIn(FORM_ANSWER, repr(session.host_input_errors) + session.stderr_tail)

    def test_host_server_cancellation_rejects_late_answer(self):
        trigger = self.paths["exit"].with_name("trigger")
        unexpected_reply = trigger.with_name("unexpected-reply")
        self.env["MCP_REPL_FIXTURE_HOST_TRIGGER"] = str(trigger)
        self.env["MCP_REPL_FIXTURE_UNEXPECTED_HOST_REPLY"] = str(unexpected_reply)
        requests = queue.Queue()

        def handle(request):
            requests.put(request)
            trigger.touch()
            if request.cancelled.wait(5):
                return accept({"apiKey": FORM_ANSWER})
            raise RuntimeError("fixture did not cancel the question")

        with self.session("--tools-only", host_input=handle) as session:
            reply = session.call("add", {"a": 1, "b": 2})
            self.assertEqual(reply.value["content"][0]["text"], "server cancelled")
            request = requests.get(timeout=5)
            status = session.next_host_input_status(timeout=5)
            self.assertTrue(request.cancelled.is_set())
            self.assertEqual((status.session, status.id), (request.session, request.id))
            self.assertEqual((status.status, status.reason), ("rejected", "stale"))
            self.assertEqual(session.host_input_errors, ())
        self.assert_reaped(session)
        self.assertFalse(unexpected_reply.exists())

    def test_host_final_parallel_inputs_and_sampling_refusal(self):
        sampling_started = threading.Event()
        refuse = threading.Event()

        def handle(request):
            if request.kind == "sampling":
                sampling_started.set()
                if refuse.is_set():
                    raise RuntimeError(SAMPLE_TEXT)
                return sample(SAMPLE_RESULT)
            if not sampling_started.wait(5):
                raise RuntimeError("sampling was blocked by the form")
            return decline()

        with self.session(host_input=handle, protocol="final") as session:
            reply = session.call("host_rounds", {})
            self.assertEqual(reply.value["content"][0]["text"], "final decline / fixture-model")
            self.assertEqual(session.host_input_errors, ())
            for _ in range(2):
                self.assertEqual(session.next_host_input_status(timeout=5).status, "accepted")
            sampling_started.clear()
            refuse.set()
            with self.assertRaises(ClientError) as failed:
                session.call("host_rounds", {})
            self.assertEqual(failed.exception.response["exitStatus"], 5)
            self.assertNotIn(SAMPLE_TEXT, str(failed.exception))
            error, = session.host_input_errors
            self.assertEqual(error.reason, "handler_exception")
        self.assert_reaped(session)
        self.assertNotIn(SAMPLE_TEXT, repr(session.host_input_errors) + session.stderr_tail)

    def test_host_socket_can_replace_closed_standard_descriptors(self):
        result = subprocess.run(
            [sys.executable, "-B", __file__, "host-low-fd", REPL, FIXTURE], env=self.env,
            stdin=DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stdout.decode("utf-8"))


def exercise_low_fds(repl, fixture):
    socketpair = socket.socketpair

    def low_socketpair():
        host, repl_end = socketpair()
        assert repl_end.fileno() < 3
        return host, repl_end

    os.close(0)
    os.close(2)
    try:
        with patch("session_driver.socket.socketpair", low_socketpair):
            with Session([fixture], repl=repl, host_input=lambda request: decline()) as session:
                response = session.call("host_form", {})
                assert response.value["content"][0]["text"] == "form decline"
                assert session.next_host_input_status(timeout=5).status == "accepted"
    except Exception as error:
        print(type(error).__name__, str(error))
        sys.exit(1)


def fake_host_input(mode, fd, expect, send):
    def frame(**fields):
        return json.dumps(dict(v=1, session="fake-1", **fields)).encode("utf-8") + b"\n"

    def question(request_id, seconds=20):
        return frame(
            type="needs_input", id=request_id, kind="form", server={},
            request={"message": "Proceed?", "schema": {}},
            actions=["accept", "decline", "cancel"],
            deadlineUnixMs=int((time.time() + seconds) * 1000),
        )

    def withdrawn(request_id):
        return frame(type="withdrawn", id=request_id, reason="server_cancelled")

    def expect_declines(channel, ids):
        with channel.makefile("rb") as reader:
            replies = [json.loads(reader.readline()) for _ in ids]
        if {reply.get("id") for reply in replies} != set(ids) or any(
            reply != {"v": 1, "type": "response", "session": "fake-1",
                      "id": reply["id"], "action": "decline"}
            for reply in replies
        ):
            sys.exit(94)

    fd = int(fd)
    with socket.socket(fileno=fd) as channel:
        expect('builtin call "probe" {}')
        invalid_frames = {
            "host-malformed": b'{' + FORM_ANSWER.encode() + b'\n',
            "host-oversized": b'x' * (1024 * 1024),
            "host-truncated": b'{"v":1',
        }
        if mode in invalid_frames:
            channel.sendall(invalid_frames[mode])
            channel.shutdown(socket.SHUT_WR)
            if channel.recv(1):
                sys.exit(93)
            send(b'{"content":[]}\n')
            expect("builtin quit")
            send(b'{"exit":true}\n')
            return
        if mode == "host-exit":
            # This keeps the peer endpoint alive until the driver closes its side.
            real_popen(
                [sys.executable, "-c",
                 "import socket, sys; s = socket.socket(fileno=int(sys.argv[1])); "
                 "s.settimeout(15); s.recv(1)", str(fd)],
                stdin=DEVNULL, stdout=DEVNULL, stderr=DEVNULL, pass_fds=(fd,),
            )
        if mode == "host-timeout":
            # Future v1 extensions have no bearing on the question or its response.
            channel.sendall(b'{"v":1,"type":"heartbeat"}\n')
            channel.sendall(frame(type="response_status", id="1", status="duplicate"))
            channel.sendall(frame(type="response_status", id="1", status="future"))
        ids = ["1"] if mode in ("host-timeout", "host-withdrawn") else ["1", "2"]
        channel.sendall(b"".join(
            question(request_id, 0.5 if mode == "host-timeout" else 20)
            for request_id in ids
        ))
        if mode == "host-busy":
            send(b'{"content":[]}\n')
            expect('builtin call "probe" {}')
            extra = [str(number) for number in range(3, 73)]
            channel.sendall(b"".join(withdrawn(key) for key in ids))
            channel.sendall(b"".join(question(key) + withdrawn(key) for key in extra))
            expect_declines(channel, ids + extra)
            send(b'{"content":[]}\n')
        if mode == "host-withdrawn":
            send(b'{"content":[]}\n')
            expect('builtin call "probe" {}')
            channel.sendall(withdrawn("1"))
            expect_declines(channel, ids)
            channel.sendall(frame(type="response_status", id="1", status="rejected",
                                  reason="stale"))
            send(b'{"content":[]}\n')
        if mode in ("host-eof", "host-exit"):
            send(b'{"content":[]}\n')
        if mode == "host-eof":
            expect('builtin call "probe" {}')
            channel.shutdown(socket.SHUT_WR)
            if channel.recv(1):
                sys.exit(93)
            send(b'{"content":[]}\n')
        if mode in ("host-close", "host-timeout"):
            expect_declines(channel, ids)
            channel.sendall(frame(type="response_status", id="1", status="rejected",
                                  reason="duplicate"))
            send(b'{"content":[]}\n')
        expect("builtin quit")
        send(b'{"exit":true}\n')


def fake_peer(mode, fd=None):
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
    if mode.startswith("host-"):
        fake_host_input(mode, fd, expect, send)
        return
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
    elif mode == "pending-task":
        expect('builtin call "probe" {} &')
        send(b'{"task":{"taskId":"t1"}}\n')
        for envelope in PENDING_ENVELOPES:
            expect("builtin wait last --timeout 1")
            send(json.dumps(envelope).encode("utf-8") + b"\n")
        expect("builtin cancel last")
        send(b'{"taskId":"t1","status":"cancelled"}\n')
    elif mode in NOT_PENDING_ENVELOPES:
        expect('builtin call "probe" {} &')
        send(b'{"task":{"taskId":"t1"}}\n')
        expect("builtin wait last --timeout 1")
        send(json.dumps(NOT_PENDING_ENVELOPES[mode]).encode("utf-8") + b"\n")
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

    def test_wait_timeout_is_pending_for_current_and_older_builds(self):
        with self.session("pending-task") as session:
            task = session.start_task("probe", {})
            for envelope in PENDING_ENVELOPES:
                with self.subTest(kind=envelope["kind"], error=envelope["error"]):
                    with self.assertRaises(TaskPending) as pending:
                        session.wait_task(task, timeout=1)
                    self.assertEqual(str(pending.exception), envelope["error"])
                    self.assertEqual(pending.exception.response, envelope)
            self.assertEqual(session.cancel_task(task).value["status"], "cancelled")
        self.assertEqual(session.returncode, 0)

    def test_wait_failure_that_is_not_this_task_pending_ends_session(self):
        for mode in NOT_PENDING_ENVELOPES:
            with self.subTest(mode=mode), patch(
                "session_driver.subprocess.Popen", side_effect=fake_popen(mode),
            ):
                with Session([sys.executable], repl=sys.executable) as session:
                    task = session.start_task("probe", {})
                    with self.assertRaises(SessionLost) as lost:
                        session.wait_task(task, timeout=1)
                self.assertEqual(session.returncode, 0)

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

    def test_host_input_requires_unix(self):
        with patch("session_driver.os.name", "nt"):
            with self.assertRaisesRegex(ValueError, "host_input requires Unix"):
                Session([sys.executable], host_input=lambda request: decline())

    @unittest.skipUnless(os.name == "posix", "the host input bridge requires Unix")
    def test_host_default_request_timeout_leaves_room_for_input(self):
        for request_timeout, host_timeout, effective in ((None, 30, 32), (None, 120, 122),
                                                          (10, 30, 10)):
            with self.subTest(request_timeout=request_timeout, host_timeout=host_timeout):
                with patch("session_driver.subprocess.Popen",
                           side_effect=fake_popen("host-timeout")) as launch:
                    with Session(
                        [sys.executable], repl=sys.executable, host_input=lambda request: decline(),
                        request_timeout=request_timeout, host_input_timeout=host_timeout,
                    ) as session:
                        session.call("probe", {})
                    argv = launch.call_args[0][0]
                self.assertEqual(argv[argv.index("--timeout") + 1], str(effective))
                self.assertEqual(argv[argv.index("--host-input-timeout") + 1], str(host_timeout))

    @unittest.skipUnless(os.name == "posix", "the host input bridge requires Unix")
    def test_host_timeout_sends_decline_and_surfaces_duplicate(self):
        def handle(request):
            request.cancelled.wait(5)
            return accept({"secret": FORM_ANSWER})

        with self.session("host-timeout", host_input=handle) as session:
            session.call("probe", {})
            status = session.next_host_input_status(timeout=5)
            self.assertEqual((status.status, status.reason), ("rejected", "duplicate"))
            self.assertEqual([error.reason for error in session.host_input_errors],
                             ["handler_timeout"])
        self.assertNotIn(FORM_ANSWER, repr(session.host_input_errors) + session.stderr_tail)

    @unittest.skipUnless(os.name == "posix", "the host input bridge requires Unix")
    def test_host_withdrawal_drops_the_handler_answer(self):
        original_run = session_driver._HostCall.run
        for started in (False, True):
            with self.subTest(started=started):
                ready = threading.Event()
                release = threading.Event()
                workers = queue.Queue()
                handled = threading.Event()

                def run(call, handler, slots):
                    workers.put((call.request, threading.current_thread()))
                    if not started:
                        ready.set()
                        release.wait(10)
                    original_run(call, handler, slots)

                def handle(request):
                    handled.set()
                    ready.set()
                    release.wait(10)
                    return accept({"secret": FORM_ANSWER})

                with patch.object(session_driver._HostCall, "run", run):
                    with self.session("host-withdrawn", host_input=handle) as session:
                        session.call("probe", {})
                        self.assertTrue(ready.wait(5))
                        request, worker = workers.get(timeout=5)
                        try:
                            session.call("probe", {})
                            self.assertTrue(request.cancelled.is_set())
                            status = session.next_host_input_status(timeout=5)
                            self.assertEqual((status.status, status.reason), ("rejected", "stale"))
                            self.assertEqual(session.host_input_errors, ())
                        finally:
                            release.set()
                        worker.join(timeout=5)
                        self.assertFalse(worker.is_alive())
                        self.assertEqual(handled.is_set(), started)

    @unittest.skipUnless(os.name == "posix", "the host input bridge requires Unix")
    def test_host_busy_refuses_a_burst_of_questions_without_ending_the_session(self):
        started = queue.Queue()
        release = threading.Event()

        def handle(request):
            started.put(request)
            release.wait(10)
            return accept({"secret": FORM_ANSWER})

        with patch.object(session_driver, "_HOST_MAX_HANDLERS", 2):
            with self.session("host-busy", host_input=handle) as session:
                session.call("probe", {})
                handlers = [started.get(timeout=5) for _ in range(2)]
                try:
                    session.call("probe", {})
                    self.assertTrue(all(request.cancelled.is_set() for request in handlers))
                    self.assertTrue(started.empty())
                    errors = session.host_input_errors
                    self.assertEqual([(error.id, error.reason) for error in errors],
                                     [(str(number), "handler_busy") for number in range(3, 73)])
                    self.assertIsNone(session.returncode)
                finally:
                    release.set()
        self.assertEqual(session.returncode, 0)

    @unittest.skipUnless(os.name == "posix", "the host input bridge requires Unix")
    def test_host_invalid_inbound_frames_close_the_bridge_with_sanitized_error(self):
        for mode in ("host-malformed", "host-oversized", "host-truncated"):
            with self.subTest(mode=mode):
                with self.session(mode, host_input=lambda request: decline()) as session:
                    session.call("probe", {})
                    self.assertIsNone(session.next_host_input_status(timeout=5))
                    error, = session.host_input_errors
                    self.assertEqual((error.session, error.id, error.reason),
                                     (None, None, "bridge_error"))
                self.assertNotIn(FORM_ANSWER, repr(session.host_input_errors) + session.stderr_tail)

    @unittest.skipUnless(os.name == "posix", "the host input bridge requires Unix")
    def test_host_close_eof_and_child_exit_release_handlers(self):
        for mode in ("host-close", "host-eof", "host-exit"):
            with self.subTest(mode=mode):
                started = queue.Queue()

                def handle(request):
                    started.put((request, threading.current_thread()))
                    request.cancelled.wait(15)
                    return accept({"secret": FORM_ANSWER})

                with patch("session_driver.subprocess.Popen", side_effect=fake_popen(mode)):
                    session = Session([sys.executable], repl=sys.executable, host_input=handle)
                    session.__enter__()
                self.addCleanup(self.close_host_peer, session)
                with ThreadPoolExecutor(max_workers=1) as executor:
                    reply = executor.submit(session.call, "probe", {})
                    handlers = [started.get(timeout=5) for _ in range(2)]
                    if mode == "host-close":
                        session.close()
                        self.assertEqual(session.next_host_input_status(timeout=5).reason,
                                         "duplicate")
                    reply.result(timeout=5)
                    if mode == "host-eof":
                        session.call("probe", {})
                        self.assertIsNone(session.returncode)
                    elif mode == "host-exit":
                        session._child.terminate()
                        session._child.wait(timeout=5)
                for request, worker in handlers:
                    self.assertTrue(request.cancelled.wait(5))
                    worker.join(timeout=5)
                    self.assertFalse(worker.is_alive())
                session._host_input._thread.join(timeout=5)
                self.assertFalse(session._host_input._thread.is_alive())
                self.assertIsNone(session.next_host_input_status(timeout=5))

    @staticmethod
    def close_host_peer(session):
        try:
            session.close()
        except CleanupError:
            if session.returncode != -signal.SIGTERM:
                raise


def fake_popen(mode):
    def launch(argv, **kwargs):
        if argv[1:] == ["--version"]:
            version = "0.4.0" if mode == "old-version" else "0.4.1"
            return real_popen(
                [sys.executable, "-c", "print('mcp-repl " + version + "')"], **kwargs,
            )
        descriptors = [str(fd) for fd in kwargs.get("pass_fds", ())]
        return real_popen([sys.executable, "-B", __file__, "fake", mode, *descriptors], **kwargs)
    return launch


if __name__ == "__main__":
    if sys.argv[1] == "fake":
        fake_peer(*sys.argv[2:])
    elif sys.argv[1] == "host-low-fd":
        exercise_low_fds(*sys.argv[2:])
    else:
        REPL, FIXTURE = sys.argv[1:3]
        unittest.main(argv=[sys.argv[0]], verbosity=2)
