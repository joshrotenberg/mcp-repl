"""Own a single stdio mcp-repl session from a Python 3.8+ workflow.

On Unix, Session(argv, host_input=handler, host_input_timeout=30) calls
handler(HostInputRequest) for each bridge question. The request has kind,
session, id, server, actions, params, a monotonic deadline (in seconds), and
a cancelled Event. Params contains message and schema for a form, with
optional credentialFields identifying inputs to mask. It contains message
and url for a URL or MCP CreateMessageParams for sampling.

Return accept(content), decline(), cancel(), confirm_url() after explicit
confirmation, or sample(result, origin="model") with an MCP
CreateMessageResult. Handlers can run concurrently. Bound any blocking work
by the deadline and observe cancelled to exit promptly on withdrawal or
shutdown. Python cannot forcibly stop a running callback.

Session.host_input_errors contains sanitized error records.
Session.next_host_input_status(timeout=...) returns a HostInputStatus or
None if the wait ends without a status. Status delivery is best effort.
With a handler, request_timeout defaults to at least host_input_timeout
plus two seconds.
"""

from collections import deque
from contextlib import ExitStack
from dataclasses import dataclass, field
from pathlib import Path
import json
import math
import os
import queue
import re
import selectors
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
from typing import Any, Optional


MIN_REPL_VERSION = "@SESSION_DRIVER_MIN_REPL_VERSION@"
REPLY_MARGIN = 2
# Private to the bundled session driver and mcp-repl.
DRIVER_ENV = "MCP_REPL_INHERIT_STDIO_PROCESS_GROUP"
_HOST_FRAME_BYTES = 1024 * 1024
_HOST_MAX_HANDLERS = 32
_HOST_REPORT_LIMIT = 256


@dataclass(frozen=True)
class Reply:
    request_id: int
    value: Any


@dataclass(frozen=True)
class TaskRef:
    request_id: int
    task_id: str


@dataclass(frozen=True)
class HostInputRequest:
    kind: str
    session: str
    id: str
    server: dict = field(repr=False)
    actions: tuple
    params: dict = field(repr=False)
    deadline: float
    cancelled: threading.Event = field(default_factory=threading.Event, repr=False, compare=False)


@dataclass(frozen=True)
class HostInputStatus:
    session: Optional[str]
    id: Optional[str]
    status: str
    reason: Optional[str]


@dataclass(frozen=True)
class HostInputError:
    session: Optional[str]
    id: Optional[str]
    reason: str


@dataclass(frozen=True)
class _HostAnswer:
    action: str
    payload: dict = field(default_factory=dict, repr=False)


def accept(content):
    """Accept a form with a dictionary of field values."""
    return _HostAnswer("accept", {"content": content})


def decline():
    """Decline elicitation or refuse sampling."""
    return _HostAnswer("decline")


def cancel():
    """Cancel elicitation or refuse sampling."""
    return _HostAnswer("cancel")


def confirm_url():
    """Confirm a URL request after the user completes its out-of-band step."""
    return _HostAnswer("confirm_url")


def sample(result, *, origin="model"):
    """Provide an MCP CreateMessageResult from a model or operator."""
    return _HostAnswer("sample", {"origin": origin, "result": result})


class SessionError(Exception):
    def __init__(self, message, request_id=None, response=None):
        super().__init__(message)
        self.request_id = request_id
        self.response = response


class ClientError(SessionError):
    """mcp-repl returned a command error envelope."""


class ToolError(SessionError):
    """The MCP tool returned isError."""


class TaskError(SessionError):
    """The MCP task settled without a successful tool result."""


class TaskPending(SessionError):
    """The wait deadline elapsed. The task remains owned by this session."""


class SessionLost(SessionError):
    """The connection is unusable. A queued command may have taken effect."""

    def __init__(self, message, request_id=None, response=None, *, sent=False):
        super().__init__(message, request_id, response)
        self.sent = sent


class CleanupError(SessionError):
    """Orderly process cleanup could not be confirmed."""

    def __init__(self, message, *, cause=None):
        super().__init__(message)
        self.cause = cause


class _WireError(ValueError):
    pass


class _DeadlineExpired(_WireError):
    pass


def _has_visible(text):
    # Rust's char::is_whitespace, which the bridge uses, does not treat
    # U+001C to U+001F as whitespace, while str.isspace does.
    return any(not c.isspace() or c in "\x1c\x1d\x1e\x1f" for c in text)


