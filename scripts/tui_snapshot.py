#!/usr/bin/env python3
"""Headless TUI driver: run mastoot in a pty, emulate the terminal with
pyte, and dump the screen as text after each scripted step.

Needs `pip install pyte`. Runs the release binary against the real
account, so the first launch of a freshly built binary triggers the
macOS Keychain prompt: click "Always Allow", or sign the binary with
scripts/codesign-dev.sh so the ACL survives rebuilds.

usage: scripts/tui_snapshot.py <cols> <rows> step...   where step is one of
  until <text>       pump until some screen line contains <text> (max 900 s)
  wait <secs>        pump output for N seconds
  key <text>         send bytes (python escapes allowed, e.g. \\x1b, \\r)
  dump <name>        write current screen to <name>.txt next to this script

MASTOOT_ARGS adds CLI args (e.g. "--config /tmp/cfg.toml");
TUI_OUT picks the directory for dumps (default: this script's folder).

example:
  scripts/tui_snapshot.py 110 34 'until ● home' 'key /' 'key rust' 'key \\r' 'wait 5' 'dump search'
"""
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

import pyte

HERE = os.environ.get("TUI_OUT", os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
cols, rows = int(sys.argv[1]), int(sys.argv[2])
steps = sys.argv[3:]

pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    os.environ["TERM"] = "xterm-256color"
    os.environ.pop("TERM_PROGRAM", None)
    os.chdir(REPO)
    extra = os.environ.get("MASTOOT_ARGS", "").split()
    os.execvp("./target/release/mastoot", ["mastoot", "-v"] + extra)


class AnsweringScreen(pyte.Screen):
    """pyte answers DSR (`CSI 5n`) / DA (`CSI c`) through
    write_process_input, which is a no-op by default. Wire it to the
    pty so terminal-capability probes get a reply like a real
    terminal would (otherwise ratatui-image's probe thread stays
    blocked on stdin and eats the next key press)."""

    def write_process_input(self, data):
        os.write(fd, data.encode("utf-8"))


screen = AnsweringScreen(cols, rows)
stream = pyte.ByteStream(screen)


def pump(secs):
    end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                data = os.read(fd, 1 << 16)
            except OSError:
                return False
            if not data:
                return False
            stream.feed(data)
    return True


for step in steps:
    op, _, arg = step.partition(" ")
    if op == "wait":
        pump(float(arg))
    elif op == "until":
        deadline = time.time() + 900
        while time.time() < deadline:
            pump(0.5)
            if any(arg in line for line in screen.display):
                break
    elif op == "key":
        os.write(fd, arg.encode("utf-8").decode("unicode_escape").encode("utf-8"))
        pump(0.3)
    elif op == "dump":
        path = os.path.join(HERE, arg + ".txt")
        with open(path, "w") as f:
            f.write("\n".join(line.rstrip() for line in screen.display))
        print(f"dumped {path}")

try:
    os.kill(pid, signal.SIGTERM)
except ProcessLookupError:
    pass
