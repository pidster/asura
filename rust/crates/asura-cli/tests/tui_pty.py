#!/usr/bin/env python3
"""Bounded production TUI journeys using only an isolated CLI fixture executable.

Usage: python3 tui_pty.py target/debug/deps/lifecycle-HASH [--conversation|--setup|--queue-reorder]
This verifies process and tty state, not native terminal visual correctness.
"""
import codecs
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unicodedata

DEADLINE = time.monotonic() + 52
CLEANUP = False
CLEANUP_DEADLINE = None
# A supervisor retains the controlling tty after the application exits on macOS.
SUPERVISOR = r'''
import fcntl, os, signal, struct, subprocess, sys, termios
before = termios.tcgetattr(0)
# Darwin sets immutable FWASWRITTEN on first tty write. Normalize that kernel
# observation before capturing flags; still compare every flag exactly.
os.write(1, b'__ASURA_TTY_PROBE__\n')
before_flags = [fcntl.fcntl(fd, fcntl.F_GETFL) for fd in (0, 1)]
child = subprocess.Popen(sys.argv[1:])
def stop_child(signum, frame):
    os.kill(child.pid, signal.SIGSTOP)
    observed, status = os.waitpid(child.pid, os.WUNTRACED)
    assert observed == child.pid and os.WIFSTOPPED(status)
    print('__ASURA_CHILD_STOPPED__', flush=True)
signal.signal(signal.SIGUSR1, stop_child)
print('__ASURA_TEST_PID__' + str(child.pid), flush=True)
code = child.wait()
after = termios.tcgetattr(0)
if sys.platform == 'darwin' and before != after:
    equal = all(a == b for i, (a, b) in enumerate(zip(before, after)) if i != 3)
    if equal and (before[3] ^ after[3]) == termios.PENDIN:
        fcntl.ioctl(0, termios.FIONREAD, struct.pack('I', 0))
        after = termios.tcgetattr(0)
restored = before == after
after_flags = [fcntl.fcntl(fd, fcntl.F_GETFL) for fd in (0, 1)]
flags_restored = before_flags == after_flags
for fd, flags in zip((0, 1), before_flags):
    fcntl.fcntl(fd, fcntl.F_SETFL, flags)
termios.tcsetattr(0, termios.TCSANOW, before)
print('__ASURA_TTY_RESTORED__' + str(restored), flush=True)
print('__ASURA_FLAGS_RESTORED__' + str(flags_restored), flush=True)
print('__ASURA_FLAGS_VALUES__' + repr((before_flags, after_flags)), flush=True)
print('__ASURA_CHILD_EXIT__' + str(code), flush=True)
sys.exit(code if restored and flags_restored else 97)
'''


def remaining(limit=8):
    deadline = CLEANUP_DEADLINE if CLEANUP else DEADLINE
    value = min(limit, deadline - time.monotonic())
    if value <= 0:
        raise AssertionError("TUI cleanup deadline exceeded" if CLEANUP else "TUI work deadline exceeded")
    return value


class Screen:
    """Minimal cursor-addressed screen for this backend's PTY text assertions.

    Handles CSI cursor/erase operations and UTF-8 incrementally. Styling and
    terminal emulation beyond these fixture outputs are deliberately ignored.
    """
    def __init__(self):
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.pending = ""
        self.cells = {}
        self.row = self.column = 0

    def feed(self, data):
        self.pending += self.decoder.decode(data)
        while self.pending:
            if self.pending.startswith("\x1b["):
                match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", self.pending)
                if match is None:
                    return
                arguments, _, action = match.groups()
                values = [int(value) if value else 0 for value in arguments.split(";")] if not arguments.startswith("?") else []
                first = values[0] if values else 0
                if action in ("H", "f"):
                    self.row = max(1, first) - 1
                    self.column = max(1, values[1] if len(values) > 1 else 1) - 1
                elif action == "G":
                    self.column = max(1, first) - 1
                elif action == "A":
                    self.row = max(0, self.row - max(1, first))
                elif action == "B":
                    self.row += max(1, first)
                elif action == "C":
                    self.column += max(1, first)
                elif action == "D":
                    self.column = max(0, self.column - max(1, first))
                elif action == "J" and first in (2, 3):
                    self.cells.clear()
                elif action == "K":
                    self.cells = {(row, col): char for (row, col), char in self.cells.items()
                        if row != self.row or (first == 0 and col < self.column)
                        or (first == 1 and col > self.column)}
                self.pending = self.pending[match.end():]
                continue
            if self.pending == "\x1b":
                return
            char, self.pending = self.pending[0], self.pending[1:]
            if char == "\r":
                self.column = 0
            elif char == "\n":
                self.row += 1
            elif char >= " " and char != "\x7f":
                self.cells[self.row, self.column] = char
                self.column += 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1

    def text(self):
        rows = {}
        for (row, col), char in self.cells.items():
            rows.setdefault(row, {})[col] = char
        return "\n".join("".join(cells.get(col, " ") for col in range(max(cells) + 1))
                         for _, cells in sorted(rows.items()))


