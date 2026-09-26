#!/usr/bin/env python3
"""TP2/TP5 process-level checks with a controlling PTY and bounded waits.

Run with the built binary path. Native terminal rendering/copy remains a separate
qualification: this test verifies process behavior and actual POSIX tty flags.
"""

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
import sys
import termios
import time
import pyte
from wcwidth import wcwidth


# macOS revokes a PTY when its controlling session leader exits. Keep a supervisor
# alive long enough to compare the actual tty modes after the tested child exits.
SUPERVISOR = """
import fcntl, os, struct, subprocess, sys, termios
before = termios.tcgetattr(0)
child = subprocess.Popen(sys.argv[1:])
print('__ASURA_TEST_PID__' + str(child.pid), flush=True)
code = child.wait()
after = termios.tcgetattr(0)
if sys.platform == 'darwin' and before != after:
    # XNU marks already queued input for reprocessing when ICANON is restored.
    # Require that PENDIN is the ONLY difference, settle it with a non-consuming
    # FIONREAD query, then still require full termios equality below.
    other_fields_equal = all(a == b for i, (a, b) in enumerate(zip(before, after)) if i != 3)
    if other_fields_equal and (before[3] ^ after[3]) == termios.PENDIN:
        fcntl.ioctl(0, termios.FIONREAD, struct.pack('I', 0))
        after = termios.tcgetattr(0)
restored = before == after
if not restored:
    print('__TTY_BEFORE__' + repr(before), flush=True)
    print('__TTY_AFTER__' + repr(after), flush=True)
termios.tcsetattr(0, termios.TCSANOW, before)
print('__ASURA_TTY_RESTORED__' + str(restored), flush=True)
sys.exit(code if restored else 97)
"""


class TerminalScreen(pyte.Screen):
    """Repair pyte 0.8.2's stale wide-cell stub on narrow overwrite only."""

    def draw(self, data):
        for char in data:
            x, y = self.cursor.x, self.cursor.y
            old = self.buffer[y][x].data if x < self.columns else " "
            replaces_wide = (wcwidth(char) == 1 and old and wcwidth(old[0]) == 2
                             and x + 1 < self.columns and self.buffer[y][x + 1].data == "")
            super().draw(char)
            if replaces_wide:
                self.buffer[y][x + 1] = self.buffer[y][x + 1]._replace(data=" ")


def decoder_wide_overwrite():
    for replacement, expected in [(None, "界面X"), (" ", "  面X"), ("A", "A 面X")]:
        screen = TerminalScreen(5, 1)
        stream = pyte.ByteStream(screen)
        stream.feed("\x1b[31m界面X".encode())
        trailing = screen.buffer[0][1]
        if replacement is not None:
            stream.feed(("\x1b[1;1H" + replacement).encode())
            assert screen.buffer[0][1] == trailing._replace(data=" ")
        assert screen.display == [expected], screen.display


class Trial:
    def __init__(self, binary, *arguments, color=False):
        self.master, self.slave = pty.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        self.original = termios.tcgetattr(self.slave)
        self.output = bytearray()
        self.screen = TerminalScreen(80, 24)
        self.screen.write_process_input = lambda data: os.write(self.master, data.encode())
        self.stream = pyte.ByteStream(self.screen)

        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)

        child_environment = {**os.environ, "TERM": "xterm-256color"}
        if color:
            child_environment.pop("NO_COLOR", None)
            child_environment["COLORTERM"] = "truecolor"
        self.process = subprocess.Popen(
            [sys.executable, "-c", SUPERVISOR, binary, *arguments], stdin=self.slave, stdout=self.slave,
            stderr=self.slave, close_fds=True, preexec_fn=controlling_terminal,
            env=child_environment,
        )

    def read(self, timeout):
        if select.select([self.master], [], [], timeout)[0]:
            try:
                chunk = os.read(self.master, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                return
            self.output.extend(chunk)
            self.stream.feed(chunk)

    def visible(self):
        return "\n".join(self.screen.display)

    def until_screen(self, text, timeout=8):
        deadline = time.monotonic() + timeout
        while text not in self.visible() and time.monotonic() < deadline:
            self.read(min(0.1, max(0, deadline - time.monotonic())))
            if self.process.poll() is not None:
                self.read(0)
                break
        assert text in self.visible(), f"missing {text!r}:\n{self.visible()}"

    def until(self, needle, timeout=8):
        deadline = time.monotonic() + timeout
        while needle not in self.output and time.monotonic() < deadline:
            self.read(min(0.1, max(0, deadline - time.monotonic())))
            if self.process.poll() is not None:
                self.read(0)
                break
        assert needle in self.output, f"missing {needle!r}: {self.output[-2000:]!r}"

    def until_absent(self, text, timeout=8):
        deadline = time.monotonic() + timeout
        while text in self.visible() and time.monotonic() < deadline:
            self.read(min(0.1, max(0, deadline - time.monotonic())))
        assert text not in self.visible(), f"still visible {text!r}:\n{self.visible()}"

    def until_background(self, x, y, color, timeout=8):
        deadline = time.monotonic() + timeout
        while self.screen.buffer[y][x].bg != color and time.monotonic() < deadline:
            self.read(min(0.1, max(0, deadline - time.monotonic())))
        actual = self.screen.buffer[y][x].bg
        assert actual == color, (x, y, actual, color)

    def until_foreground(self, x, y, color, timeout=8):
        deadline = time.monotonic() + timeout
        while self.screen.buffer[y][x].fg != color and time.monotonic() < deadline:
            self.read(min(0.1, max(0, deadline - time.monotonic())))
        actual = self.screen.buffer[y][x].fg
        assert actual == color, (x, y, actual, color)

    def send(self, data):
        os.write(self.master, data)

    def resize(self, columns, rows):
        self.screen.resize(lines=rows, columns=columns)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))

    def finish(self, expected):
        deadline = time.monotonic() + 8
        while self.process.poll() is None and time.monotonic() < deadline:
            self.read(0.1)
        assert self.process.poll() is not None, "process did not exit within 8 seconds"
        self.read(0)
        assert self.process.returncode == expected, (self.process.returncode, self.output[-2000:])
        assert b"__ASURA_TTY_RESTORED__True" in self.output, "terminal flags not restored"
        assert b"\x1b[?1049l" in self.output, "alternate screen not released"
        if b"\x1b[?2004h" in self.output:
            assert b"\x1b[?2004l" in self.output, "bracketed paste not disabled"
        if b"\x1b[?25l" in self.output:
            assert b"\x1b[?25h" in self.output, "cursor not restored"

    def close(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGKILL)
        # Closing both PTY endpoints also releases a child blocked in a Darwin
        # tty operation; always close them before waiting for failure cleanup.
        os.close(self.master)
        os.close(self.slave)
        self.process.wait(timeout=5)


