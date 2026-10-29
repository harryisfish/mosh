#!/usr/bin/env python3
"""Exercise the Rust binaries through real local PTYs and optional impaired UDP."""

import argparse
import fcntl
import heapq
import json
import os
from pathlib import Path
import pty
import random
import re
import shlex
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time
import tty


RAW_NETWORK_INPUT = b"impaired-" + b"x" * 8192 + b"\r"
DIAGNOSTIC_FILES = set()


def exact(data, timeout=5):
    received = bytearray()
    while len(received) < len(data):
        assert select.select([0], [], [], timeout)[0], ("remote input timed out", bytes(received), data)
        chunk = os.read(0, len(data) - len(received))
        if not chunk:
            raise RuntimeError("remote input ended early")
        received.extend(chunk)
    assert bytes(received) == data, (bytes(received), data)


def remote_fixture(mode, state):
    tty.setraw(0)
    Path(state).write_text(json.dumps({"command": os.getpid(), "worker": os.getppid()}))
    if mode == "wait":
        os.write(1, b"\x1b[2J\x1b[HWAIT_READY")
        while os.read(0, 1):
            pass
        return 0
    if mode == "diagnostic-idle":
        os.write(1, b"DIAGNOSTIC_IDLE_READY\r\n")
        exact(b"finish\r")
        return 0
    if mode == "diagnostic-flood":
        os.write(1, b"DIAGNOSTIC_FLOOD_READY\r\n")
        exact(b"start\r")
        received = bytearray()
        counter = 0
        while b"stop\r" not in received:
            if select.select([0], [], [], 0)[0]:
                received.extend(os.read(0, 4096))
            os.write(1, (f"{counter:016x} " + "D" * 64 + "\r\n").encode() * 8)
            counter += 1
            time.sleep(0.001)
        os.write(1, b"\r\nDIAGNOSTIC_FLOOD_DONE\r\n")
        return 0
    if mode == "job":
        os.write(1, b"\x1b[2J\x1b[H\x1b[?5h\x1b]2;JOB_TITLE\x1b\\\x1b]7;file:///tmp/mosh-job\x1b\\"
                    b"\x1b]133;A\x1b\\\x07\x1b]52;c;am9i\x1b\\"
                    b"JOB_CACHED_TEXT\r\n\x1b_Ga=T,f=32,s=1,v=1,i=9,c=1,r=1,C=1,q=2;/wAA/w==\x1b\\\r\nJOB_READY")
        exact(b"\x1a")
        os.write(1, b"\r\nREMOTE_CTRL_Z_OK")
        for stage in [b"first", b"second", b"third", b"external", b"final"]:
            exact(stage + b"\r", timeout=60)
            os.write(1, b"\r\nJOB_" + stage.upper() + b"_OK")
        return 9
    if mode == "network":
        os.write(1, b"\x1b[2J\x1b[HNETWORK_READY\r\n")
        exact(b"baseline\r")
        os.write(1, b"NETWORK_BASELINE_OK\r\n")
        exact(RAW_NETWORK_INPUT, timeout=60)
        os.write(1, b"NETWORK_BURST_OK\r\n")
        exact(b"resume-once\r", timeout=60)
        os.write(1, b"\r\nNETWORK_ONCE_OK\r\n")
        return 9
    assert sys.argv[4] == "literal ' quotes $HOME $(printf unsafe) ; &"
    os.write(1, b"\x1b[2J\x1b[HDEFAULT_COLOR_CLI\x1b[40mBLACK_COLOR_CLI\x1b[49mRESTORED_COLOR_CLI\r\n"
                b"\x1b[?1h\x1b[?2004h\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[>31u\x1b[38;2;12;34;56mREADY_CLI\x1b[0m " + "e\u0301中文".encode())
    exact(b"\x1b[200~paste \xe4\xb8\xad\xe6\x96\x87\x1b[201~")
    os.write(1, b"\r\nPASTE_CLI_OK\r\n")
    exact(b"\x1b[97:65:97;5:2;65:98u")
    os.write(1, b"KEY_CLI_OK\r\n")
    exact(b"\x1b[1;1:1A\x1b[1;1:2A\x1b[1;1:3A")
    os.write(1, b"ARROW_CLI_OK\r\n")
    exact(b"\x1b[<0;3;2M")
    exact(b"\x1b[<0;3;2m")
    exact(b"\x1b[<0;1;1M\x1b[<0;1;1m")
    exact(b"\x1b[<66;3;2M")
    exact(b"\x1b[O\x1b[I")
    os.write(1, b"\x1b[<u\r\nINPUT_CLI_OK\r\n")
    exact(b"\n\x08")
    exact(b"\r")
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        rows, cols, _, _ = struct.unpack("HHHH", fcntl.ioctl(0, termios.TIOCGWINSZ, bytes(8)))
        if (rows, cols) == (12, 60):
            break
        time.sleep(0.01)
    assert (rows, cols) == (12, 60), (rows, cols)
    os.write(1, b"RESIZE_CLI_OK\r\n\x1b]52;c;cmVtb3Rl\x1b\\\x1b]52;c;?\x1b\\")
    exact(b"\x1b]52;c;bG9jYWw=\x1b\\")
    os.write(1, b"CLIPBOARD_CLI_OK\r\n")
    return 7