def _require(condition, message):
    if not condition:
        raise _WireError(message)


def _invalid_json_constant(value):
    raise _WireError("invalid JSON constant: " + value)


def _host_response(request, answer):
    _require(
        isinstance(answer, _HostAnswer) and answer.action in request.actions,
        "invalid host input answer",
    )
    if answer.action == "accept":
        content = answer.payload.get("content")
        required = request.params.get("schema", {}).get("required", ())
        _require(
            isinstance(content, dict)
            and all(isinstance(key, str) for key in content)
            and all(key in content for key in required),
            "invalid form content",
        )
    elif answer.action == "sample":
        origin, result = answer.payload.get("origin"), answer.payload.get("result")
        _require(
            isinstance(result, dict) and result.get("role") == "assistant",
            "invalid sampling result",
        )
        model = result.get("model")
        _require(
            isinstance(model, str) and (
                (origin == "operator" and model == "mcp-repl/operator")
                or (origin == "model" and _has_visible(model)
                    and model not in ("mcp-repl/operator", "mcp-repl/canned"))
            ),
            "invalid sampling origin or model",
        )
    return _host_frame((request.session, request.id), answer.action, answer.payload)


def _host_frame(key, action, payload):
    session, request_id = key
    frame = {
        **payload,
        "v": 1, "type": "response", "session": session, "id": request_id, "action": action,
    }
    data = (
        json.dumps(frame, allow_nan=False, ensure_ascii=False, separators=(",", ":")) + "\n"
    ).encode("utf-8")
    _require(len(data) <= _HOST_FRAME_BYTES, "host input response exceeds the byte limit")
    return data


@dataclass
class _HostCall:
    request: HostInputRequest
    result: queue.Queue = field(default_factory=lambda: queue.Queue(maxsize=1), repr=False)
    response: Optional[bytes] = field(default=None, repr=False)

    def run(self, handler, slots):
        try:
            request = self.request
            if request.cancelled.is_set() or time.monotonic() >= request.deadline:
                return
            try:
                answer = handler(request)
            except BaseException:
                data, error = None, "handler_exception"
            else:
                if request.cancelled.is_set():
                    return
                try:
                    data, error = _host_response(request, answer), None
                except BaseException:
                    data, error = None, "invalid_answer"
            self.result.put_nowait((data, error))
        finally:
            slots.release()