class Trial:
    def __init__(self, binary, cwd=None):
        self.master, self.slave = pty.openpty()
        self.output = bytearray()
        self.screen = Screen()
        self.size = (0, 0)
        self.resize(80, 24)

        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)

        self.process = subprocess.Popen(
            [sys.executable, "-c", SUPERVISOR, str(binary)],
            stdin=self.slave, stdout=self.slave, stderr=self.slave,
            close_fds=True, preexec_fn=controlling_terminal, cwd=cwd or binary.parent,
            env={**os.environ, "TERM": "xterm-256color"},
        )
        try:
            self.until(b"__ASURA_TEST_PID__")
            self.until(b"\x1b[?2004h")
        except Exception:
            self.close()
            raise

    def read(self, timeout):
        if select.select([self.master], [], [], timeout)[0]:
            try:
                data = os.read(self.master, 65536)
                self.output.extend(data)
                self.screen.feed(data)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise

    def until(self, text, offset=0, plain=False, refresh=False, limit=8, reject=None):
        end = time.monotonic() + remaining(limit)
        next_refresh = 0
        while True:
            data = bytes(self.output[offset:])
            if (text.decode() in self.screen.text() and len(self.output) > offset) if plain else text in data:
                return
            if reject and reject in self.screen.text():
                notice = next(line.strip() for line in self.screen.text().splitlines() if reject in line)
                raise AssertionError(f"terminal failure while waiting for {text!r}: {notice[:256]}")
            assert time.monotonic() < end, (text, bytes(self.output[-2500:]))
            if refresh and time.monotonic() >= next_refresh:
                width = 82 if self.size[0] != 82 else 80
                self.resize(width, self.size[1])
                next_refresh = time.monotonic() + 0.3
            self.read(0.025)
            if self.process.poll() is not None:
                data = bytes(self.output[offset:])
                assert (text.decode() in self.screen.text() if plain else text in data), bytes(self.output[-2500:])
                return

    def send(self, data):
        os.write(self.master, data)

    def resize(self, columns, rows):
        self.size = (columns, rows)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ,
                    struct.pack("HHHH", rows, columns, 0, 0))

    def repaint(self, columns=82, rows=25):
        offset = len(self.output)
        self.resize(columns, rows)
        return offset

    def finish(self, limit=8):
        end = time.monotonic() + remaining(limit)
        while self.process.poll() is None and time.monotonic() < end:
            self.read(0.025)
        self.read(0)
        assert self.process.poll() == 0, bytes(self.output[-2500:])
        assert b"__ASURA_TTY_RESTORED__True" in self.output
        assert b"__ASURA_FLAGS_RESTORED__True" in self.output
        for marker in (b"\x1b[?1049l", b"\x1b[?2004l", b"\x1b[?25h"):
            assert marker in self.output, ("cleanup absent", marker)

    def finish_killed(self):
        end = time.monotonic() + remaining()
        while self.process.poll() is None and time.monotonic() < end:
            self.read(0.025)
        self.read(0)
        assert self.process.poll() is not None, "supervisor did not reap killed TUI"
        assert b"__ASURA_CHILD_EXIT__-9" in self.output, bytes(self.output[-1000:])
        # SIGKILL cannot run terminal cleanup. The supervisor restores its tty;
        # this case asserts backend lifetime only, not application restoration.

    def signal(self, sig):
        match = re.search(rb"__ASURA_TEST_PID__(\d+)", self.output)
        assert match and self.process.poll() is None
        # Child PID belongs to our retained supervisor and cannot be reused until
        # that supervisor reaps it; test acts only while its application is live.
        os.kill(int(match.group(1)), sig)

    def idle(self, seconds):
        end = time.monotonic() + min(seconds, remaining(seconds))
        while time.monotonic() < end:
            self.read(min(0.025, end - time.monotonic()))
            assert self.process.poll() is None, bytes(self.output[-2500:])

    def stop_child(self):
        offset = len(self.output)
        os.kill(self.process.pid, signal.SIGUSR1)
        self.until(b"__ASURA_CHILD_STOPPED__", offset)

    def close(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGKILL)
        os.close(self.master)
        os.close(self.slave)
        self.process.wait(timeout=5)


class Fixture:
    def __init__(self, source):
        self.root = Path(tempfile.mkdtemp(prefix="asura-tui-", dir="/private/tmp"))
        self.root.chmod(0o700)
        (self.root / "home").mkdir(mode=0o700)
        self.binary = self.root / "asura-fixture"
        shutil.copyfile(source, self.binary)
        self.binary.chmod(0o700)
        self.counter = 0
        self.started = False
        self.uncertain_start = False
        self.active_trials = []

    def command(self, *args):
        self.counter += 1
        out = self.root / f"command-{self.counter}.out"
        err = self.root / f"command-{self.counter}.err"
        with out.open("wb") as stdout, err.open("wb") as stderr:
            child = subprocess.Popen([str(self.binary), *args], cwd=self.root,
                stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
            )
            try:
                end = time.monotonic() + remaining()
                while child.poll() is None:
                    assert time.monotonic() < end, ("fixture CLI deadline", args)
                    for trial in self.active_trials:
                        if trial.process.poll() is None:
                            trial.read(0)
                    time.sleep(0.01)
            finally:
                if child.poll() is None:
                    child.kill()
                child.wait(timeout=2)
        return child.returncode, out.read_bytes(), err.read_bytes()

    def launch(self, keep_project_prompt=False, cwd=None):
        # The TUI may spawn before it renders; retain ownership uncertainty until
        # authenticated CLI status proves a current service after the launch.
        self.started = True
        self.uncertain_start = True
        trial = Trial(self.binary, cwd=cwd)
        self.active_trials.append(trial)
        try:
            trial.until(b"Connected (", plain=True, refresh=True)
            result = self.command("service", "status", "--json")
            assert result[0] == 0 and json.loads(result[1])["service"] == "current", result
            self.uncertain_start = False
            trial.until(b"Installation graph_ready", plain=True, limit=15)
            code, projects, _ = self.command("project", "list")
            assert code == 0, projects
            if b"No projects." in projects:
                trial.until(b"Use this directory as a project?", plain=True)
                if not keep_project_prompt:
                    trial.send(b"\x1b")
                    end = time.monotonic() + remaining()
                    while "Use this directory as a project?" in trial.screen.text():
                        assert time.monotonic() < end, trial.screen.text()
                        trial.read(0.025)
            return trial
        except Exception:
            trial.close()
            raise

    def start(self):
        self.started = True
        self.uncertain_start = True  # Retain fixture on any uncertain start outcome.
        result = self.command("service", "start")
        assert result[0] == 0, result
        self.uncertain_start = False
        return json.loads(self.command("service", "status", "--json")[1])["service_epoch"]

    def stop(self):
        result = self.command("service", "stop")
        assert result[0] == 0, result
        status = self.command("service", "status", "--json")
        assert status[0] == 0 and json.loads(status[1])["service"] == "absent", status
        self.started = False

    def status(self):
        result = self.command("service", "status", "--json")
        assert result[0] == 0, result
        return json.loads(result[1])

    def wait_absent(self, stable=0):
        end = time.monotonic() + remaining()
        absent_since = None
        while True:
            result = self.command("service", "status", "--json")
            if result[0] == 0 and json.loads(result[1])["service"] == "absent":
                if absent_since is None:
                    absent_since = time.monotonic()
                if time.monotonic() - absent_since >= stable:
                    self.started = False
                    self.uncertain_start = False
                    return
            else:
                absent_since = None
            assert time.monotonic() < end, ("owned backend did not stop", result)
            time.sleep(0.025)

    def cleanup(self):
        global CLEANUP, CLEANUP_DEADLINE
        CLEANUP = True
        CLEANUP_DEADLINE = time.monotonic() + 8
        if self.started:
            try:
                self.stop()
            except AssertionError:
                # Lifetime EOF may already be draining the owner while Stop
                # attaches. Only proved absence can settle that cleanup race.
                self.wait_absent(stable=1)
        if self.uncertain_start:
            self.wait_absent(stable=1)
        assert not self.uncertain_start, "detached startup outcome remains uncertain"
        shutil.rmtree(self.root)