def proxy_bootstrap(server, state):
    request = json.loads(sys.stdin.buffer.readline())
    request["bind"] = "127.0.0.1"
    request = (json.dumps(request) + "\n").encode()
    diagnostic_args = ["--diagnostics"] if "--diagnostics" in sys.argv else []
    result = subprocess.run([server, "bootstrap", *diagnostic_args], input=request,
                            stdout=subprocess.PIPE, check=True)
    grant = json.loads(result.stdout)
    front = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    front.bind(("127.0.0.1", 0))
    back = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    back.bind(("127.0.0.1", 0))
    destination = ("127.0.0.1", grant["port"])
    grant["address"] = "127.0.0.1"
    grant["port"] = front.getsockname()[1]
    pid = os.fork()
    if pid == 0:
        os.setsid()
        null = os.open(os.devnull, os.O_RDWR)
        for fd in (0, 1, 2):
            os.dup2(null, fd)
        peer = None
        rng = random.Random(12345)
        queue = []
        queued_bytes = 0
        serial = 0
        last_forwarded = [0, 0]
        profile = {}
        refresh = 0
        stats = {"received": 0, "dropped": 0, "duplicated": 0, "forwarded": 0,
                 "reordered": 0, "overflow": 0}
        while True:
            now = time.monotonic()
            if now >= refresh:
                try:
                    profile = json.loads(Path(state + ".profile").read_text())
                except FileNotFoundError:
                    profile = {}
                stats["profile"] = profile
                stats["queued"] = len(queue)
                updated = Path(state + ".stats.new")
                updated.write_text(json.dumps(stats))
                updated.replace(state + ".stats")
                refresh = now + 0.05
            if Path(state + ".drop").exists():
                queue.clear()
                queued_bytes = 0
            while queue and queue[0][0] <= now:
                _, order, to_front, packet, target = heapq.heappop(queue)
                queued_bytes -= len(packet)
                (front if to_front else back).sendto(packet, target)
                stats["forwarded"] += 1
                direction = int(to_front)
                stats["reordered"] += int(order < last_forwarded[direction])
                last_forwarded[direction] = max(order, last_forwarded[direction])
            for source in select.select([front, back], [], [], 0.01)[0]:
                packet, address = source.recvfrom(65536)
                stats["received"] += 1
                if source is front:
                    peer = address
                target = peer if source is back else destination
                if target is None:
                    continue
                if Path(state + ".drop").exists() or rng.random() < profile.get("loss", 0):
                    stats["dropped"] += 1
                    continue
                delay = max(0, profile.get("delay_ms", 0)
                            + rng.uniform(-profile.get("jitter_ms", 0), profile.get("jitter_ms", 0))) / 1000
                duplicate = rng.random() < profile.get("duplicate", 0)
                for copy in range(1 + int(duplicate)):
                    if len(queue) >= 4096 or queued_bytes + len(packet) > 8 * 1024 * 1024:
                        stats["overflow"] += 1
                        continue
                    serial += 1
                    heapq.heappush(queue, (now + delay + copy * 0.007, serial,
                                           source is back, packet, target))
                    queued_bytes += len(packet)
                    stats["duplicated"] += copy
    Path(state + ".proxy").write_text(str(pid))
    print(json.dumps(grant), flush=True)