class _HostInput:
    def __init__(self, handler):
        self._handler = handler
        self._slots = threading.BoundedSemaphore(_HOST_MAX_HANDLERS)
        self._reports = threading.Condition()
        self._statuses = deque(maxlen=_HOST_REPORT_LIMIT)
        self._errors = deque(maxlen=_HOST_REPORT_LIMIT)
        self._closing = False
        self._finished = False
        self._stop = threading.Event()
        self._thread = None
        # Only the bridge thread accesses question and response state.
        self._pending = {}
        self._declines = deque()
        self._sent = {}

    @property
    def errors(self):
        with self._reports:
            return tuple(self._errors)

    def next_status(self, timeout):
        with self._reports:
            self._reports.wait_for(lambda: self._statuses or self._finished, timeout)
            return self._statuses.popleft() if self._statuses else None

    def start(self, channel, child):
        thread = threading.Thread(
            target=self._run, args=(channel, child), name="mcp-repl host input", daemon=True,
        )
        thread.start()
        self._thread = thread

    def stop_handlers(self):
        with self._reports:
            self._closing = True

    def finish(self):
        if self._thread is not None:
            # Allow the reader to drain final status frames after the child exits.
            self._thread.join(timeout=0.1)
            self._stop.set()
            self._thread.join()
        with self._reports:
            self._finished = True
            self._reports.notify_all()

    def _refuse(self, key, reason=None):
        self._declines.append(key)
        if reason is not None:
            with self._reports:
                self._errors.append(HostInputError(*key, reason))

    def _status(self, frame, key):
        status, reason = frame.get("status"), frame.get("reason")
        if status not in ("accepted", "rejected"):
            return
        _require(
            all(value is None or isinstance(value, str) for value in (*key, reason)),
            "invalid host input status",
        )
        if self._sent.pop(key, False) and status == "rejected" and reason in (
            "malformed", "invalid_payload", "wrong_kind",
        ):
            # Retire the original attempt so the fallback is sent at most once.
            self._refuse(key, "answer_rejected")
        with self._reports:
            self._statuses.append(HostInputStatus(*key, status, reason))
            self._reports.notify_all()

    def _receive(self, frame):
        _require(
            isinstance(frame, dict) and type(frame.get("v")) is int and frame["v"] == 1,
            "invalid host input version",
        )
        kind = frame.get("type")
        key = (frame.get("session"), frame.get("id"))
        if kind == "response_status":
            self._status(frame, key)
            return
        if kind not in ("needs_input", "withdrawn"):
            return
        _require(
            all(isinstance(value, str) for value in key),
            "missing host input identifiers",
        )
        if kind == "withdrawn":
            self._sent.pop(key, None)
            call = self._pending.pop(key, None)
            if call is not None:
                call.request.cancelled.set()
                reason = "handler_timeout" if frame.get("reason") == "expired" else None
                self._refuse(key, reason)
            return
        _require(key not in self._pending and key not in self._sent, "duplicate host input id")
        actions, params, server = frame.get("actions"), frame.get("request"), frame.get("server")
        deadline_ms = frame.get("deadlineUnixMs")
        _require(
            frame.get("kind") in ("form", "url", "sampling")
            and isinstance(params, dict) and isinstance(server, dict)
            and isinstance(actions, list) and "decline" in actions
            and all(isinstance(action, str) for action in actions)
            and type(deadline_ms) is int and deadline_ms >= 0,
            "invalid host input question",
        )
        deadline = time.monotonic() + (deadline_ms / 1000 - time.time())
        with self._reports:
            if self._closing:
                self._refuse(key)
                return
        if time.monotonic() >= deadline:
            self._refuse(key, "handler_timeout")
            return
        if len(self._pending) >= _HOST_MAX_HANDLERS or not self._slots.acquire(blocking=False):
            self._refuse(key, "handler_busy")
            return
        request = HostInputRequest(frame["kind"], *key, server, tuple(actions), params, deadline)
        call = self._pending[key] = _HostCall(request)
        worker = threading.Thread(
            target=call.run, args=(self._handler, self._slots),
            name="mcp-repl host input handler", daemon=True,
        )
        try:
            worker.start()
        except RuntimeError:
            self._slots.release()
            del self._pending[key]
            self._refuse(key, "handler_busy")

    def _collect_responses(self):
        now = time.monotonic()
        for key, call in tuple(self._pending.items()):
            request = call.request
            if self._closing or now >= request.deadline:
                request.cancelled.set()
                del self._pending[key]
                self._refuse(key, None if self._closing else "handler_timeout")
            elif call.response is None:
                try:
                    data, error = call.result.get_nowait()
                except queue.Empty:
                    continue
                if error is None:
                    call.response = data
                else:
                    del self._pending[key]
                    self._refuse(key, error)

    def _send(self, channel, partial):
        with self._reports:
            # Check every deadline even if the socket cannot take more bytes.
            self._collect_responses()
            try:
                if partial:
                    partial = partial[channel.send(partial):]
                    if partial:
                        return partial
                while self._declines:
                    data = _host_frame(self._declines[0], "decline", {})
                    sent = channel.send(data)
                    self._declines.popleft()
                    if sent < len(data):
                        return memoryview(data)[sent:]
                for key, call in tuple(self._pending.items()):
                    if call.response is None:
                        continue
                    data = call.response
                    sent = channel.send(data)
                    del self._pending[key]
                    # Acknowledgements are best effort. Retain only recent identifiers.
                    if len(self._sent) == _HOST_REPORT_LIMIT:
                        del self._sent[next(iter(self._sent))]
                    self._sent[key] = True
                    if sent < len(data):
                        return memoryview(data)[sent:]
            except BlockingIOError:
                pass
            return partial

    def _run(self, channel, child):
        partial = b""
        buffer = bytearray()
        try:
            with channel, selectors.DefaultSelector() as selector:
                channel.setblocking(False)
                selector.register(channel, selectors.EVENT_READ)
                while not self._stop.is_set():
                    # Poll for handler completions and a child exit even with an idle socket.
                    delay = 0.01 if self._pending or self._declines or partial else 0.05
                    if len(self._declines) >= _HOST_MAX_HANDLERS:
                        # Apply backpressure if the peer sends questions faster than it reads.
                        self._stop.wait(delay)
                        if child.poll() is not None:
                            return
                    elif selector.select(delay):
                        try:
                            chunk = channel.recv(65536)
                        except BlockingIOError:
                            continue
                        if not chunk:
                            _require(not buffer, "truncated host input frame")
                            return
                        buffer.extend(chunk)
                        while True:
                            end = buffer.find(b"\n")
                            if end < 0:
                                _require(len(buffer) < _HOST_FRAME_BYTES, "oversized host input frame")
                                break
                            _require(end < _HOST_FRAME_BYTES, "oversized host input frame")
                            frame = json.loads(
                                buffer[:end].decode("utf-8"), parse_constant=_invalid_json_constant,
                            )
                            del buffer[:end + 1]
                            self._receive(frame)
                    elif child.poll() is not None:
                        return
                    partial = self._send(channel, partial)
        except (OSError, ValueError, RecursionError, OverflowError):
            with self._reports:
                self._errors.append(HostInputError(None, None, "bridge_error"))
        finally:
            for call in self._pending.values():
                call.request.cancelled.set()
            self._pending.clear()
            self._declines.clear()
            self._sent.clear()
            with self._reports:
                self._finished = True
                self._reports.notify_all()