def absent_and_signal(fixture):
    result = fixture.command()
    assert result[0] == 2, ("non-TTY exit", result)
    assert b"\x1b[?1049h" not in result[1] + result[2]
    assert not (fixture.root / "home/.asura").exists()
    for action in ("normal", "/quit", "/exit", "signal", "crash"):
        trial = fixture.launch()
        try:
            assert "Connected (process)" in trial.screen.text(), trial.screen.text()
            assert "uptime " in trial.screen.text() and "memory:" in trial.screen.text(), trial.screen.text()
            log = fixture.root / "home/.asura/logs/asura.log"
            assert log.is_file() and b"service_serving" in log.read_bytes()
            if action == "signal":
                trial.send(b"signal-draft")
                trial.until(b"signal-draft", plain=True)
                trial.signal(signal.SIGTERM)
            elif action == "crash":
                trial.signal(signal.SIGKILL)
            else:
                trial.send(b"\x11" if action == "normal" else action.encode() + b"\r")
            if action == "crash":
                trial.finish_killed()
            else:
                trial.finish()
        finally:
            trial.close()
        fixture.wait_absent()
        print(f"PASS TUI-owned backend stops after {action} exit")


def attached_lifetimes(fixture):
    epoch = fixture.start()
    trial = fixture.launch()
    try:
        assert "Connected (local)" in trial.screen.text(), trial.screen.text()
        trial.send(b"/quit\r")
        trial.finish()
    finally:
        trial.close()
    assert fixture.status()["service_epoch"] == epoch, "attached TUI stopped standalone backend"
    fixture.stop()
    owner = fixture.launch()
    epoch = fixture.status()["service_epoch"]
    attached = None
    try:
        attached = fixture.launch()
        assert "Connected (local)" in attached.screen.text(), attached.screen.text()
        attached.send(b"/exit\r")
        attached.finish()
        attached.close()
        attached = None
        assert fixture.status()["service_epoch"] == epoch, "attacher stopped owner's backend"
        owner.send(b"\x11")
        owner.finish()
    finally:
        if attached is not None:
            attached.close()
        owner.close()
    fixture.wait_absent()
    owner = fixture.launch()
    epoch = fixture.status()["service_epoch"]
    try:
        fixture.stop()
        replacement = fixture.start()
        assert replacement != epoch
        owner.send(b"\x11")
        owner.finish()
    finally:
        owner.close()
    assert fixture.status()["service_epoch"] == replacement, "old owner stopped replacement"
    fixture.stop()
    print("PASS TUI attach does not own standalone/concurrent/replacement backend")


def startup_cancel(fixture):
    for delay in (0, 0.01, 0.05):
        fixture.started = True
        fixture.uncertain_start = True
        trial = Trial(fixture.binary)
        try:
            if delay:
                trial.idle(delay)
            trial.send(b"\x11")
            trial.finish()
        finally:
            trial.close()
        fixture.wait_absent(stable=1)
    print("PASS TUI early startup cancellation leaves no live backend in bounded observation")


def bounded_input(fixture):
    fixture.start()  # Explicit service is independent of the successive TUI lifetimes.
    trial = fixture.launch()
    try:
        trial.until(b"Connected (", plain=True, refresh=True)
        # Past the five-second observation expiry, no input or resize wakes it.
        trial.idle(5.2)
        offset = len(trial.output)
        trial.send(b"idle-ready")
        trial.until(b"idle-ready", offset, plain=True)
        offset = len(trial.output)
        trial.send(b"\r")
        trial.until(b"No project selected", offset, plain=True)
        trial.signal(signal.SIGTERM)
        trial.finish()
    finally:
        trial.close()
    trial = fixture.launch()
    try:
        trial.until(b"Connected (", plain=True, refresh=True)
        # The supervisor confirms SIGSTOP, so readiness is queued while the
        # application cannot consume either event. No further key wakes it.
        trial.stop_child()
        offset = len(trial.output)
        trial.resize(90, 28)
        trial.send(b"ready-together")
        trial.signal(signal.SIGCONT)
        trial.until(b"ready-together", offset, plain=True)
        trial.signal(signal.SIGTERM)
        trial.finish()
    finally:
        trial.close()
    trial = fixture.launch()
    try:
        trial.until(b"Connected (", plain=True, refresh=True)
        trial.send(b"\x1b[")
        trial.idle(0.2)  # Allow the reader to enter its incomplete-CSI branch.
        trial.signal(signal.SIGTERM)
        trial.finish()  # No byte completes or wakes the pending input sequence.
    finally:
        trial.close()
    fixture.stop()
    print("PASS TUI idle input, simultaneous resize/key, incomplete escape, restored flags")


def config_commands(fixture):
    fixture.start()
    trial = fixture.launch()
    try:
        trial.send(b"/con\t")
        trial.until(b"/config", plain=True)
        trial.send(b" set model ollama:granite4.1:8b\r")
        trial.until(b"Saved model", plain=True)
        trial.send(b"\x1b")
        trial.idle(0.1)
        trial.send(b"/config set audit.keepFiles 7\r")
        trial.until(b"Saved audit.keepFiles", plain=True)
        trial.send(b"\x1b")
        trial.idle(0.1)
        trial.send(b"/config\r")
        trial.until(b"model: ollama:granite4.1:8b", plain=True)
        trial.until(b"keepFiles: 7", plain=True)
        trial.until(b"maxFileBytes: 10485760", plain=True)
        trial.send(b"\x1b")
        trial.idle(0.1)
        trial.send(b"/config get audit.keepFiles\r")
        trial.until("Configuration · Esc closes".encode(), plain=True)
        trial.until(b"audit.keepFiles", plain=True)
        trial.send(b"\x1b")
        trial.idle(0.1)
        trial.send(b"/config set audit.keepFiles 0\r")
        trial.until(b"Config error:", plain=True)
        path = fixture.root / "home/.asura/config.yaml"
        saved = path.read_text()
        assert "ollama:granite4.1:8b" in saved and "keepFiles: 7" in saved, saved
        trial.send(b"\x1b")
        trial.idle(0.1)
        trial.send(b"\x01/quit\r")
        trial.finish()
    finally:
        trial.close()
        fixture.stop()
    print("PASS TUI config completion, set/get, validation and persistence")