class TerminalProcess:
    def __init__(self, command, extra_env=None):
        self.master, self.slave = pty.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 800, 480))
        self.saved = termios.tcgetattr(self.slave)
        self.output = bytearray()
        self.closing = False
        self.diagnostic_files = set()
        environment = dict(os.environ, TERM="xterm-kitty", COLORTERM="truecolor")
        environment.update(extra_env or {})

        def session():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        self.process = subprocess.Popen(command, stdin=self.slave, stdout=self.slave,
                                        stderr=self.slave, env=environment, preexec_fn=session)

    def send(self, data):
        pending = memoryview(data)
        deadline = time.monotonic() + 30
        os.set_blocking(self.master, False)
        try:
            while pending:
                try:
                    written = os.write(self.master, pending[:4096])
                    pending = pending[written:]
                except BlockingIOError:
                    pass
                self.pump(0.01)
                if pending:
                    assert self.process.poll() is None, (self.process.returncode, bytes(self.output[-4000:]))
                    assert time.monotonic() < deadline, "terminal input remained blocked"
        finally:
            os.set_blocking(self.master, True)

    def pump(self, duration=0.05):
        if select.select([self.master], [], [], duration)[0]:
            try:
                data = os.read(self.master, 65536)
            except OSError:
                data = b""
            self.output.extend(data)
            for match in re.finditer(rb"mosh-rs(?:-server)?: diagnostics: ([^\r\n]+)[\r\n]", self.output[-8192:]):
                self.diagnostic_files.add(Path(os.fsdecode(match[1])))
            DIAGNOSTIC_FILES.update(self.diagnostic_files)
        if self.closing:
            self.output = self.output[-4096:]
        else:
            assert len(self.output) < 4 * 1024 * 1024, f"CLI output exceeded fixture limit: {bytes(self.output[-1000:])!r}"

    def until(self, marker, timeout=15, start=0):
        deadline = time.monotonic() + timeout
        while marker not in self.output[start:]:
            self.pump()
            if self.process.poll() is not None:
                raise AssertionError(f"CLI exited {self.process.returncode} before {marker!r}: {bytes(self.output[-4000:])!r}")
            if time.monotonic() > deadline:
                raise AssertionError(f"CLI timed out before {marker!r}: {bytes(self.output[-4000:])!r}")

    def finish(self, code, timeout=10):
        deadline = time.monotonic() + timeout
        while self.process.poll() is None:
            self.pump()
            assert time.monotonic() < deadline, bytes(self.output[-4000:])
        self.pump(0)
        assert self.process.returncode == code, (self.process.returncode, bytes(self.output[-4000:]))
        assert termios.tcgetattr(self.master) == self.saved, "local termios was not restored"
        assert b"\x1b[?1049l" in self.output, "alternate screen was not restored"

    def close(self):
        self.closing = True
        if self.process.poll() is None:
            self.process.terminate()
            deadline = time.monotonic() + 3
            while self.process.poll() is None and time.monotonic() < deadline:
                self.pump()
            if self.process.poll() is None:
                self.process.kill()
                deadline = time.monotonic() + 3
                while self.process.poll() is None and time.monotonic() < deadline:
                    self.pump()
        os.close(self.master)
        os.close(self.slave)


def cleanup(state):
    if Path(state).exists():
        for pid in json.loads(Path(state).read_text()).values():
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    if Path(state + ".proxy").exists():
        try:
            os.kill(int(Path(state + ".proxy").read_text()), signal.SIGTERM)
        except ProcessLookupError:
            pass