def run_case(binary, name, action, arguments=(), expected=0, color=False):
    trial = Trial(binary, *arguments, color=color)
    try:
        action(trial)
        trial.finish(expected)
        print(f"PASS {name}")
    finally:
        trial.close()


def exit_empty(trial):
    trial.until_screen("18% used")
    trial.send(b"\x11")


def paste_draft(trial):
    trial.until_screen("18% used")
    trial.send(b"\x1b[200~first\r\nsecond\x1b[201~")
    trial.until_screen("second")
    assert "18% used" in trial.visible()
    assert "Sending" not in trial.visible() and "Message received" not in trial.visible()
    trial.send(b"\x11")
    trial.until_screen("Unsaved")
    # No default selection: opening then Enter must retain the warning.
    # Dismissal and new typing is an observable barrier proving it did not exit.
    trial.send(b"\r\x1bOPx")  # F1 dismisses; unlike Esc+x this cannot decode as Alt+x.
    trial.until_screen("secondx")
    assert trial.process.poll() is None
    trial.send(b"\x11")
    trial.until_screen("Unsaved")
    trial.send(b"\t\r")


def newline_and_acknowledgement(trial):
    trial.until_screen("18% used")
    trial.send(b"first\n\x1b\rsecond")  # Ctrl+J is inert; Alt+Enter inserts.
    trial.until_screen("12 chars")  # Exactly first + one newline + second.
    assert_composer(trial, ["first", "second"])
    trial.until_screen("second")
    assert "18% used" in trial.visible() and "Sending" not in trial.visible()
    trial.send(b"\r")
    trial.until_screen("F5 accepts")
    trial.send(b"new draft")
    trial.until_screen("new draft")
    trial.send(b"\x1b[15~")  # F5: explicit manual acknowledgement.
    trial.until_screen("Message received")
    assert "new draft" in trial.visible() and "Sending" not in trial.visible()
    trial.send(b"\x11")
    trial.until_screen("Unsaved")
    trial.send(b"\t\r")


def interrupt(trial, sig):
    trial.until_screen("18% used")
    match = re.search(rb"__ASURA_TEST_PID__(\d+)", trial.output)
    assert match is not None, "missing supervised child identity"
    os.kill(int(match.group(1)), sig)


def start_work(trial):
    trial.until_screen("18% used")
    trial.send(b"start work\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[15~")
    trial.until_screen("Message received")
    trial.until_absent("F5 accepts")


def exit_with_text(trial):
    trial.send(b"\x11")
    trial.until_screen("Unsaved")
    trial.send(b"\t\r")


def assert_composer(trial, lines):
    # Terminal writes may arrive in partial frames. Wait for the entire editor
    # and its following strip/status, not just disappearance of an overlay title.
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        rows = trial.screen.display
        matches = [index for index, row in enumerate(rows) if row.strip() == "› " + lines[0]]
        if len(matches) == 1:
            start = matches[0]
            body_matches = all(rows[start + offset].strip() == expected
                               for offset, expected in enumerate(lines[1:], 1))
            following = rows[start + len(lines)].strip()
            status_row = following == rows[-1].strip() and "%" in following
            if body_matches and (following.startswith("▀") or status_row):
                return
        trial.read(0.1)
    raise AssertionError(f"missing complete composer {lines!r}:\n{trial.visible()}")


def steer_choice(trial):
    start_work(trial)
    trial.send(b"refine\r")
    trial.until_screen("Steer or queue")
    trial.send(b"\r\x1bOP-kept")
    trial.until_screen("refine-kept")
    assert "Sending" not in trial.visible()
    trial.send(b"\r\t\r")
    trial.until_screen("F5 accepts")
    trial.send(b"newer draft")
    trial.until_screen("newer draft")
    trial.send(b"\x1b[15~")
    trial.until_absent("F5 accepts")
    assert "newer draft" in trial.visible()
    exit_with_text(trial)


def expired_decision(trial):
    start_work(trial)
    trial.send(b"draft kept\x1bOR\x0f")  # F3 then Ctrl+O.
    trial.until_screen("Decision")
    trial.send(b"\x1bOR")
    trial.until_screen("unavailable")
    trial.resize(40, 12)
    trial.until_screen("unavailable")
    trial.send(b"\t\r\x1bOPx")
    trial.until_screen("draft keptx")
    assert "Sending" not in trial.visible()
    trial.resize(80, 24)
    trial.until_screen("draft keptx")
    exit_with_text(trial)