def models_command(fixture, source=None):
    if source is not None:
        helper = source.parent / "asura-model"
        assert helper.is_file(), "Build the verified inventory helper beside the fixture"
        shutil.copyfile(helper, fixture.root / "asura-model")
        (fixture.root / "asura-model").chmod(0o700)
    fixture.start()
    trial = fixture.launch()
    try:
        trial.send(b"/mod\t")
        trial.until(b"/models", plain=True)
        trial.send(b" extra\r")
        trial.until(b"Use /models without arguments", plain=True)
        trial.send(b"\x01/models\r")
        if source is None:
            trial.until(b"Model inventory error:", plain=True, refresh=True, limit=15)
            assert "/models" in trial.screen.text(), trial.screen.text()
        else:
            trial.until("Models · Esc closes".encode(), plain=True, refresh=True, limit=15,
                        reject="Model inventory error:")
            for text in ["Model", "Provider", "Status", "system"]:
                assert text in trial.screen.text(), trial.screen.text()
        trial.send(b"\x1b")
        trial.idle(0.1)
        trial.send(b"\x01/models\r")
        expected = b"Model inventory error:" if source is None else "Models · Esc closes".encode()
        trial.until(expected, plain=True, refresh=True, limit=15)
        trial.send(b"\x1b")
        trial.idle(0.1)
        trial.send(b"\x01/quit\r")
        trial.finish()
    finally:
        trial.close()
        fixture.stop()
    print("PASS TUI model inventory completion, arity, result, repeated command and exit")


def live_draft(fixture):
    trial = fixture.launch()
    try:
        trial.until(b"Connected (", plain=True, refresh=True)
        first_epoch = fixture.start()
        trial.until(b"Connected (", plain=True, refresh=True)
        trial.until(b"Installation graph_ready", plain=True)
        # Bracketed paste preserves a multiline draft without submission.
        trial.send(b"\x1b[200~draft-alpha\nsecond-line\x1b[201~")
        offset = trial.repaint(90, 28)
        trial.until(b"draft-alpha", offset, plain=True, refresh=True)
        trial.until(b"second-line", offset, plain=True, refresh=True)
        trial.send(b"\r")
        trial.until(b"No project selected", plain=True, refresh=True)
        offset = trial.repaint(92, 29)
        trial.until(b"draft-alpha", offset, plain=True, refresh=True)
        trial.until(b"second-line", offset, plain=True, refresh=True)
        trial.send(b"\x11")
        trial.until(b"Discard draft and exit?", plain=True, refresh=True)
        trial.send(b"\r")  # Keep editing is the default.
        trial.send(b"-kept")  # Observable barrier: overlay must be closed to edit.
        offset = trial.repaint(94, 30)
        trial.until(b"second-line-kept", offset, plain=True, refresh=True)
        fixture.stop()
        offset = len(trial.output)
        trial.until(b"Disconnected", offset, plain=True, refresh=True)
        second_epoch = fixture.start()
        assert first_epoch != second_epoch, "fixture restart retained epoch"
        offset = len(trial.output)
        trial.until(b"Connected (", offset, plain=True, refresh=True)
        offset = trial.repaint(80, 24)
        trial.until(b"draft-alpha", offset, plain=True, refresh=True)
        trial.until(b"second-line", offset, plain=True, refresh=True)
        offset = len(trial.output)
        trial.send(b"\x11")
        trial.until(b"Discard draft and exit?", offset, plain=True, refresh=True)
        offset = len(trial.output)
        trial.send(b"\t")
        trial.until(b"[Discard and exit]", offset, plain=True, refresh=True)
        trial.send(b"\r")
        trial.finish()
    finally:
        trial.close()
    fixture.stop()
    print("PASS TUI real attach/restart, paste/Enter/resize retained draft, discard cleanup")


def stalled_backend(fixture):
    # A foreground fixture is a retained direct child. No PID file or discovered
    # process identity is used for signal injection.
    stdout = fixture.root / "foreground.out"
    stderr = fixture.root / "foreground.err"
    with stdout.open("wb") as out, stderr.open("wb") as err:
        backend = subprocess.Popen([str(fixture.binary), "service", "run"],
            cwd=fixture.root, stdin=subprocess.DEVNULL, stdout=out, stderr=err)
    fixture.started = True
    trial = None
    stopped = False
    try:
        end = time.monotonic() + remaining()
        while True:
            assert backend.poll() is None, "foreground fixture exited before serving"
            code, data, _ = fixture.command("service", "status", "--json")
            if code == 0 and json.loads(data)["service"] == "current":
                break
            assert time.monotonic() < end, "foreground service did not become ready"
            time.sleep(0.01)
        trial = fixture.launch()
        os.kill(backend.pid, signal.SIGSTOP)
        stopped = True
        observed, status = os.waitpid(backend.pid, os.WUNTRACED)
        assert observed == backend.pid and os.WIFSTOPPED(status)
        offset = len(trial.output)
        trial.send(b"backend-stalled")
        trial.until(b"backend-stalled", offset, plain=True)
        offset = len(trial.output)
        trial.send(b"\r")
        trial.until(b"No project selected", offset, plain=True)
        # Let the service observation timeout and expire while the terminal works.
        trial.idle(5.2)
        trial.until(b"Disconnected", plain=True, refresh=True)
        offset = len(trial.output)
        trial.send(b"\x01/config set model system\r")
        trial.until(b"Configuration request pending", offset, plain=True, refresh=True)
        offset = len(trial.output)
        trial.send(b"\x01/models\r")
        trial.until(b"Model inventory pending", offset, plain=True, refresh=True)
        offset = len(trial.output)
        trial.send(b"\x11")
        trial.until(b"Discard draft and exit?", offset, plain=True, refresh=True)
        offset = len(trial.output)
        trial.send(b"\t")
        trial.until(b"[Discard and exit]", offset, plain=True, refresh=True)
        started = time.monotonic()
        trial.send(b"\r")
        trial.finish(limit=2)
        elapsed = time.monotonic() - started
        print(f"PASS TUI stalled owned backend: editing, expiry, exit {elapsed:.3f}s")
    finally:
        if trial is not None:
            trial.close()
        if stopped and backend.poll() is None:
            os.kill(backend.pid, signal.SIGCONT)
        try:
            fixture.stop()
            assert backend.wait(timeout=remaining()) == 0
        except Exception:
            # Only the retained direct process is killed on failed fixture cleanup.
            if backend.poll() is None:
                backend.kill()
                backend.wait(timeout=5)
            raise