def verify_job_control(command, state, inspect_session=None, cleanup_session=cleanup):
    shell_path = "/bin/zsh" if Path("/bin/zsh").exists() else "/bin/bash"
    shell = TerminalProcess([shell_path, "-f", "-i"] if shell_path.endswith("zsh")
                            else [shell_path, "--noprofile", "--norc", "-i"])
    prompt = b"LOCAL_JOB_PROMPT> "
    client_pid = None
    if inspect_session is None:
        inspect_session = lambda: json.loads(Path(state).read_text())
    try:
        shell.send(b"PS1='LOCAL_JOB_''PROMPT> '; unset PROMPT_COMMAND\n")
        shell.until(prompt)
        shell.pump(0.05)
        prompt_modes = termios.tcgetattr(shell.master)
        start = len(shell.output)
        shell.send(shlex.join(command).encode() + b"\n")
        shell.until(b"JOB_READY", start=start)
        client_pid = os.tcgetpgrp(shell.master)
        assert client_pid != shell.process.pid
        original_processes = inspect_session()
        shell.send(b"\x1a")
        shell.until(b"REMOTE_CTRL_Z_OK")
        for stage, prefix, pause in [(b"first", b"\x1e\x1a", 0),
                                     (b"second", b"\x1b[94;5u\x1b[94;5:3u\x1b[122;5u", 22),
                                     (b"third", b"\x1e\x1a", 0),
                                     (b"external", None, 0)]:
            start = len(shell.output)
            if prefix is None:
                os.kill(client_pid, signal.SIGSTOP)
            else:
                shell.send(stage + b"\r" + prefix)
            shell.until(prompt, start=start)
            assert os.tcgetpgrp(shell.master) == shell.process.pid
            modes = termios.tcgetattr(shell.master)
            assert modes == prompt_modes, (modes, prompt_modes)
            if stage in [b"first", b"external"]:
                start = len(shell.output)
                shell.send(b"bg\n")
                shell.until(prompt, start=start)
                start = len(shell.output)
                for _ in range(2):
                    os.kill(client_pid, signal.SIGCONT)
                deadline = time.monotonic() + 0.3
                while time.monotonic() < deadline:
                    shell.pump(0.02)
                assert b"\x1b[?1049h" not in shell.output[start:]
                assert b"\x1b_G" not in shell.output[start:]
                assert termios.tcgetattr(shell.master) == prompt_modes
            deadline = time.monotonic() + pause
            while time.monotonic() < deadline:
                shell.pump(0.05)
            if stage == b"third":
                fcntl.ioctl(shell.master, termios.TIOCSWINSZ, struct.pack("HHHH", 12, 60, 600, 240))
            start = len(shell.output)
            shell.send(b"fg\n")
            shell.until(b"JOB_CACHED_TEXT", start=start)
            if prefix is None:
                shell.send(stage + b"\r")
            shell.until(b"JOB_" + stage.upper() + b"_OK", start=start)
            assert os.tcgetpgrp(shell.master) == client_pid
            assert b"\x1b]2;JOB_TITLE\x1b\\" in shell.output[start:]
            assert b"\x1b]7;file:///tmp/mosh-job\x1b\\" in shell.output[start:]
            assert b"\x1b_Ga=t," in shell.output[start:]
            assert b"\x1b[?5h" in shell.output[start:]
            assert shell.output.count(b"\x1b]52;c;am9i\x1b\\") == 1
            assert shell.output.count(b"\x1b]133;A\x1b\\") == 1
            assert inspect_session() == original_processes
            print(f"CLI job-control {stage.decode()} passed with the original session", flush=True)
        start = len(shell.output)
        shell.send(b"final\r")
        shell.until(prompt, start=start)
        assert b"JOB_FINAL_OK" in shell.output[start:]
        assert shell.output.count(b"\x1b[22;0t") == shell.output.count(b"\x1b[23;0t")
        assert shell.output.count(b"\x1b[>31u") == shell.output.count(b"\x1b[<u")
        shell.send(b"exit\n")
        shell.finish(9)
    finally:
        if client_pid is not None:
            try:
                os.kill(client_pid, signal.SIGTERM)
                os.kill(client_pid, signal.SIGCONT)
            except ProcessLookupError:
                pass
        shell.close()
        cleanup_session(state)
    print("CLI suspend/bg/fg, long suspension, cached graphics/metadata, effects once, queued keys once, resize and termios passed", flush=True)


def verify_background_exit(command, state, termination):
    shell_path = "/bin/zsh" if Path("/bin/zsh").exists() else "/bin/bash"
    shell = TerminalProcess([shell_path, "-f", "-i"] if shell_path.endswith("zsh")
                            else [shell_path, "--noprofile", "--norc", "-i"])
    prompt = b"LOCAL_BACKGROUND_PROMPT> "
    client_pid = None
    try:
        shell.send(b"PS1='LOCAL_BACKGROUND_''PROMPT> '; unset PROMPT_COMMAND\n")
        shell.until(prompt)
        shell.send(shlex.join(command).encode() + b"\n")
        shell.until(b"WAIT_READY")
        client_pid = os.tcgetpgrp(shell.master)
        original_processes = json.loads(Path(state).read_text())
        start = len(shell.output)
        shell.send(b"\x1e\x1a")
        shell.until(prompt, start=start)
        start = len(shell.output)
        shell.send(b"bg\n")
        shell.until(prompt, start=start)
        start = len(shell.output)
        os.kill(client_pid, termination)
        deadline = time.monotonic() + 5
        while True:
            shell.pump(0.02)
            try:
                os.kill(client_pid, 0)
            except ProcessLookupError:
                break
            assert time.monotonic() < deadline, "background client did not exit"
        assert b"\x1b[?1049h" not in shell.output[start:]
        assert b"\x1b[23;0t" not in shell.output[start:]
        for pid in original_processes.values():
            os.kill(pid, 0)
        shell.send(b"exit 0\n")
        shell.finish(0)
    finally:
        if client_pid is not None:
            try:
                os.kill(client_pid, signal.SIGTERM)
                os.kill(client_pid, signal.SIGCONT)
            except ProcessLookupError:
                pass
        shell.close()
        cleanup(state)