def held_queue_recovery(trial, stop=b"\x18"):
    start_work(trial)
    trial.send(b"queued follow-up\r\t\t\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[15~")
    trial.until_absent("F5 accepts")
    trial.send(b"fresh draft" + stop + b"\x0f")
    trial.until_screen("Recover text")
    trial.until_screen("Append to draft")
    trial.send(b"\r\x1bOP")  # No default action; dismiss explicitly.
    trial.until_absent("Recover text")
    trial.send(b"\x0f")
    trial.until_screen("Recover text")
    trial.send(b"\t\r")
    trial.until_absent("Recover text")
    assert_composer(trial, ["fresh draft", "queued follow-up"])
    # One undo must remove only the explicit append, not the newer draft.
    trial.send(b"\x1a!")
    trial.until_screen("fresh draft!")
    assert_composer(trial, ["fresh draft!"])
    first = next(i for i, row in enumerate(trial.screen.display) if row.strip() == "› fresh draft!")
    assert "queued follow-up" not in trial.screen.display[first + 1], trial.visible()
    exit_with_text(trial)


def project_switch_and_late_ack(trial):
    trial.until_screen("18% used")
    trial.send(b"Studio request\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x10Observatory draft")
    trial.until_screen("Observatory draft")
    trial.send(b"\x10\x1b[15~")
    trial.until_screen("Message received")
    trial.send(b"\x10")
    trial.until_screen("Observatory draft")
    trial.send(b"\x1a")
    trial.until_absent("Observatory draft")
    # Further typing remains local to this editor.
    trial.send(b"retained")
    trial.until_screen("retained")
    exit_with_text(trial)


def disconnect_reconcile_and_reset(trial):
    trial.until_screen("18% used")
    trial.send(b"uncertain request\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x1bOQnew draft")  # F2 disconnect; drafting continues.
    trial.until_screen("new draft")
    trial.send(b"\x1bOQ\x0f")
    trial.until_screen("Recover text")
    trial.until_screen("Append to draft")
    trial.send(b"\t\r")
    trial.until_absent("Recover text")
    assert_composer(trial, ["new draft", "uncertain request"])
    trial.send(b"\x1b[17~")  # F6 reset.
    trial.until_screen("Reset both projects")
    trial.send(b"\r\x1bOP-kept")
    trial.until_screen("uncertain request-kept")
    trial.send(b"\x1b[17~\t\r")
    trial.until_absent("Reset both projects")
    trial.until_absent("uncertain request")
    trial.until_screen("18% used")
    assert "uncertain request" not in trial.visible()
    trial.send(b"\x11")


def accepted_lost_ack(trial):
    trial.until_screen("18% used")
    trial.send(b"accepted request\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[18~")  # F7 loses the acknowledgement after acceptance.
    trial.until_screen("Disconnected · F2 reconnects")
    assert "Message received" not in trial.visible(), trial.visible()
    trial.send(b"independent draft")
    trial.until_screen("independent draft")
    trial.send(b"\x1bOQ")
    trial.until_absent("Disconnected · F2 reconnects")
    assert "Message received" in trial.visible()
    assert trial.visible().count("› accepted request") == 1, trial.visible()
    assert_composer(trial, ["independent draft"])
    exit_with_text(trial)


def steer_lost_ack(trial):
    start_work(trial)
    trial.send(b"steer exactly once\r\t\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[18~")
    trial.until_screen("Disconnected · F2 reconnects")
    assert "Steering instruction acknowledged" not in trial.visible(), trial.visible()
    trial.send(b"independent draft\x1bOQ")
    trial.until_screen("Steering instruction acknowledged")
    assert trial.visible().count("Steer  steer exactly once") == 1, trial.visible()
    assert_composer(trial, ["independent draft"])
    exit_with_text(trial)


def rejected_recovery(trial):
    trial.until_screen("18% used")
    trial.send(b"\x1bOSrejected original\r")  # F4 arms rejection.
    trial.until_screen("F5 accepts")
    trial.send(b"newer draft\x1b[15~")
    trial.until_absent("F5 accepts")
    trial.send(b"\x0f")
    trial.until_screen("Recover text")
    trial.send(b"\t\r")
    trial.until_absent("Recover text")
    assert_composer(trial, ["newer draft", "rejected original"])
    exit_with_text(trial)


def pending_inspector(trial):
    trial.until_screen("18% used")
    trial.send(b"\x1b[200~pending alpha\npending omega\x1b[201~\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x0f")
    trial.until_screen("Pending message:")
    trial.until_screen("pending omega")
    assert "pending alpha" in trial.visible()
    trial.send(b"\x1b[15~")
    trial.until_screen("Captured request resolved")
    trial.send(b"\r\x1bOPindependent draft")
    trial.until_screen("independent draft")
    assert_composer(trial, ["independent draft"])
    exit_with_text(trial)