def repeated_initialization(fixture):
    project = fixture.root
    default_name = project.name[0].upper() + project.name[1:]
    installation_id = project_id = None
    metadata_root = fixture.root / "home/.asura"
    metadata_root.mkdir(parents=True, exist_ok=True)
    metadata_root.chmod(0o700)
    finder = metadata_root / ".DS_Store"
    finder.write_bytes(b"Finder metadata fixture")
    finder.chmod(0o644)

    # Declining the first-use prompt leaves the canonical registry empty.
    trial = fixture.launch(keep_project_prompt=True)
    try:
        trial.send(b"\x1b")
        end = time.monotonic() + remaining()
        while "Use this directory as a project?" in trial.screen.text():
            assert time.monotonic() < end, trial.screen.text()
            trial.read(0.025)
        code, projects, _ = fixture.command("project", "list")
        assert code == 0 and b"No projects." in projects, projects
        trial.send(b"/quit\r")
        trial.finish()
    finally:
        trial.close()
    fixture.wait_absent(stable=0.2)

    # A new session may offer again. Yes registers exactly the shown directory.
    trial = fixture.launch(keep_project_prompt=True)
    try:
        assert str(project) in trial.screen.text(), trial.screen.text()
        trial.send(b"\r")
        end = time.monotonic() + remaining(15)
        while True:
            code, projects, _ = fixture.command("project", "list")
            match = re.search(r"([0-9a-f]{32})  " + re.escape(default_name) + r"  " + re.escape(str(project)), projects.decode())
            if code == 0 and match:
                project_id = match[1]
                break
            assert time.monotonic() < end, projects
            trial.read(0.025)
        trial.until((default_name + " · ").encode(), plain=True)
        trial.send(b"\x1b")
        trial.idle(0.1)
        trial.send(b"/quit\r")
        trial.finish()
    finally:
        trial.close()
    fixture.wait_absent(stable=0.2)

    def dismiss(trial):
        trial.send(b"\x1b")
        # Wait for the panel to close before the next command.
        end = time.monotonic() + remaining()
        while "Setup · Esc closes" in trial.screen.text():
            assert time.monotonic() < end, trial.screen.text()
            trial.read(0.025)

    def command(trial, text, expected):
        offset = len(trial.output)
        trial.send(text.encode() + b"\r")
        trial.until(expected.encode(), offset, plain=True, limit=15)
        return trial.screen.text()

    for restarted in (False, True):
        trial = fixture.launch()
        try:
            # No setup command: first launch itself must become ready.
            trial.until(b"Installation graph_ready", plain=True, limit=15)
            assert "Use this directory as a project?" not in trial.screen.text()
            trial.until((("My project" if restarted else default_name) + " · ").encode(), plain=True)
            assert "No projects" not in trial.screen.text(), trial.screen.text()
            for _ in range(2):
                screen = command(trial, "/tools", "Tools · Esc closes")
                trial.until(b"read_file(path", plain=True)
                assert "Description" in trial.screen.text(), trial.screen.text()
                trial.send(b"\x1b")
                end = time.monotonic() + remaining(5)
                while "Tools · Esc closes" in trial.screen.text():
                    assert time.monotonic() < end, trial.screen.text()
                    trial.read(0.025)
            screen = command(trial, "/audit 4", "Audit · Esc closes")
            trial.until(b"Window: recent", plain=True)
            assert "capacity 256" in trial.screen.text(), trial.screen.text()
            trial.send(b"\x1b")
            end = time.monotonic() + remaining(5)
            while "Audit · Esc closes" in trial.screen.text():
                assert time.monotonic() < end, trial.screen.text()
                trial.read(0.025)
            screen = command(trial, "/audit 17", "Use /audit [limit]")
            assert "/audit 17" in screen, screen
            trial.send(b"\x01")
            screen = command(trial, "/tools extra", "Use /tools without arguments")
            assert "/tools extra" in screen, screen
            trial.send(b"\x01")
            for text, feedback in [("/cancel", "No active conversation to cancel"), ("/retry extra", "Use /retry")]:
                screen = command(trial, text, feedback)
                assert text in screen, screen
                trial.send(b"\x01")
            screen = command(trial, "/init", "Installation already initialized.")
            assert "request_conflict" not in screen and "outcome unconfirmed" not in screen, screen
            dismiss(trial)
            trial.until(b"Installation graph_ready", plain=True)
            code, raw, _ = fixture.command("installation", "status", "--json")
            assert code == 0, raw
            current = json.loads(raw)
            if restarted:
                assert current["installation_id"] == installation_id, current
            else:
                installation_id = current["installation_id"]
            journal = fixture.root / "home/.asura/state/control/slot-0.log"
            before = journal.read_bytes()
            command(trial, "/init", "Installation already initialized.")
            assert journal.read_bytes() == before, "repeated init changed journal"
            dismiss(trial)
            for _ in range(2):
                screen = command(trial, "/project add " + str(project), "Setup · Esc closes")
                match = re.search(r"Project ([0-9a-f]{32})", screen)
                assert match, screen
                if project_id is None:
                    project_id = match[1]
                assert match[1] == project_id, screen
                dismiss(trial)
            screen = command(trial, "/project list", project_id + "  ")
            assert screen.count(project_id) == 1 and "[stale]" not in screen, screen
            dismiss(trial)
            # Restoration may replace the brief selection notice before redraw.
            screen = command(
                trial,
                "/project select " + project_id,
                "No previous conversation in this project",
            )
            assert ("My project" if restarted else default_name) + " · " in screen, screen
            if not restarted:
                screen = command(trial, "/project rename my project", "Project renamed to My project")
                assert "My project · " in screen, screen
                screen = command(trial, "/project list", "My project  " + str(project))
                assert screen.count(project_id) == 1, screen
                dismiss(trial)
            # Canonical CLI list independently confirms the persisted registry.
            code, listed, _ = fixture.command("project", "list")
            assert code == 0 and listed.decode().count(project_id) == 1, listed
            assert (project_id + "  My project  " + str(project)).encode() in listed, listed
            invalid = "/project add " + str(fixture.root / "missing")
            before = journal.read_bytes()
            screen = command(trial, invalid, "project_stale")
            assert invalid in screen, screen
            assert journal.read_bytes() == before, "rejected project changed journal"
            trial.send(b"\x01/quit\r")
            trial.finish()
        finally:
            trial.close()
        fixture.wait_absent(stable=0.2)
    assert finder.read_bytes() == b"Finder metadata fixture", "Finder metadata changed"
    nested = project / "nested-project"
    launch_dir = nested / "src"
    launch_dir.mkdir(parents=True)
    subprocess.run(["/usr/bin/git", "init", "--initial-branch=context-test", str(nested)],
                   check=True, capture_output=True, timeout=5)
    tracked = launch_dir / "tracked.txt"
    tracked.write_text("first\nsecond\nthird\n")
    subprocess.run(["/usr/bin/git", "-C", str(nested), "add", "src/tracked.txt"],
                   check=True, capture_output=True, timeout=5)
    subprocess.run(["/usr/bin/git", "-C", str(nested), "-c", "user.name=Asura Test",
                    "-c", "user.email=asura-test@example.invalid", "-c", "commit.gpgsign=false",
                    "-c", "core.hooksPath=/dev/null", "commit", "-m", "Private fixture"],
                   check=True, capture_output=True, timeout=5)
    trial = fixture.launch()
    try:
        code, out, err = fixture.command("project", "add", str(nested))
        assert code == 0, (out, err)
        trial.send(b"/quit\r")
        trial.finish()
    finally:
        trial.close()
    fixture.wait_absent(stable=0.2)
    trial = fixture.launch(cwd=launch_dir)
    try:
        trial.resize(200, 24)
        trial.until("Nested-project · ".encode(), plain=True)
        displayed_path = "…/" + "/".join(launch_dir.parts[-4:])
        trial.until(displayed_path.encode(), plain=True)
        trial.until(b"context-test 0 +0-0", plain=True, limit=12)
        offset = len(trial.output)
        changed = launch_dir / "signal-observation.txt"
        changed.write_text("filesystem signal, no terminal input\n")
        trial.until(b"context-test 1 +0-0", offset, plain=True, limit=12)
        assert displayed_path in trial.screen.text(), trial.screen.text()
        offset = len(trial.output)
        changed.unlink()
        trial.until(b"context-test 0 +0-0", offset, plain=True, limit=12)
        offset = len(trial.output)
        tracked.write_text("first\nreplacement\nextra\nthird\n")
        trial.until(b"context-test 1 +2-1", offset, plain=True, limit=12)
        offset = len(trial.output)
        tracked.write_text("first\nsecond\nthird\n")
        trial.until(b"context-test 0 +0-0", offset, plain=True, limit=12)
        assert displayed_path in trial.screen.text(), trial.screen.text()
        assert "No projects" not in trial.screen.text(), trial.screen.text()
        trial.send(b"/quit\r")
        trial.finish()
    finally:
        trial.close()
    fixture.wait_absent(stable=0.2)
    print("PASS automatic setup with Finder metadata, init/repeat/restart, stable project add/list/select, invalid path, visible results and cleanup")
    print("PASS registered project selection, nested path, and Git numeric totals from filesystem signals without terminal input")