def _word(value):
    if not isinstance(value, str) or not value or any(c in value for c in "$\r\n\0"):
        raise ValueError("expected a nonempty REPL token without $, CR, LF or NUL")
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def _call_line(name, args):
    if not isinstance(args, dict):
        raise TypeError("tool arguments must be a JSON object")
    # REPL variable substitution also scans JSON strings, before JSON decoding.
    body = json.dumps(args, allow_nan=False, ensure_ascii=True, separators=(",", ":"))
    return "builtin call " + _word(name) + " " + body.replace("$", r"\u0024")


def _executable(program, env):
    path = shutil.which(program, path=env.get("PATH", os.defpath))
    if path is None:
        raise FileNotFoundError(os.fspath(program))
    if os.name == "nt" and Path(path).suffix.lower() in (".bat", ".cmd"):
        raise ValueError("use an executable instead of a Windows batch file")
    return os.path.abspath(path)


def _version_key(version):
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)(?:-([\w.-]+))?(?:\+[\w.-]+)?", version)
    if match is None:
        return None
    major, minor, patch, prerelease = match.groups()
    identifiers = () if prerelease is None else tuple(
        (0, int(part)) if part.isdecimal() else (1, part) for part in prerelease.split(".")
    )
    return int(major), int(minor), int(patch), prerelease is None, identifiers


def _check_version(repl, env, deadline):
    minimum = MIN_REPL_VERSION
    if minimum.startswith("@"):
        return
    try:
        version = subprocess.run(
            [repl, "--version"], env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            encoding="ascii", errors="replace",
            timeout=max(0, deadline - time.monotonic()), check=True,
        ).stdout.strip()
    except (OSError, subprocess.SubprocessError) as error:
        raise SessionError("could not check mcp-repl --version; need {}".format(minimum)) from error
    prefix = "mcp-repl "
    installed = _version_key(version[len(prefix):]) if version.startswith(prefix) else None
    if installed is None:
        raise SessionError("unrecognized mcp-repl --version output; need {}".format(minimum))
    if installed < _version_key(minimum):
        raise SessionError("need mcp-repl {} or newer; found {}".format(minimum, version))


def _unknown_task(error, task_id):
    message = str(error).lower()
    status = error.response["exitStatus"]
    if status == 1:
        return message.startswith("no task `last` in this session")
    if status != 3:
        return False
    message = message.split(" (code ", 1)[0].strip()
    missing = ("task not found", "unknown task", "unknown task id", "task expired")
    return message in missing or any(
        message == reason + ": " + task_id.lower() for reason in missing
    )


def _tools(reply):
    value = reply.value
    _require(
        isinstance(value, list)
        and all(isinstance(tool, dict) and isinstance(tool.get("name"), str) for tool in value),
        "expected a tools array",
    )
    return reply


def _tool_result(reply):
    value = reply.value
    _require(
        isinstance(value, dict)
        and isinstance(value.get("content"), list)
        and isinstance(value.get("isError", False), bool),
        "expected an MCP tool result",
    )
    if value.get("isError"):
        raise ToolError("MCP tool returned isError", reply.request_id, value)
    return reply