def direct_shortcuts_and_tray(trial):
    start_work(trial)
    # Paste and a direct action can share a batch after a known task was painted.
    trial.send(b"\x1b[200~steer from direct key\x1b[201~\x13")
    trial.until_screen("F5 accepts")
    trial.send(b"queued next\x1b[15~")
    trial.until_screen("Steering instruction acknowledged")
    assert_composer(trial, ["queued next"])
    trial.send(b"\x14")
    trial.until_screen("F5 accepts")
    trial.send(b"independent draft\x1b[15~")
    trial.until_screen("Queued:")
    assert_composer(trial, ["independent draft"])
    trial.send(b"\x02")
    trial.until_screen("Studio · Message tray")
    trial.send(b"\r\t\r")  # Unselected Enter cannot inspect; Tab selects Queue first.
    trial.until_screen("Studio · Message 3 · Queue")
    trial.until_screen("queued next")
    trial.send(b"\x13\x14\r\x1b[15~")  # Focus consumes actions; F5 completes prerequisite.
    trial.until_screen("Running")
    assert "Studio · Message 3 · Queue" in trial.visible(), trial.visible()
    trial.send(b"\x1bOP")
    trial.until_absent("Studio · Message 3 · Queue")
    assert_composer(trial, ["independent draft"])
    trial.resize(40, 12)
    trial.until_screen("18%")
    assert_composer(trial, ["independent draft"])
    exit_with_text(trial)


def unavailable_direct_actions(trial):
    trial.until_screen("18% used")
    trial.send(b"idle draft\x13\x14")
    trial.until_screen("No presented active task")
    assert "F5 accepts" not in trial.visible(), trial.visible()
    assert_composer(trial, ["idle draft"])
    trial.send(b"\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[15~")
    trial.until_screen("Message received")
    trial.send(b"draft after completion\x1b[15~\x13\x14")
    trial.until_screen("18% used")
    # Help is an observable input barrier proving both shortcut bytes were consumed.
    trial.send(b"\x1bOP")
    trial.until_screen("Controls")
    trial.send(b"\x1bOP")
    trial.until_absent("Controls")
    assert "F5 accepts" not in trial.visible(), trial.visible()
    assert_composer(trial, ["draft after completion"])
    trial.send(b"\x16\x13\x16")  # Plain-paste capture cannot execute Steer.
    trial.until_screen("Finish paste")
    trial.send(b"\x1b")
    trial.until_absent("Finish paste")
    assert_composer(trial, ["draft after completion"])
    exit_with_text(trial)