def queue_history_admission(fixture, source):
    # The oracle is durable admission, never generated text or native availability.
    helper = source.parent / "asura-model"
    assert helper.is_file(), "Build the packaged helper beside the lifecycle fixture"
    shutil.copyfile(helper, fixture.root / "asura-model")
    (fixture.root / "asura-model").chmod(0o700)
    result = fixture.command("--seed-queue-history")
    assert result[0] == 0, result
    for count in (1, 2):
        trial = fixture.launch(cwd=fixture.root / "project")
        try:
            trial.until("project · ".encode(), plain=True, limit=15)
            trial.until(f"Restored {count + 3} conversation turns".encode(), plain=True, limit=20)
            trial.send(b"\x1b[A")  # Up enters the scrollable conversation pane.
            trial.until(b"scroll history", plain=True, limit=5)
            focus_offset = len(trial.output)
            trial.send(b"\x1b[5~\x1b")  # Page up, then return to the editor.
            trial.until(b"^P/^N recall", focus_offset, plain=True, limit=5)
            # Wait for actual historical ingestion; elapsed time is not evidence.
            trial.send(b"/queue\r")
            trial.until(b"Input queue", plain=True, limit=12)
            trial.until(b"Complete", plain=True, limit=12)
            trial.send(b"\x1b")
            end = time.monotonic() + remaining(5)
            while "Input queue" in trial.screen.text():
                assert time.monotonic() < end, trial.screen.text()
                trial.read(0.025)
            trial.send(f"queue-history-admission-{count}\r".encode())
            trial.until(b"Generating", plain=True, limit=12, reject="request_conflict")
            trial.send(b"activity draft")
            trial.send(b"\x1b[17~")  # F6 enters response history without recalling input.
            trial.until(b"responses", plain=True, limit=5)
            trial.send(b"\r")  # Expand/collapse activity, not a new submission.
            trial.send(b"\x1b[5~\x1b[6~")  # Page through retained response.
            trial.send(b"\x1b")
            trial.idle(0.1)
            assert "activity draft" in trial.screen.text(), trial.screen.text()
            # Queue-first admission holds successors if the preceding operation
            # is interrupted. Let this admitted turn settle before closing its
            # owner, so the next launch can dispatch the next queued input.
            end = time.monotonic() + remaining(65)
            marker = f"queue-history-admission-{count}"
            while True:
                screen = trial.screen.text()
                assert marker in screen, screen
                response = screen.split(marker, 1)[1]
                if "Complete" in response and "Generating" not in response:
                    break
                assert "Incomplete response" not in response, screen
                assert time.monotonic() < end, screen
                trial.read(0.025)
            trial.send(b"\x01/quit\r")
            trial.finish()
        finally:
            trial.close()
        fixture.wait_absent(stable=0.2)
        result = fixture.command("--verify-queue-history", str(count))
        assert result[0] == 0, result
    print("PASS canonical queued generation3/direct4 history, two restored TUI admissions, exact replay and owned cleanup")


