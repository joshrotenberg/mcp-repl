#!/usr/bin/env python3
"""Exercise reedline through a real POSIX PTY, with bounded owned cleanup."""

import argparse
import errno
import fcntl
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time

ANSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07|\x1b\\)")


class Terminal:
    def __init__(self, binary, config, json_output):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        env = os.environ.copy()
        env.pop("MCP_BEARER", None)
        env.pop("RUST_LOG", None)
        env.update(TERM="xterm-256color", NO_COLOR="1")
        args = [str(binary), "--demo", "--protocol", "final", "--no-history",
                "--color", "never", "--config", str(config)]
        if json_output:
            args.append("--json")
        self.process = subprocess.Popen(args, stdin=slave, stdout=slave, stderr=slave,
                                        env=env, start_new_session=True)
        os.close(slave)
        self.output = bytearray()
        self.answered_until = 0

    def send(self, value):
        os.write(self.master, value)

    def wait(self, text, start=0, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            plain = ANSI.sub(b"", bytes(self.output[start:])).decode(errors="replace").replace("\r", "")
            if text in plain:
                return len(self.output)
            if not select.select([self.master], [], [], min(0.1, max(0, deadline-time.monotonic())))[0]:
                continue
            try:
                chunk = os.read(self.master, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                break
            if not chunk:
                break
            self.output.extend(chunk)
            # reedline queries the cursor before painting. Reply to every
            # fresh query, including one split across read boundaries.
            raw = bytes(self.output)
            while True:
                index = raw.find(b"\x1b[6n", self.answered_until)
                if index < 0:
                    break
                self.send(b"\x1b[1;1R")
                self.answered_until = index + 4
        raise AssertionError(f"terminal did not show {text!r}:\n"
                             + ANSI.sub(b"", bytes(self.output)).decode(errors="replace"))

    def close(self):
        if self.process.poll() is None:
            mark = len(self.output)
            self.send(b"\x15quit")
            try:
                self.wait("mcp-repl-demo> quit", start=mark, timeout=3)
            except AssertionError:
                pass
            self.send(b"\r")
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                print(ANSI.sub(b"", bytes(self.output)).decode(errors="replace"))
                # This process owns its own session; never signal other work.
                os.killpg(self.process.pid, signal.SIGTERM)
                self.process.wait(timeout=5)
        os.close(self.master)
        assert self.process.returncode == 0, self.process.returncode


def exercise(binary):
    with tempfile.TemporaryDirectory(prefix="mcp-repl-terminal-") as directory:
        config = Path(directory) / "config.toml"
        config.write_text("")
        terminal = Terminal(binary, config, True)
        try:
            terminal.wait("mcp-repl-demo>")
            mark = len(terminal.output)
            terminal.send(b"ec")
            terminal.wait("mcp-repl-demo> ec", start=mark)
            terminal.send(b"\t")
            terminal.wait("ECHO", start=mark)
            terminal.send(b"\r")
            terminal.wait("mcp-repl-demo> echo", start=mark)
            terminal.send(b" message=PTY_COMPLETION\r")
            terminal.wait('"text":"PTY_COMPLETION"')
            mark = len(terminal.output)
            terminal.send(b"echo message=PTY_CANCELLED")
            terminal.send(b"\x03")
            terminal.wait("mcp-repl-demo>", start=mark)
            terminal.send(b"echo message=PTY_AFTER_CANCEL\r")
            terminal.wait('"text":"PTY_AFTER_CANCEL"')
            assert b'"text":"PTY_CANCELLED"' not in terminal.output
        finally:
            terminal.close()

        terminal = Terminal(binary, config, False)
        try:
            terminal.wait("mcp-repl-demo>")
            mark = len(terminal.output)
            terminal.send(b"slow_add a=20 b=22 &\r")
            terminal.wait("mcp-repl-demo>", start=mark)
            terminal.send(b"echo message=PTY_AFTER_EVENT")
            terminal.wait("completed", start=mark)
            # An asynchronous printer redraw must preserve the typed line.
            mark = len(terminal.output)
            terminal.send(b"\r")
            terminal.wait("\nPTY_AFTER_EVENT\n", start=mark)
        finally:
            terminal.close()
    print("PTY completion, Ctrl-C and asynchronous prompt redraw passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    options = parser.parse_args()
    exercise(options.binary.resolve())