def _resource(reply):
    value = reply.value
    _require(
        isinstance(value, dict) and isinstance(value.get("contents"), list),
        "expected an MCP resource result",
    )
    return reply


class Session:
    """Synchronous context manager for one explicitly owned stdio server.

    Successful calls return checked MCP wrappers in Reply.value. Interpret
    application results according to the server's contract before acting on
    them. TaskRef objects are valid only in the session that issued them.
    """

    def __init__(
        self,
        server_argv,
        *,
        repl="mcp-repl",
        protocol="stable",
        request_timeout=None,
        startup_timeout=35,
        shutdown_timeout=20,
        max_frame_bytes=8 * 1024 * 1024,
        stderr_bytes=16 * 1024,
        env=None,
        host_input=None,
        host_input_timeout=30,
    ):
        if host_input is not None and os.name != "posix":
            raise ValueError("host_input requires Unix")
        if host_input is not None and not callable(host_input):
            raise TypeError("host_input must be callable")
        if type(host_input_timeout) is not int or not 1 <= host_input_timeout <= 120:
            raise ValueError("host_input_timeout must be an integer from 1 to 120 seconds")
        if request_timeout is None:
            request_timeout = 10
            if host_input is not None:
                request_timeout = max(request_timeout, host_input_timeout + REPLY_MARGIN)
        if isinstance(server_argv, (str, bytes)):
            raise TypeError("pass the server command as an argv sequence")
        self._server_argv = tuple(os.fsdecode(arg) for arg in server_argv)
        if not self._server_argv or not self._server_argv[0]:
            raise ValueError("a stdio server executable is required")
        if protocol not in ("stable", "final", "2026-07-28"):
            raise ValueError("pin a protocol to avoid respawning the stdio server")
        timeouts = (request_timeout, startup_timeout, shutdown_timeout)
        if any(not math.isfinite(t) or t <= 0 for t in timeouts):
            raise ValueError("timeouts must be finite and positive")
        if max_frame_bytes <= 0 or stderr_bytes <= 0:
            raise ValueError("pipe limits must be positive")
        self._repl = os.fsdecode(repl)
        self._protocol = protocol
        self._env = dict(os.environ if env is None else env)
        self._request_timeout = math.ceil(request_timeout)
        self._startup_timeout = startup_timeout
        self._shutdown_timeout = shutdown_timeout
        self._max_frame = max_frame_bytes
        self._stderr_limit = stderr_bytes
        self._host_input = _HostInput(host_input) if host_input is not None else None
        self._host_input_timeout = host_input_timeout

        self._operation = threading.RLock()
        self._state = threading.Condition()
        self._writes = queue.Queue()
        self._workers = []
        self._child = None
        self._stopping = False
        self._closed = False
        self._reaped = False
        self._quit_acknowledged = False
        self._expecting = False
        self._frame = None
        self._fault = None
        self._write_fault = None
        self._eof = False
        self._stderr = bytearray()
        self._next_id = 1
        self._task = None
        self.cleanup_error = None
        self._cleanup_reported = False

    @property
    def stderr_tail(self):
        with self._state:
            return self._stderr.decode("utf-8", errors="replace")

    @property
    def host_input_errors(self):
        """The most recent 256 host input errors, in observation order."""
        return self._host_input.errors if self._host_input is not None else ()

    def next_host_input_status(self, timeout=None):
        """Read a best-effort status, or return None at timeout or bridge EOF."""
        if self._host_input is None:
            return None
        return self._host_input.next_status(timeout)

    @property
    def returncode(self):
        return self._child.poll() if self._child is not None else None

    def __enter__(self):
        with self._operation:
            if self._closed or self._child is not None:
                raise RuntimeError("each Session can be entered only once")
            deadline = time.monotonic() + self._startup_timeout
            repl = _executable(self._repl, self._env)
            server = _executable(self._server_argv[0], self._env)
            prefix, separator, _ = server.rpartition(":")
            if len(self._server_argv) == 1 and separator and (
                Path(prefix).suffix.lower() == ".json" or Path(prefix).exists()
            ):
                raise ValueError("the server executable path resembles a config import")
            try:
                _check_version(repl, self._env, deadline)
                env = self._env.copy()
                env[DRIVER_ENV] = "1"
                # An ambient profile can shadow a single positional program name.
                with tempfile.TemporaryDirectory(prefix="mcp-repl-session-") as directory:
                    config = Path(directory) / "config.toml"
                    config.write_text("", encoding="utf-8")
                    self._spawn(repl, config, server, env)
                    self._expecting = True
                    for target, pipe in (
                        (self._write_stdin, self._child.stdin),
                        (self._read_stdout, self._child.stdout),
                        (self._read_stderr, self._child.stderr),
                    ):
                        worker = threading.Thread(target=target, args=(pipe,), daemon=True)
                        worker.start()
                        self._workers.append(worker)
                    self._request("builtin tools", _tools, deadline=deadline, starting=True)
            except KeyboardInterrupt:
                self._stop(interrupt=True)
                raise
            except BaseException:
                self._stop()
                raise
            return self

    def _spawn(self, repl, config, server, env):
        argv = [
            repl, "--config", str(config), "--json", "--no-history",
            "--color", "never", "--no-reconnect", "--protocol", self._protocol,
            "--timeout", str(self._request_timeout),
        ]
        server_args = ["--", server, *self._server_argv[1:]]
        options = dict(
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env=env, shell=False, start_new_session=os.name == "posix",
            creationflags=getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0),
        )
        if self._host_input is None:
            argv.extend(["--sampling", "decline", "--elicitation", "decline"])
            self._child = subprocess.Popen(argv + server_args, **options)
            return
        host, repl_end = socket.socketpair()
        try:
            with ExitStack() as endpoints:
                endpoints.enter_context(repl_end)
                fd = repl_end.fileno()
                # Unix can assign a socket to a standard stream closed by the caller.
                while fd < 3:
                    fd = os.dup(fd)
                    endpoints.callback(os.close, fd)
                argv.extend([
                    "--host-input-fd", str(fd), "--host-input-timeout",
                    str(self._host_input_timeout),
                ])
                self._child = subprocess.Popen(argv + server_args, pass_fds=(fd,), **options)
            self._host_input.start(host, self._child)
        except BaseException:
            host.close()
            raise

    def __exit__(self, exc_type, exc, traceback):
        if exc is None:
            self.close()
        else:
            self._close()

    def discover(self):
        return self._request("builtin tools", _tools)

    def call(self, name, args):
        return self._request(_call_line(name, args), _tool_result)

    def read(self, uri):
        word = _word(uri)
        if uri.startswith("--"):
            raise ValueError("the resource URI resembles a REPL option")
        return self._request("builtin read " + word, _resource)

    def start_task(self, name, args):
        """Start one MCP task. Settle or abandon it before starting another."""
        with self._operation:
            if self._task is not None:
                raise ValueError("settle or abandon the outstanding MCP task first")

            def created(reply):
                value = reply.value
                task = value.get("task") if isinstance(value, dict) else None
                task_id = task.get("taskId") if isinstance(task, dict) else None
                _require(isinstance(task_id, str) and bool(task_id), "expected an MCP task ID")
                self._task = TaskRef(reply.request_id, task_id)
                return self._task

            return self._request(_call_line(name, args) + " &", created)

    def wait_task(self, ref, *, timeout):
        """Wait for terminal settlement and check the task's tool result."""
        if not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("the wait timeout must be finite and positive")
        seconds = math.ceil(timeout)
        return self._task_request(ref, "builtin wait last --timeout " + str(seconds), seconds)

    def cancel_task(self, ref):
        """Request cancellation of the owned task and return its reported state."""
        return self._task_request(ref, "builtin cancel last")

    def abandon_task(self, ref):
        """Release the local reference without contacting the server."""
        with self._operation:
            self._check_task_ref(ref)
            self._task = None

    def _check_task_ref(self, ref):
        if self._closed or not isinstance(ref, TaskRef) or ref is not self._task:
            raise ValueError("expected the outstanding MCP task from this session")

    def _task_request(self, ref, command, wait_seconds=None):
        with self._operation:
            self._check_task_ref(ref)

            def task_reply(reply):
                task = reply.value
                _require(
                    isinstance(task, dict) and task.get("taskId") == ref.task_id,
                    "command returned a different MCP task",
                )
                status = task.get("status")
                terminal = ("completed", "failed", "cancelled")
                if wait_seconds is None:
                    _require(status in terminal + ("working", "input_required"), "invalid task status")
                    if status in terminal:
                        self._task = None
                    return reply
                _require(status in terminal, "task has not settled")
                self._task = None
                if status != "completed" or task.get("error") is not None or task.get("result") is None:
                    raise TaskError("MCP task did not produce a successful result", reply.request_id, task)
                try:
                    _tool_result(Reply(reply.request_id, task["result"]))
                except ToolError as error:
                    raise TaskError(str(error), reply.request_id, task) from error
                return reply

            def task_error(error):
                if _unknown_task(error, ref.task_id):
                    self._task = None
                if wait_seconds is not None and error.response["exitStatus"] == 4:
                    expected = "task {} was still running after {}s (--timeout)".format(
                        ref.task_id, wait_seconds,
                    )
                    if str(error) in (expected, "Transport error: " + expected):
                        raise TaskPending(str(error), error.request_id, error.response)

            return self._request(command, task_reply, timeout=wait_seconds, on_error=task_error)

    def _pipe_fault(self, message):
        with self._state:
            if not self._stopping and self._fault is None:
                self._fault = message
            self._state.notify_all()

    def _write_stdin(self, pipe):
        try:
            with pipe:
                for data in iter(self._writes.get, None):
                    pipe.write(data)
                    pipe.flush()
        except OSError as error:
            with self._state:
                self._write_fault = "stdin write failed: " + str(error)
                self._state.notify_all()

    def _read_stdout(self, pipe):
        with pipe:
            try:
                for line in iter(lambda: pipe.readline(self._max_frame + 1), b""):
                    with self._state:
                        if not self._stopping and self._fault is None:
                            if len(line) > self._max_frame:
                                self._fault = "stdout frame exceeds the byte limit"
                            elif not line.endswith(b"\n"):
                                self._fault = "stdout ended in a partial frame"
                            elif not self._expecting or self._frame is not None:
                                self._fault = "unsolicited stdout frame"
                            else:
                                self._frame = line
                            self._state.notify_all()
                        discard = self._stopping or self._fault is not None
                    if discard:
                        # Keep draining so a faulty peer can still exit.
                        while pipe.read1(8192):
                            pass
                        break
            except OSError as error:
                self._pipe_fault("stdout read failed: " + str(error))
            finally:
                with self._state:
                    self._eof = True
                    self._state.notify_all()

    def _read_stderr(self, pipe):
        with pipe:
            try:
                for chunk in iter(lambda: pipe.read1(8192), b""):
                    with self._state:
                        self._stderr.extend(chunk)
                        del self._stderr[:-self._stderr_limit]
            except OSError as error:
                self._pipe_fault("stderr read failed: " + str(error))

    def _exchange(self, data, deadline, *, starting, quitting):
        with self._state:
            _require(self._fault is None, self._fault)
            live = not self._eof and self.returncode is None
            _require(live or starting, "mcp-repl exited before the write")
            self._expecting = True
            sent = False
            try:
                if time.monotonic() >= deadline:
                    raise _DeadlineExpired("command deadline expired")
                if live and self._frame is None:
                    self._writes.put(data)
                    sent = True
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise _DeadlineExpired("command deadline expired")
                    _require(self._fault is None, self._fault)
                    if self._frame is not None:
                        return self._frame, sent
                    _require(not self._eof, "stdout closed before a response")
                    if not (starting or quitting):
                        _require(self._write_fault is None, self._write_fault)
                        _require(self.returncode is None, "mcp-repl exited before a response")
                    self._state.wait(min(remaining, 0.05))
            except _WireError as error:
                error.sent = sent
                raise
            finally:
                self._frame = None
                self._expecting = False

    def _request(
        self, command, validate, *, deadline=None, timeout=None, starting=False,
        quitting=False, on_error=None,
    ):
        with self._operation:
            if self._closed or self._child is None:
                raise SessionLost("session is closed")
            data = (command + "\n").encode("utf-8")
            if len(data) > self._max_frame:
                raise ValueError("command exceeds the byte limit")
            request_id = self._next_id
            self._next_id += 1
            if deadline is None:
                seconds = self._request_timeout if timeout is None else timeout
                deadline = time.monotonic() + seconds + REPLY_MARGIN
            sent = False
            try:
                frame, sent = self._exchange(data, deadline, starting=starting, quitting=quitting)
                try:
                    value = json.loads(frame.decode("utf-8"), parse_constant=_invalid_json_constant)
                except (json.JSONDecodeError, UnicodeDecodeError) as error:
                    if starting:
                        raise _WireError(
                            "mcp-repl startup stdout is not JSON; use a build with piped --json support"
                        ) from error
                    raise
                if isinstance(value, dict) and {"error", "kind", "exitStatus"} <= value.keys():
                    _require(
                        isinstance(value["error"], str)
                        and isinstance(value["kind"], str)
                        and type(value["exitStatus"]) is int
                        and 1 <= value["exitStatus"] <= 6,
                        "invalid client error envelope",
                    )
                    failure = ClientError(value["error"], request_id, value)
                    if on_error is not None:
                        on_error(failure)
                    if value["exitStatus"] in (4, 6):
                        self._stop()
                        raise SessionLost(value["error"], request_id, value, sent=sent)
                    raise failure
                return validate(Reply(request_id, value))
            except (ValueError, RecursionError) as error:
                sent = sent or getattr(error, "sent", False)
                self._stop(interrupt=sent and isinstance(error, _DeadlineExpired))
                raise SessionLost(str(error), request_id, sent=sent) from error
            except KeyboardInterrupt:
                self._stop(interrupt=True)
                raise

    def _interrupt_child(self):
        if self.returncode is not None:
            return
        try:
            if os.name == "posix":
                os.kill(self._child.pid, signal.SIGINT)
            elif os.name == "nt":
                self._child.send_signal(signal.CTRL_BREAK_EVENT)
        except OSError:
            # A Windows runner may have no console attached. EOF still works.
            pass

    def _stop(self, *, interrupt=False):
        if self._host_input is not None:
            self._host_input.stop_handlers()
        try:
            self._stop_child(interrupt=interrupt)
        finally:
            if self._host_input is not None:
                self._host_input.finish()

    def _stop_child(self, *, interrupt):
        if self._reaped:
            return
        if not self._closed:
            self._closed = True
            self._task = None
            if self._child is not None:
                with self._state:
                    self._stopping = True
                try:
                    if interrupt:
                        self._interrupt_child()
                finally:
                    self._writes.put(None)
        if self._child is None:
            self._reaped = True
            return
        try:
            status = self._child.wait(timeout=self._shutdown_timeout)
        except subprocess.TimeoutExpired:
            self.cleanup_error = CleanupError("forced mcp-repl shutdown; server cleanup unverified")
            if os.name == "posix":
                try:
                    os.killpg(self._child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            else:
                self._child.kill()
            try:
                status = self._child.wait(timeout=1)
            except subprocess.TimeoutExpired:
                self.cleanup_error = CleanupError("mcp-repl did not exit after kill")
                return
        self._reaped = True
        if self.cleanup_error is None and status != 0:
            self.cleanup_error = CleanupError("mcp-repl exited with status {}".format(status))
        deadline = time.monotonic() + 0.25
        for worker in self._workers:
            worker.join(timeout=max(0, deadline - time.monotonic()))
        if not self._quit_acknowledged and self.cleanup_error is None and any(
            worker.is_alive() for worker in self._workers
        ):
            self.cleanup_error = CleanupError("mcp-repl pipes did not close")

    def _close(self):
        if self._host_input is not None:
            # Release input waits before acquiring the lock held by a live command.
            self._host_input.stop_handlers()
        with self._operation:
            def quit_reply(reply):
                value = reply.value
                _require(
                    isinstance(value, dict) and len(value) == 1 and value.get("exit") is True,
                    "expected the quit acknowledgement",
                )
                self._quit_acknowledged = True

            quit_error = self.cleanup_error.cause if self.cleanup_error is not None else None
            try:
                if not self._closed and self._child is not None:
                    self._request("builtin quit", quit_reply, quitting=True)
            except SessionError as error:
                quit_error = error
            finally:
                try:
                    self._stop()
                finally:
                    if quit_error is not None:
                        if self.cleanup_error is None:
                            self.cleanup_error = CleanupError(str(quit_error), cause=quit_error)
                        else:
                            self.cleanup_error.cause = quit_error

    def close(self):
        """Close the owned connection and report a cleanup failure once."""
        self._close()
        with self._operation:
            if self.cleanup_error is not None and not self._cleanup_reported:
                self._cleanup_reported = True
                raise self.cleanup_error from self.cleanup_error.cause