def managed_queue_reorder(fixture):
    # These messages are durably accepted but held behind a failed predecessor.
    # The owner cannot dispatch them before the TUI move is observed.
    result = fixture.command("--seed-queue-reorder")
    assert result[0] == 0, result
    trial = fixture.launch(cwd=fixture.root / "project")
    try:
        trial.until(b"Queued messages:", plain=True, limit=15)
        end = time.monotonic() + remaining(8)
        while True:
            screen = trial.screen.text()
            if "queue-reorder-first" in screen and "queue-reorder-second" in screen:
                assert screen.index("queue-reorder-first") < screen.index("queue-reorder-second"), screen
                break
            assert time.monotonic() < end, screen
            trial.read(0.025)
        trial.send(b"\x1b[A")  # From empty editor, focus the last accepted row.
        trial.until(b"[ move up", plain=True, limit=5)
        offset = len(trial.output)
        trial.send(b"[")
        trial.until(b"Queue order updated", offset, plain=True, limit=12,
                    reject="Queue order changed")
        screen = trial.screen.text()
        assert screen.index("queue-reorder-second") < screen.index("queue-reorder-first"), screen
        trial.send(b"\x1b")
        trial.until(b"^P/^N recall", plain=True, limit=5)
        trial.send(b"/quit\r")
        trial.finish()
    finally:
        trial.close()
    fixture.wait_absent(stable=0.2)
    result = fixture.command("--verify-queue-reorder", "moved")
    assert result[0] == 0, result
    # A second owner replays the same format-1 journal and publishes the order.
    trial = fixture.launch(cwd=fixture.root / "project")
    try:
        trial.until(b"Queued messages:", plain=True, limit=15)
        end = time.monotonic() + remaining(8)
        while True:
            screen = trial.screen.text()
            if "queue-reorder-first" in screen and "queue-reorder-second" in screen:
                assert screen.index("queue-reorder-second") < screen.index("queue-reorder-first"), screen
                break
            assert time.monotonic() < end, screen
            trial.read(0.025)
        trial.send(b"/quit\r")
        trial.finish()
    finally:
        trial.close()
    fixture.wait_absent(stable=0.2)
    result = fixture.command("--verify-queue-reorder", "moved")
    assert result[0] == 0, result
    print("PASS managed queue TUI reorder, durable replay after owner restart and owned cleanup")


def native_conversation(fixture, source):
    helper = source.parent / "asura-model"
    assert helper.is_file(), "Build the verified native helper beside the fixture executable"
    shutil.copyfile(helper, fixture.root / "asura-model")
    (fixture.root / "asura-model").chmod(0o700)
    project = fixture.root / "project"
    project.mkdir(mode=0o700)
    trial = fixture.launch()
    try:
        trial.until(b"Installation graph_ready", plain=True, limit=15)
        trial.send(b"/init\r")
        trial.until(b"Installation already initialized.", plain=True, limit=15)
        trial.send(b"\x1b")
        trial.idle(0.1)
        trial.send(("/project add " + str(project) + "\r").encode())
        trial.until(b"Project ", plain=True)
        assert re.search(r"Project [0-9a-f]{32}", trial.screen.text()), trial.screen.text()
        trial.send(b"\x1b")
        trial.idle(0.1)
        trial.send(b"Reply with just the word ORCHARD.\r")
        trial.until(b"Complete", plain=True, limit=65, reject="Incomplete response")
        assert trial.screen.text().count("ORCHARD") >= 2, trial.screen.text()
        trial.until(b"% input", plain=True)
        assert re.search(r"\d+% input.* · (?!\?)[^\n]+", trial.screen.text()), trial.screen.text()
        trial.send(b"Write a detailed 300 word explanation of forests.\r")
        trial.until(b"Generating", plain=True, limit=15)
        trial.send(b"Reply with only PINE.\r")
        trial.until(b"Queued messages:", plain=True, limit=5)
        assert "select an action" not in trial.screen.text(), trial.screen.text()
        # Service dispatch survives the original observer completing. The TUI
        # discovers its operation instead of resubmitting the queued prompt.
        end = time.monotonic() + remaining(120)
        while trial.screen.text().count("PINE") < 2 or "Complete" not in trial.screen.text():
            assert time.monotonic() < end, trial.screen.text()
            trial.read(0.025)
        # Advance the same conversation past the queued generation before restart.
        trial.send(b"Reply with only BIRCH.\r")
        end = time.monotonic() + remaining(65)
        while trial.screen.text().count("BIRCH") < 2 or "Complete" not in trial.screen.text():
            assert "request_conflict" not in trial.screen.text(), trial.screen.text()
            assert time.monotonic() < end, trial.screen.text()
            trial.read(0.025)
        trial.send(b"/queue\r")
        trial.until(b"Input queue", plain=True)
        assert "Complete" in trial.screen.text(), trial.screen.text()
        trial.send(b"\x1b")
        trial.idle(0.1)
        trial.send(b"/quit\r")
        trial.finish()
        trial.close()
        fixture.wait_absent(stable=0.2)
        trial = fixture.launch(cwd=project)
        trial.until("project · ".encode(), plain=True, limit=15)
        trial.send(b"Reply with only MAPLE.\r")
        end = time.monotonic() + remaining(65)
        while trial.screen.text().count("MAPLE") < 2 or "Complete" not in trial.screen.text():
            assert "request_conflict" not in trial.screen.text(), trial.screen.text()
            assert time.monotonic() < end, trial.screen.text()
            trial.read(0.025)
        trial.send(b"/quit\r")
        trial.finish()
    finally:
        if sys.exc_info()[0] is not None:
            log = fixture.root / "home/.asura/logs/asura.log"
            if log.is_file():
                with log.open("rb") as handle:
                    handle.seek(max(0, log.stat().st_size - 8192))
                    for line in handle.read(8192).decode("utf-8", errors="replace").splitlines():
                        if "model_helper_diagnostic" in line:
                            print(line, file=sys.stderr)
        if trial.process.poll() is None:
            try:
                trial.signal(signal.SIGTERM)
                trial.finish(limit=3)
            finally:
                trial.close()
        else:
            trial.close()
    fixture.wait_absent(stable=0.2)
    print("PASS native TUI setup, real response, default queue admission, service-dispatched successor, queue inspection, direct successor, persisted-history restart and cleanup")