def read_diagnostics(paths):
    records = []
    for path in paths:
        data = path.read_bytes()
        lines = data.splitlines(keepends=True)
        records.extend(json.loads(line) for line in lines if line.endswith(b"\n"))
    return records


def verify_diagnostics(client, server, script, temporary):
    prefix = [client, "--diagnostics", "--local", "--server", server, "--predict", "never", "--"]
    state = str(Path(temporary) / "diagnostic-idle.json")
    process = TerminalProcess(prefix + [sys.executable, script, "--remote", "diagnostic-idle", state])
    try:
        process.until(b"DIAGNOSTIC_IDLE_READY")
        assert len(process.diagnostic_files) == 2, process.diagnostic_files
        deadline = time.monotonic() + 1.7
        while time.monotonic() < deadline:
            process.pump()
        records = read_diagnostics(process.diagnostic_files)
        assert {record["role"] for record in records} == {"client", "server"}, records
        assert not any(record["event"] == "waiting" for record in records), records
        process.send(b"finish\r")
        process.finish(0)
    finally:
        process.close()
        cleanup(state)

    state = str(Path(temporary) / "diagnostic-flood.json")
    process = TerminalProcess(prefix + [sys.executable, script, "--remote", "diagnostic-flood", state])
    try:
        process.until(b"DIAGNOSTIC_FLOOD_READY")
        process.send(b"start\r")
        deadline = time.monotonic() + 10
        waiting = []
        while not waiting:
            assert process.process.poll() is None, "client exited during blocked terminal output"
            waiting = [record for record in read_diagnostics(process.diagnostic_files)
                       if record["role"] == "client" and record["event"] == "waiting"
                       and record.get("stage") in {"tty_write", "tty_flush"}]
            assert time.monotonic() < deadline, "no live diagnostic while terminal output was blocked"
            time.sleep(0.05)
        assert waiting[0]["duration_ms"] >= 1000, waiting[0]
        assert waiting[0]["progress"]["epoch"] > 0, waiting[0]
        process.send(b"stop\r")
        process.until(b"DIAGNOSTIC_FLOOD_DONE")
        process.finish(0)
        records = read_diagnostics(process.diagnostic_files)
        assert any(record["event"] == "resumed" and record.get("stage") in {"tty_write", "tty_flush"}
                   for record in records), "no recovery diagnostic after draining terminal output"
    finally:
        process.close()
        cleanup(state)
    print("CLI diagnostics observed a blocked terminal before release, recovery and healthy idle", flush=True)


def check_diagnostics(evidence_dir):
    assert DIAGNOSTIC_FILES, "no diagnostic paths were reported"
    records = read_diagnostics(DIAGNOSTIC_FILES)
    assert {record["role"] for record in records} == {"client", "server"}
    for record in records:
        assert record["schema"] == 1, record
        assert isinstance(record["pid"], int) and record["pid"] > 0, record
        assert record["elapsed_ms"] >= 0, record
        assert record["event"] in {"start", "summary", "waiting", "resumed", "transition",
                                   "slow", "queue_full", "stop", "budget_exhausted"}, record
        if "context" in record:
            assert set(record["context"]) <= {"attempt", "epoch", "sequence", "revision", "bytes"}, record
            assert all(isinstance(value, int) and value >= 0 for value in record["context"].values()), record
    forbidden = [b"DIAGNOSTIC_IDLE_READY", b"DIAGNOSTIC_FLOOD", b"NETWORK_BURST_OK",
                 b"READY_CLI", b"literal ' quotes", b"local-clipboard", b"am9i", RAW_NETWORK_INPUT[:-1]]
    for path in DIAGNOSTIC_FILES:
        data = path.read_bytes()
        assert path.stat().st_mode & 0o777 == 0o600, path
        assert path.parent.stat().st_mode & 0o777 == 0o700, path.parent
        assert len(data) <= 16 * 1024 * 1024, path
        assert all(value not in data for value in forbidden), path
        if evidence_dir is not None:
            evidence_dir.mkdir(parents=True, exist_ok=True)
            (evidence_dir / f"{path.parent.name}-{path.name}").write_bytes(data)
    print(f"CLI diagnostics schema, privacy and file bounds passed for {len(DIAGNOSTIC_FILES)} files", flush=True)