def unknown_queue_tray(trial):
    start_work(trial)
    trial.send(b"queue exactly once\x14")
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[18~")
    trial.until_screen("Disconnected · F2 reconnects")
    trial.send(b"retained draft\x02\t\r")
    trial.until_screen("Studio · Message 2 · Queue")
    trial.until_screen("Unknown")
    assert "Follow-up queued" not in trial.visible(), trial.visible()
    trial.send(b"\x1bOQ")
    trial.until_screen("Queued")
    trial.until_absent("Unknown")
    trial.send(b"\r\x1bOP")
    trial.until_absent("Studio · Message 2 · Queue")
    trial.until_screen("Queued:")
    assert_composer(trial, ["retained draft"])
    exit_with_text(trial)


def compact_full_text(trial):
    trial.until_screen("18% used")
    text = "\n".join(["Captured full text", *[f"line {i} · 界面" for i in range(24)], "LAST CAPTURED LINE"])
    trial.send(b"\x1b[200~" + text.encode() + b"\x1b[201~\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[15~")
    trial.until_screen("Message received")
    trial.send(b"new draft\x02")
    trial.until_screen("Studio · Message 1 · Message")
    trial.resize(40, 12)
    trial.until_screen("Read only")
    trial.send(b"\x1b[6~" * 20)
    trial.until_screen("LAST CAPTURED LINE")
    trial.send(b"\x1b[15~\r")
    trial.until_screen("LAST CAPTURED LINE")
    trial.send(b"\x1bOP")
    trial.until_absent("Studio · Message 1 · Message")
    assert_composer(trial, ["new draft"])
    exit_with_text(trial)


def guided_tour(trial):
    trial.until_screen("Tour 1/10")
    # Bracketed contents cannot become navigation or quit keys. The subsequent
    # help view is a processing barrier; no timed sleep assumes input consumption.
    trial.send(b"\x1b[200~npqreal draft\x1b[201~\r\x1b[15~h")
    trial.until_screen("Tour help")
    trial.until_screen("Editing is locked")
    trial.send(b"h")
    trial.until_absent("Tour help")
    assert "Tour 1/10" in trial.visible(), trial.visible()
    assert "real draft" not in trial.visible(), trial.visible()
    trial.send(b"n")
    trial.until_screen("Tour 2/10")
    trial.until_screen("Steer")
    trial.until_screen("Queue")
    trial.resize(40, 12)
    trial.until_screen("Locked · H help Q quit")
    trial.until_background(0, 11, "default")
    trial.until_foreground(1, 11, "8fd3f4")
    trial.send(b"lh")
    trial.until_screen("Tour help")
    trial.until_background(0, 11, "default")
    trial.until_foreground(1, 11, "0e7490")
    trial.send(b"h")
    trial.until_absent("Tour help")
    for index in range(3, 11):
        trial.send(b"n")
        trial.until_screen(f"Tour {index}/10")
    trial.send(b"ph")
    trial.until_screen("Tour help")
    trial.send(b"h")
    trial.until_absent("Tour help")
    assert "Tour 9/10" in trial.visible(), trial.visible()
    trial.send(b"r")
    trial.until_screen("Tour 1/10")
    trial.send(b"q")


def interrupted_tour(trial):
    trial.until_screen("Tour 1/10")
    match = re.search(rb"__ASURA_TEST_PID__(\d+)", trial.output)
    assert match, "missing child pid"
    os.kill(int(match.group(1)), signal.SIGTERM)


def key_inspection(trial):
    trial.until_screen("KEYS · no submission")
    for index, (encoding, modifier) in enumerate([
        (b"\r", "NONE"),
        (b"\x1b[13;5u", "CONTROL"),
        (b"\x1b[13;2u", "SHIFT"),
        (b"\x1b[13;3u", "ALT"),
        (b"\x1b[13;9u", "SUPER"),
        (b"\x1b[13;5:2u", "CONTROL"),
    ], 1):
        trial.send(encoding)
        kind = "Repeat" if index == 6 else "Press"
        trial.until_screen(f"#{index} code=Enter kind={kind}")
        trial.until_screen(f"modifiers={modifier}")
    # Current decoder discards this unsupported xterm encoding; the next valid
    # key is a processing barrier. No-event does not prove emulator interception.
    trial.send(b"\x1b[27;5;13~x")
    trial.until_screen("#7 code=Char('x') kind=Press")
    trial.send(b"\x1b[200~private content /help \x13\x14\x1b[201~")
    trial.until_screen("#8 Paste ignored")
    assert "private content" not in trial.visible(), trial.visible()
    assert "Message received" not in trial.visible(), trial.visible()
    trial.resize(30, 8)
    trial.until_screen("#8 Paste ignored")
    trial.send(b"\x11")


def interrupted_keys(trial):
    trial.until_screen("KEYS · no submission")
    match = re.search(rb"__ASURA_TEST_PID__(\d+)", trial.output)
    assert match, "missing child pid"
    os.kill(int(match.group(1)), signal.SIGTERM)


def minimal_composer(trial):
    trial.until_screen("18% used")

    def assert_minimal():
        # Writes can arrive in partial frames; inspect the complete idle composer.
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            rows = trial.screen.display
            footer = rows[-1].strip()
            if (footer.startswith("Studio") and "18% used" in footer
                    and not any(frame in footer for frame in "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
                    and rows[-4].strip() == "▄" * trial.screen.columns):
                assert "Studio / Conversation 1" not in trial.visible(), trial.visible()
                assert "Message tray" not in trial.visible(), trial.visible()
                assert "Ready to send" not in trial.visible(), trial.visible()
                assert " chars" not in footer, trial.visible()
                return
            trial.read(0.1)
        raise AssertionError(trial.visible())

    assert_minimal()
    trial.send(b"retained history\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[15~")
    trial.until_screen("Message received")
    trial.send(b"\x1b[15~")
    trial.until_screen("Work completed successfully")
    assert_minimal()
    trial.send(b"\x02")
    trial.until_screen("Studio · Message 1 · Message")
    trial.until_screen("Completed")
    trial.until_screen("retained history")
    trial.send(b"\x1bOP")
    trial.until_absent("Studio · Message 1 · Message")
    trial.until_screen("18% used")
    assert_minimal()
    trial.send(b"\x11")


def styled_history(trial, light):
    trial.until_screen("18% used · demo v1 fast")
    footer = trial.screen.display[-1]
    assert "Studio · ~/src/studio · main · +24-8" in footer, footer
    assert not any(ch in footer for ch in "|[]"), footer
    for value, color in [
        ("Studio", "0e7490" if light else "8fd3f4"),
        ("+24", "15803d" if light else "86efac"),
        ("-8", "b91c1c" if light else "fca5a5"),
    ]:
        start = footer.index(value)
        for x in range(start, start + len(value)):
            trial.until_foreground(x, trial.screen.lines - 1, color)
        next_color = (("b91c1c" if light else "fca5a5") if value == "+24"
                      else ("475569" if light else "86aec8"))
        trial.until_foreground(start + len(value), trial.screen.lines - 1, next_color)
    trial.until_background(1, 2, "default")
    trial.until_background(1, trial.screen.lines - 1, "default")
    trial.send(b"\x1b[200~history styling\n\nlast line\x1b[201~\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[15~")
    trial.until_screen("Message received")
    trial.until_screen("history styling")
    user_color = "d4deea" if light else "1c2e3d"
    base_color = "default"
    for width, height in [(80, 24), (40, 20)]:
        trial.resize(width, height)
        # Wait for the complete user row and both colored edges after resizing.
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            lines = trial.screen.display
            rows = [y for y, line in enumerate(lines) if "history styling" in line]
            if rows:
                y = rows[0]
                assert "You" not in lines[y], lines[y]
                if all(trial.screen.buffer[y][x].bg == user_color for x in [0, width - 1]):
                    break
            trial.read(0.1)
        else:
            raise AssertionError(trial.visible())
        assert trial.screen.buffer[y][1].data == "›", trial.visible()
        for content_y in range(y, y + 3):
            trial.until_background(0, content_y, user_color)
            trial.until_background(width - 1, content_y, user_color)
        fixture_y = next(y for y, line in enumerate(trial.screen.display) if "Message received" in line)
        trial.until_background(1, fixture_y, base_color)
    trial.send(b"\x11")


def project_status(trial):
    frames = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"

    def footer_when(predicate):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            footer = trial.screen.display[-1].strip()
            if predicate(footer):
                return footer
            trial.read(0.1)
        raise AssertionError(trial.visible())

    footer = footer_when(lambda row: "18% used · demo v1 fast" in row)
    assert "Studio · ~/src/studio · main · +24-8" in footer, footer
    assert "chars" not in footer and not any(frame in footer for frame in frames), footer
    trial.send(b"\x1b[200~" + "e\u0301 👩🏽‍💻\n界".encode() + b"\x1b[201~")
    footer_when(lambda row: "5 chars" in row)
    trial.send(b"\r")
    trial.until_screen("F5 accepts")
    footer_when(lambda row: "chars" not in row and not any(frame in row for frame in frames))
    trial.send(b"\x1b[15~")
    footer_when(lambda row: any(frame in row for frame in frames))
    trial.send(b"new")
    footer_when(lambda row: "3 chars" in row and any(frame in row for frame in frames))
    trial.send(b"\x10")
    footer_when(lambda row: "Observatory" in row and "42% used · demo v2" in row)
    trial.send(b"\x10\x1bOQ")
    footer_when(lambda row: "?% used" in row and "3 chars" in row and not any(frame in row for frame in frames))
    trial.until_screen("Disconnected · F2 reconnects")
    trial.send(b"\x1bOQ\x1b[15~")
    footer_when(lambda row: "18% used" in row and "3 chars" in row and not any(frame in row for frame in frames))
    trial.resize(30, 8)
    footer = footer_when(lambda row: "3c" in row and "18%" in row)
    assert len(footer) <= 28, footer
    trial.resize(120, 40)
    footer_when(lambda row: "Studio · ~/src/studio · main · +24-8 · 3 chars" in row)
    exit_with_text(trial)


def command_completion_and_exit(trial):
    trial.until_screen("18% used")
    trial.send(b"\x1b[20~")  # F9 opens discovery from an empty draft.
    trial.until_screen("Commands")
    trial.send(b"\r")  # No default choice and no execution.
    assert trial.process.poll() is None
    trial.send(b"\t\t")  # First Tab selects, second completes.
    trial.until_absent("Commands")
    trial.until_screen("/quit")
    assert trial.process.poll() is None
    trial.send(b"\r")


def command_help_view(trial):
    trial.until_screen("18% used")
    trial.send(b"\x1b[20~")  # F9 opens an unselected browser.
    trial.until_screen("Commands")
    trial.send(b"\x1b[B\x1b[B")  # Select the second local built-in.
    trial.until_screen("/help")
    trial.send(b"\t")  # Completion alone does not open help.
    trial.until_absent("Commands")
    assert_composer(trial, ["/help"])
    trial.send(b"\r")
    trial.until_screen("Controls")
    assert "F5 accepts" not in trial.visible(), trial.visible()
    trial.send(b"\x1bOP")  # F1 closes the same controls view.
    trial.until_absent("Controls")
    assert any(row.strip() == "›" for row in trial.screen.display), trial.visible()
    trial.send(b"/help topic\r")
    trial.until_screen("Press Enter again")
    trial.send(b"\r")
    trial.until_screen("takes no arguments")
    assert_composer(trial, ["/help topic"])
    trial.send(b"\x0c\t\r")  # Clear the invalid command explicitly.
    trial.send(b"/ext:checks/help\r")  # Qualified work cannot take the local route.
    trial.until_screen("Unknown")
    assert_composer(trial, ["/ext:checks/help"])
    exit_with_text(trial)


def command_help_during_disconnected_work(trial):
    start_work(trial)
    trial.send(b"\x1bOQ")  # F2 disconnects this project.
    trial.until_screen("Disconnected")
    trial.send(b"\x1b[21~")  # F10 revokes synthetic command sources.
    trial.send(b"/help")
    assert_composer(trial, ["/help"])
    trial.send(b"\r")
    trial.until_screen("Controls")
    assert "Steer or queue" not in trial.visible(), trial.visible()
    trial.send(b"\x1bOP")
    trial.until_absent("Controls")
    trial.send(b"\x11")


def command_literal_and_unknown(trial):
    trial.until_screen("18% used")
    trial.send(b"//tmp/file\r")
    trial.until_screen("Unknown")
    assert_composer(trial, ["//tmp/file"])
    trial.send(b"\x0c\t\r")  # Explicitly clear the rejected command.
    trial.send(b"/extension:checks/test note\r")
    trial.until_screen("Unknown")
    assert_composer(trial, ["/extension:checks/test note"])
    assert "F5 accepts" not in trial.visible(), trial.visible()
    trial.send(b"\x0c\t\r")
    trial.send(b"'/tmp/file\t")  # A leading quote disables command completion.
    trial.until_screen("12 chars")  # Ordinary Tab inserted two spaces.
    assert_composer(trial, ["'/tmp/file"])
    trial.send(b"\r")
    trial.until_screen("F5 accepts")
    assert "Unknown" not in trial.visible()
    trial.send(b"\x1b[15~")
    trial.until_screen("Message received")
    trial.send(b"\x11")


def command_source_change(trial):
    trial.until_screen("18% used")
    trial.send(b"/ext:ch\t")
    trial.until_screen("/ext:checks/test")
    trial.send(b" check this")
    assert_composer(trial, ["/ext:checks/test check this"])
    trial.send(b"\x1b[21~")  # F10 revokes this project's synthetic sources.
    trial.send(b"\r")
    trial.until_screen("unavailable")
    assert_composer(trial, ["/ext:checks/test check this"])
    assert "F5 accepts" not in trial.visible(), trial.visible()
    trial.send(b"\x1b[21~")  # F10 restores new source revisions.
    trial.send(b"\r")
    trial.until_screen("Press Enter again")  # First Enter was not painted as a binding.
    trial.send(b"\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[15~")
    trial.until_screen("Message received")
    trial.send(b"\x11")


def command_active_choice_and_preacceptance_revoke(trial):
    start_work(trial)
    trial.send(b"/skill:project/review review this")
    assert_composer(trial, ["/skill:project/review review this"])
    trial.until_screen("Enter command")
    trial.send(b"\r")
    trial.until_screen("Steer or queue")
    trial.send(b"\r")  # No default action.
    assert "F5 accepts" not in trial.visible(), trial.visible()
    trial.send(b"\t\r")  # Skill Steer is unavailable.
    assert "F5 accepts" not in trial.visible(), trial.visible()
    trial.send(b"\t\r")  # Queue the captured command.
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[21~\x1b[15~")  # Revoke source before F5 accepts.
    trial.until_absent("F5 accepts")
    trial.send(b"\x0f")
    trial.until_screen("Recover text")
    trial.until_screen("/skill:project/review review this")
    trial.send(b"\x1b")
    trial.until_absent("Recover text")
    trial.send(b"\x11")
    trial.until_screen("Unsaved")
    trial.send(b"\t\r")


def command_accepted_queue_revoke(trial, command="/ext:checks/test"):
    start_work(trial)
    raw = f"{command} check this"
    trial.send(raw.encode())
    assert_composer(trial, [raw])
    trial.until_screen("Enter command")
    trial.send(b"\r")
    trial.until_screen("Steer or queue")
    trial.send(b"\t\t\r")  # Select Queue.
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[15~")  # Accept while current work is still active.
    trial.until_screen("Queued:")
    trial.send(b"\x1b[21~")
    trial.until_screen("held")
    trial.send(b"\x0f")
    trial.until_screen("Recover text")
    trial.until_screen(raw)
    trial.send(b"\x1b")
    trial.until_absent("Recover text")
    trial.send(b"\x11")
    trial.until_screen("Unsaved")
    trial.send(b"\t\r")


def command_reconnect_keeps_newer_project_drafts(trial, command="/ext:checks/test"):
    trial.until_screen("18% used")
    raw = f"{command} reconcile once"
    trial.send(raw.encode())
    assert_composer(trial, [raw])
    trial.until_screen("Enter command")
    trial.send(b"\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[18~")  # F7 records acceptance but loses acknowledgement.
    trial.until_screen("Disconnected · F2 reconnects")
    trial.send(b"Studio newer draft")
    trial.until_screen("Studio newer draft")
    trial.send(b"\x10Observatory draft")
    trial.until_screen("Observatory draft")
    trial.send(b"\x10\x1bOQ")  # Return and reconcile the original identity.
    trial.until_screen("Message received")
    assert trial.visible().count(raw) == 1, trial.visible()
    assert_composer(trial, ["Studio newer draft"])
    trial.send(b"\x10")
    assert_composer(trial, ["Observatory draft"])
    exit_with_text(trial)


def first_party_extension_path(trial):
    trial.until_screen("18% used")
    trial.send(b"\x1b[20~")  # Discovery shows the supplying origin.
    trial.until_screen("/ext:core/check")
    trial.until_screen("Asura")
    trial.send(b"\x1b")
    trial.until_absent("Commands")
    trial.send(b"/ext:co\t")
    trial.until_screen("/ext:core/check")
    trial.send(b" note\r")
    trial.until_screen("Press Enter again")
    trial.send(b"\r")
    trial.until_screen("F5 accepts")
    trial.send(b"\x1b[21~\x1b[15~")  # Revoke before acceptance.
    trial.until_absent("F5 accepts")
    trial.send(b"\x0f")
    trial.until_screen("Recover text")
    trial.until_screen("/ext:core/check note")
    trial.send(b"\x1b")
    trial.until_absent("Recover text")
    trial.send(b"\x11")
    trial.until_screen("Unsaved")
    trial.send(b"\t\r")


def old_first_party_name_is_unknown(trial):
    trial.until_screen("18% used")
    trial.send(b"/ext:asura/check note\r")
    trial.until_screen("Unknown")
    assert_composer(trial, ["/ext:asura/check note"])
    assert "F5 accepts" not in trial.visible(), trial.visible()
    exit_with_text(trial)


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: terminal_e2e.py path/to/asura-tui-chat")
    binary = str(Path(sys.argv[1]).resolve(strict=True))
    decoder_wide_overwrite()
    print("PASS PTY decoder preserves wide cells and repairs narrow overwrite")
    for arguments in [(), ("--keys",)]:
        pipe = subprocess.run([binary, *arguments], input=b"", stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, timeout=5)
        assert pipe.returncode != 0 and b"terminal stdin and stdout" in pipe.stderr
        assert b"\x1b" not in pipe.stdout, "non-TTY mode changed terminal state"
    print("PASS TP5/KI3 non-TTY rejection before mode changes")
    for arguments in [("--fault=tour",), ("--tour", "--unknown"),
                      ("--keys", "--tour"), ("--keys", "--fault=tour")]:
        invalid = subprocess.run([binary, *arguments], input=b"", stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, timeout=5)
        assert invalid.returncode != 0 and b"\x1b" not in invalid.stdout
        assert any(message in invalid.stderr for message in
                   [b"requires --tour", b"Unknown argument", b"Cannot combine --keys and --tour"])
    print("PASS GT3/KI3 invalid surface arguments rejected before terminal setup")
    run_case(binary, "TP5 normal exit restores terminal", exit_empty)
    run_case(binary, "TP2 multiline paste and no-default exit", paste_draft, arguments=("--manual",))
    run_case(binary, "TP2 Option+Return, inert Ctrl+J and delayed acknowledgement preserve draft", newline_and_acknowledgement, arguments=("--manual",))
    run_case(binary, "TP3 no-default Steer and independent new draft", steer_choice, arguments=("--manual",))
    run_case(binary, "TP3 expired decision and compact resize retain focus", expired_decision, arguments=("--manual",))
    run_case(binary, "TP4 cancellation holds queued text for explicit append", held_queue_recovery, arguments=("--manual",))
    run_case(binary, "TP4 failure holds queued text for explicit append", lambda trial: held_queue_recovery(trial, b"\x1b[19~"), arguments=("--manual",))
    run_case(binary, "TP4 project switch retains draft and undo across late ack", project_switch_and_late_ack, arguments=("--manual",))
    run_case(binary, "TP4 reconnect reconciliation and explicit reset", disconnect_reconcile_and_reset, arguments=("--manual",))
    run_case(binary, "TP4 accepted lost acknowledgement reconciles without retry", accepted_lost_ack, arguments=("--manual",))
    run_case(binary, "TP4 lost Steer acknowledgement is not shown applied before reconnect", steer_lost_ack, arguments=("--manual",))
    run_case(binary, "TP4 rejection preserves original and newer drafts", rejected_recovery, arguments=("--manual",))
    run_case(binary, "TP4 pending inspector retains text and resolved focus", pending_inspector, arguments=("--manual",))
    run_case(binary, "CP1/CP2 direct Steer and Queue, read-only tray and newer draft", direct_shortcuts_and_tray, arguments=("--manual",))
    run_case(binary, "CP1 idle and ended direct actions preserve text; paste cannot submit", unavailable_direct_actions, arguments=("--manual",))
    run_case(binary, "CP2 unknown Queue reconciles in tray without another request", unknown_queue_tray, arguments=("--manual",))
    run_case(binary, "CP3 compact full-text inspection scrolls and retains focus", compact_full_text, arguments=("--manual",))
    run_case(binary, "CP6 idle and settled composer stays minimal; history remains inspectable", minimal_composer, arguments=("--manual",))
    run_case(binary, "PS1/PS2/PS3 project metadata, Unicode count, activity, switch and reconnect", project_status, arguments=("--manual",))
    run_case(binary, "CT1 F9 selection, completion and local exit restore terminal", command_completion_and_exit, arguments=("--manual",))
    run_case(binary, "CT10 /help browser completion, local view and argument rejection", command_help_view, arguments=("--manual",))
    run_case(binary, "CT10 /help during disconnected work and revoked sources", command_help_during_disconnected_work, arguments=("--manual",))
    run_case(binary, "CT1D/CT1E/CT8 quoted path literal and old extension prefix unknown", command_literal_and_unknown, arguments=("--manual",))
    run_case(binary, "CT5A/CT8 short extension completion and F10 revocation", command_source_change, arguments=("--manual",))
    run_case(binary, "CT4/CT5B active Skill choice and pre-acceptance revoke preserve text", command_active_choice_and_preacceptance_revoke, arguments=("--manual",))
    run_case(binary, "CT5C accepted queued command is held after source revoke", command_accepted_queue_revoke, arguments=("--manual",))
    run_case(binary, "CT6 command reconnect keeps original identity and both newer drafts", command_reconnect_keeps_newer_project_drafts, arguments=("--manual",))
    run_case(binary, "CT9 first-party extension discovery, completion and protected revoke", first_party_extension_path, arguments=("--manual",))
    run_case(binary, "CT9F old first-party source name is unknown and retained", old_first_party_name_is_unknown, arguments=("--manual",))
    run_case(binary, "CT9 accepted first-party Queue held after source revoke",
             lambda trial: command_accepted_queue_revoke(trial, "/ext:core/check"), arguments=("--manual",))
    run_case(binary, "CT9 first-party lost ack keeps identity and both newer drafts",
             lambda trial: command_reconnect_keeps_newer_project_drafts(trial, "/ext:core/check"), arguments=("--manual",))
    run_case(binary, "GT1/GT2 guided scenes, locked input, palette, resize and revisit", guided_tour,
             arguments=("--tour", "--manual"), color=True)
    run_case(binary, "GT3 tour failure restores terminal", lambda _: None,
             arguments=("--tour", "--fault=tour"), expected=1)
    run_case(binary, "GT3 tour SIGTERM restores terminal", interrupted_tour, arguments=("--tour",))
    run_case(binary, "KI1/KI2 decoded Return modifiers and redacted paste stay inert", key_inspection,
             arguments=("--keys", "--manual"))
    run_case(binary, "KI3 key inspection SIGTERM restores terminal", interrupted_keys,
             arguments=("--keys",))
    run_case(binary, "KI3 key inspection draw failure restores terminal", lambda _: None,
             arguments=("--keys", "--fault=draw"), expected=1)
    for light in [False, True]:
        arguments = ("--manual", "--light") if light else ("--manual",)
        run_case(binary, f"TS1/TS2 user history bands and output separation ({'light' if light else 'dark'})",
                 lambda trial, theme=light: styled_history(trial, theme), arguments=arguments, color=True)
    for fault, expected in [("setup", 1), ("draw", 1), ("panic", 101)]:
        run_case(binary, f"TP5 {fault} failure restores terminal", lambda _: None,
                 arguments=(f"--fault={fault}",), expected=expected)
    for sig in [signal.SIGINT, signal.SIGTERM, signal.SIGHUP]:
        run_case(binary, f"TP5 {sig.name} restores terminal", lambda trial, s=sig: interrupt(trial, s))


if __name__ == "__main__":
    main()