def composer_interactions(fixture, source):
    """Real service selectors and local recall; no model weights or inference."""
    # Keep this port reserved but not listening, so discovery cannot contact a
    # user's Ollama daemon or race with another process taking an unused port.
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as endpoint_guard:
        endpoint_guard.bind(("127.0.0.1", 0))
        shutil.copyfile(source.parent / "asura-model", fixture.root / "asura-model")
        (fixture.root / "asura-model").chmod(0o700)
        # Let the service create its private authority/runtime tree first. Creating
        # all parents via mkdir(parents=True) would give .asura the default 0755 mode.
        fixture.start()
        config = fixture.root / "home/.asura/config.yaml"
        config.write_text(f"providers:\n  ollama:\n    endpoint: http://127.0.0.1:{endpoint_guard.getsockname()[1]}\n")
        config.chmod(0o600)
        model = fixture.root / "home/.asura"
        for component in ("data", "models", "coreai", "composer-test"):
            model = model / component
            model.mkdir(mode=0o700, exist_ok=True)
            model.chmod(0o700)
        metadata = model / "metadata.json"
        metadata.write_text(json.dumps({"kind":"llm", "assets":{"main":"model.aimodel"}, "language":{"max_context_length":4096}}))
        metadata.chmod(0o600)
        trial = fixture.launch()
        try:
            for name in ("composer-one", "composer-two"):
                project = fixture.root / name
                project.mkdir(mode=0o700)
                code, output, error = fixture.command("project", "add", str(project))
                assert code == 0, (output, error)
            trial.send(b"/project list\r")
            trial.until("Setup · Esc closes".encode(), plain=True)
            trial.send(b"\x1b")
            trial.idle(0.1)
            trial.send(b"preserved draft")
            trial.until(b"preserved draft", plain=True)
            offset = len(trial.output)
            trial.send(b"\x1b[B")
            trial.until(b"Enter open", offset, plain=True)
            offset = len(trial.output)
            trial.send(b"\r")
            trial.until("↑↓ select".encode(), offset, plain=True)
            trial.until(b"Composer-two", plain=True)
            offset = len(trial.output)
            trial.send(b"\x1b[F\r")
            trial.until(b"Project selected", offset, plain=True)
            trial.until(b"Enter open", offset, plain=True)
            assert "preserved draft" in trial.screen.text(), trial.screen.text()
            offset = len(trial.output)
            trial.send(b"\t\r")
            trial.until(b"coreai:composer-test", offset, plain=True, limit=15, reject="Generating")
            # Inventory is sorted by provider then selector. Only this CoreAI
            # fixture and system exist; Home selects this exact metadata candidate.
            offset = len(trial.output)
            trial.send(b"\x1b[H\r")
            trial.until(b"Model selected for future inputs", offset, plain=True)
            # The hints row is drawn after the editor. Wait for the completed focus
            # transition rather than inspecting an intermediate terminal write.
            trial.until(b"Enter open", offset, plain=True)
            trial.until(b"preserved draft", offset, plain=True)
            saved = (fixture.root / "home/.asura/config.yaml").read_text()
            assert "coreai:composer-test" in saved, saved
            # Picker-origin config must not create an overlay or consume the draft.
            assert "Configuration · Esc closes" not in trial.screen.text()
            trial.send(b"\x1b")
            trial.idle(0.1)
            trial.send(b"\x10")
            trial.until(b"/project list", plain=True)
            trial.send(b"\x0e")
            trial.until(b"preserved draft", plain=True)
            trial.send(b"\x01/quit\r")
            trial.finish()
        finally:
            trial.close()
            fixture.stop()
        print("PASS composer project/model selectors, acknowledged model save, preserved draft, history and cleanup")


def main():
    global DEADLINE
    assert len(sys.argv) in (2, 3), __doc__
    native = len(sys.argv) == 3 and sys.argv[2] == "--conversation"
    setup = len(sys.argv) == 3 and sys.argv[2] == "--setup"
    models = len(sys.argv) == 3 and sys.argv[2] == "--models"
    composer = len(sys.argv) == 3 and sys.argv[2] == "--composer"
    queue_history = len(sys.argv) == 3 and sys.argv[2] == "--queue-history"
    queue_reorder = len(sys.argv) == 3 and sys.argv[2] == "--queue-reorder"
    assert len(sys.argv) == 2 or native or setup or models or composer or queue_history or queue_reorder, __doc__
    if native:
        DEADLINE = time.monotonic() + 240
    source = Path(sys.argv[1]).resolve(strict=True)
    fixture = Fixture(source)
    try:
        if queue_reorder:
            managed_queue_reorder(fixture)
            return
        if queue_history:
            queue_history_admission(fixture, source)
            return
        if composer:
            composer_interactions(fixture, source)
            return
        if models:
            models_command(fixture, source)
            return
        if setup:
            repeated_initialization(fixture)
            return
        if native:
            native_conversation(fixture, source)
            return
        absent_and_signal(fixture)
        attached_lifetimes(fixture)
        startup_cancel(fixture)
        config_commands(fixture)
        models_command(fixture)
        bounded_input(fixture)
        live_draft(fixture)
        stalled_backend(fixture)
        # Fault injection: cleanup must stop an independently started backend
        # even when the journey's work budget has already expired.
        fixture.start()
        DEADLINE = time.monotonic() - 1
    finally:
        try:
            fixture.cleanup()
            if not native and not setup and not models and not composer and not queue_history and not queue_reorder:
                print("PASS expired work budget still stops independent test backend with fresh cleanup budget")
        except Exception:
            print(f"Fixture cleanup unconfirmed; retained {fixture.root}", file=sys.stderr)
            raise


if __name__ == "__main__":
    def terminate(signum, frame):
        raise RuntimeError("Terminal regression terminated; running fixture cleanup")
    signal.signal(signal.SIGTERM, terminate)
    main()