def verify(bin_dir, network, diagnostics=False, evidence_dir=None):
    DIAGNOSTIC_FILES.clear()
    script = str(Path(__file__).resolve())
    client = str(bin_dir / "mosh-rs")
    server = str(bin_dir / "mosh-rs-server")
    diagnostic_args = ["--diagnostics"] if diagnostics else []
    with tempfile.TemporaryDirectory(prefix="mosh-rs-cli-") as temporary:
        state = str(Path(temporary) / "session.json")
        prefix = [client, *diagnostic_args, "--local", "--server", server, "--graphics", "kitty", "--clipboard-read", "--"]
        process = TerminalProcess(prefix + [sys.executable, script, "--remote", "modern", state,
                                             "literal ' quotes $HOME $(printf unsafe) ; &"])
        try:
            process.until(b"READY_CLI")
            assert b"38;2;12;34;56" in process.output and "e\u0301中文".encode() in process.output
            assert b"\x1b[0mDEFAULT_COLOR_CLI" in process.output, "default text must inherit the terminal theme"
            assert b"\x1b[0;48;2;0;0;0mBLACK_COLOR_CLI" in process.output, "explicit black must remain black"
            assert b"\x1b[0mRESTORED_COLOR_CLI" in process.output, "SGR 49 must restore the terminal theme"
            process.send(b"\x1b[200~paste \xe4\xb8\xad\xe6\x96\x87\x1b[201~")
            process.send(b"\x1b[97:65:97;5:2;65:98u\x1b[1;1A\x1b[1;1:2A\x1b[1;1:3A"
                         b"\x1b[<0;3;2M\x1b[<0;3;2m\x1b[M !!\x1b[M#!!\x1b[<66;3;2M\x1b[O\x1b[I")
            process.until(b"PASTE_CLI_OK")
            process.until(b"KEY_CLI_OK")
            process.until(b"ARROW_CLI_OK")
            process.until(b"INPUT_CLI_OK")
            fcntl.ioctl(process.master, termios.TIOCSWINSZ, struct.pack("HHHH", 12, 60, 600, 240))
            process.send(b"\n\x08\r")
            process.until(b"\x1b]52;c;?\x1b\\")
            assert b"\x1b]52;c;cmVtb3Rl\x1b\\" in process.output
            process.send(b"\x1b]52;c;bG9jYWw=\x1b\\")
            process.until(b"CLIPBOARD_CLI_OK")
            process.finish(7)
        finally:
            process.close()
            cleanup(state)
        print("CLI default colors, explicit black, arrow phases, modern input, clipboard, literal argv, resize, exit code, and termios restoration passed", flush=True)

        shim = Path(temporary) / "ssh"
        shim.write_text(f"#!{sys.executable}\nimport os,sys\nif '-G' in sys.argv:\n    print('hostname ::1')\nelse:\n    os.execl('/bin/sh','sh','-c',sys.argv[-1])\n")
        shim.chmod(0o700)
        state = str(Path(temporary) / "ssh-bind.json")
        process = TerminalProcess(
            [client, *diagnostic_args, "--server", server, "--bind", "127.0.0.1", "fixture", "--",
             sys.executable, script, "--remote", "wait", state],
            {"PATH": temporary + os.pathsep + os.environ["PATH"],
             "SSH_CONNECTION": "192.0.2.1 40000 ::1 22"})
        try:
            process.until(b"WAIT_READY")
            process.send(b"\x1e.")
            process.finish(0)
        finally:
            process.close()
            cleanup(state)
        print("CLI explicit bind survives conflicting SSH metadata through a local SSH shim", flush=True)

        for suffix, prefix_bytes in [("legacy", b"\x1e"), ("kitty", b"\x1b[94;5u\x1b[94;5:3u")]:
            state = str(Path(temporary) / f"detach-{suffix}.json")
            process = TerminalProcess(prefix + [sys.executable, script, "--remote", "wait", state])
            try:
                process.until(b"WAIT_READY")
                process.send(prefix_bytes + b".")
                process.finish(0)
                os.kill(json.loads(Path(state).read_text())["command"], 0)
            finally:
                process.close()
                cleanup(state)
        print("CLI legacy and kitty release escape sequences detach without terminating the remote command", flush=True)

        state = str(Path(temporary) / "job.json")
        verify_job_control(prefix + [sys.executable, script, "--remote", "job", state], state)
        for termination in [signal.SIGTERM, signal.SIGHUP]:
            state = str(Path(temporary) / f"background-{termination}.json")
            verify_background_exit(prefix + [sys.executable, script, "--remote", "wait", state],
                                   state, termination)
        print("CLI running background clients exit on TERM/HUP without touching the terminal or ending the remote command", flush=True)

        if network:
            state = str(Path(temporary) / "network.json")
            wrapper = Path(temporary) / "bootstrap-proxy"
            wrapper.write_text(f"#!{sys.executable}\nimport runpy,sys\nsys.argv={[script, '--proxy', server, state]!r}+sys.argv[1:]\nrunpy.run_path({script!r},run_name='__main__')\n")
            wrapper.chmod(0o700)
            process = TerminalProcess([client, *diagnostic_args, "--local", "--server", str(wrapper), "--predict", "never", "--",
                                       sys.executable, script, "--remote", "network", state])
            try:
                def set_profile(profile):
                    updated = Path(state + ".profile.new")
                    updated.write_text(json.dumps(profile))
                    updated.replace(state + ".profile")
                    deadline = time.monotonic() + 5
                    while True:
                        process.pump(0.01)
                        if Path(state + ".stats").exists():
                            stats = json.loads(Path(state + ".stats").read_text())
                            if stats["profile"] == profile:
                                return stats
                        assert process.process.poll() is None, bytes(process.output[-4000:])
                        assert time.monotonic() < deadline, "UDP relay did not install profile"

                process.until(b"NETWORK_READY")
                original_processes = json.loads(Path(state).read_text())
                process.send(b"baseline\r")
                process.until(b"NETWORK_BASELINE_OK")
                set_profile({"delay_ms": 65, "jitter_ms": 55, "loss": 0.12, "duplicate": 0.10})
                process.send(RAW_NETWORK_INPUT)
                process.until(b"NETWORK_BURST_OK", timeout=45)
                stats = set_profile({})
                assert stats["dropped"] > 0 and stats["duplicated"] > 0 and stats["reordered"] > 0, stats
                assert stats["overflow"] == 0, stats
                print("CLI raw 8192-key input arrived byte-for-byte through UDP delay, jitter, loss, duplication, and reordering", flush=True)
                Path(state + ".drop").touch()
                process.send(b"resume-once\r")
                deadline = time.monotonic() + 22
                while time.monotonic() < deadline:
                    process.pump()
                    assert process.process.poll() is None, bytes(process.output[-4000:])
                Path(state + ".drop").unlink()
                process.until(b"NETWORK_ONCE_OK", timeout=30)
                process.finish(9)
                assert json.loads(Path(state).read_text()) == original_processes, "reconnect started a replacement command"
            finally:
                process.close()
                cleanup(state)
            print("CLI recovered through a UDP outage with queued input applied once and the original command still running", flush=True)
        if diagnostics:
            verify_diagnostics(client, server, script, temporary)
            check_diagnostics(evidence_dir)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--remote":
        sys.exit(remote_fixture(sys.argv[2], sys.argv[3]))
    if len(sys.argv) > 1 and sys.argv[1] == "--proxy":
        proxy_bootstrap(sys.argv[2], sys.argv[3])
        sys.exit(0)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path(__file__).resolve().parents[1] / "target" / "debug")
    parser.add_argument("--network", action="store_true", help="also exercise raw input over impaired UDP and automatic reconnect")
    parser.add_argument("--diagnostics", action="store_true", help="verify both endpoint logs, blocked terminal output and healthy idle")
    parser.add_argument("--evidence-dir", type=Path, help="copy diagnostic JSONL files to this directory")
    arguments = parser.parse_args()
    if arguments.evidence_dir is not None and not arguments.diagnostics:
        parser.error("--evidence-dir requires --diagnostics")
    verify(arguments.bin_dir.resolve(), arguments.network, arguments.diagnostics, arguments.evidence_dir)
