#!/usr/bin/env python3
"""Exercise the Rust binaries through real local PTYs and optional impaired UDP."""

import argparse
import fcntl
import heapq
import json
import locale
import os
from pathlib import Path
import pty
import random
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
ATOMIC_SECOND_PASTE_BYTES = 128 * 1024 - 24 - 98
GRANT_BEGIN = b"\x1eMOSH-RS-GRANT\n"
GRANT_END = b"\x1eMOSH-RS-GRANT-END\n"
SSH_LITERAL_ARG = "literal ' quotes \" $HOME $(printf unsafe) ; &\nsecond line"


def decode_grant(output):
    assert len(output) <= 512 * 1024 + 64 * 1024 + len(GRANT_BEGIN) + len(GRANT_END)
    assert output.count(GRANT_BEGIN) == output.count(GRANT_END) == 1, "missing or duplicate grant frame"
    _, framed = output.split(GRANT_BEGIN)
    body, _ = framed.split(GRANT_END)
    assert body.endswith(b"\n") and body.count(b"\n") == 1 and len(body) <= 512 * 1024
    return json.loads(body)


def encode_grant(grant):
    return GRANT_BEGIN + json.dumps(grant).encode() + b"\n" + GRANT_END


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
    if mode == "locale":
        locale.setlocale(locale.LC_CTYPE, "")
        observed = {key: os.environ.get(key) for key in ["LC_ALL", "LC_CTYPE", "LANG"]}
        observed["codeset"] = locale.nl_langinfo(locale.CODESET)
        Path(state + ".locale").write_text(json.dumps(observed))
        os.write(1, "\x1b[2J\x1b[HLOCALE_CLI_READY 中文é".encode())
        exact(b"finish\r")
        return 0
    if mode == "startup":
        with Path(state + ".starts").open("a") as marker:
            marker.write(str(os.getpid()) + "\n")
        os.write(1, b"\x1b[2J\x1b[HSTARTUP_READY")
        exact(b"finish\r")
        os.write(1, b"\r\nSTARTUP_DONE")
        return 0
    if mode == "prediction":
        os.write(1, b"\x1b[2J\x1b[HPREDICTION_READY\x1b[3;1H")
        for glyph in b"abc":
            exact(bytes([glyph]))
            os.write(1, bytes([glyph]))
        exact(b"d")
        exact(bytes.fromhex(sys.argv[4]))
        exact(b"S")
        Path(state + ".hidden").touch()
        exact(b"\r")
        os.write(1, b"\r\nPREDICTION_DONE")
        return 0
    if mode in ["wait", "ssh-argv"]:
        if mode == "ssh-argv":
            assert sys.argv[4] == SSH_LITERAL_ARG, "SSH changed structured argv"
        os.write(1, b"\x1b[2J\x1b[HWAIT_READY")
        while os.read(0, 1):
            pass
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
        for index, offset in enumerate(range(0, len(RAW_NETWORK_INPUT), 256)):
            exact(RAW_NETWORK_INPUT[offset:offset + 256], timeout=60)
            os.write(1, f"NETWORK_CHUNK_{index}_OK\r\n".encode())
        os.write(1, b"NETWORK_BURST_OK\r\n")
        exact(b"resume-once\r", timeout=60)
        os.write(1, b"\r\nNETWORK_ONCE_OK\r\n")
        return 9
    if mode in ["saturation", "atomic-admission", "paste-refusal"]:
        os.write(1, b"\x1b[2J\x1b[HSATURATION_READY\r\n")
        if mode in ["saturation", "atomic-admission"]:
            exact(b"A" * (128 * 1024), timeout=60)
            if mode == "atomic-admission":
                exact(b"D" * ATOMIC_SECOND_PASTE_BYTES, timeout=60)
            Path(state + ".accepted").write_text(str(128 * 1024))
            os.write(1, b"\r\nSATURATION_ACCEPTED_ONCE\r\n")
        else:
            exact(b"v", timeout=60)
            Path(state + ".tail").write_text("v")
            os.write(1, b"\r\nPASTE_TAIL_OK\r\n")
        exact(b"finish\r", timeout=60)
        assert not select.select([0], [], [], 0.2)[0], "refused input was sent later"
        os.write(1, b"\r\nSATURATION_DONE\r\n")
        return 9
    if mode == "close-after-input":
        os.write(1, b"\x1b[2J\x1b[HCLOSE_INPUT_READY\r\n")
        deadline = time.monotonic() + 30
        while not Path(state + ".read").exists():
            assert time.monotonic() < deadline, "close fixture was not released"
            time.sleep(0.01)
        count = 0
        while count < 128 * 1024:
            chunk = os.read(0, min(4096, 128 * 1024 - count))
            assert chunk and chunk == b"A" * len(chunk), "accepted close input was changed"
            count += len(chunk)
            progress = Path(state + ".progress.new")
            progress.write_text(str(count))
            progress.replace(state + ".progress")
            time.sleep(0.005)
        while True:
            time.sleep(0.01)
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
    result = subprocess.run([server, "bootstrap"], input=request, stdout=subprocess.PIPE, check=True)
    grant = decode_grant(result.stdout)
    if Path(state + ".drop").exists():
        assert not Path(state).exists(), "server bootstrap spawned a command before publishing its grant"
        assert not Path(state + ".starts").exists(), "server bootstrap created a command marker"
        Path(state + ".prepared").touch()
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
    sys.stdout.buffer.write(encode_grant(grant))
    sys.stdout.buffer.flush()


class TerminalProcess:
    def __init__(self, command, extra_env=None):
        self.master, self.slave = pty.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 800, 480))
        self.saved = termios.tcgetattr(self.slave)
        self.output = bytearray()
        self.closing = False
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
        drain_deadline = time.monotonic() + 0.25
        while time.monotonic() < drain_deadline:
            self.pump(0.01)
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


def verify_saturated_input(bin_dir, temporary):
    script = str(Path(__file__).resolve())
    client = str(bin_dir / "mosh-rs")
    server = str(bin_dir / "mosh-rs-server")

    def session(name, mode="wait", proxy=True):
        state = str(Path(temporary) / (name + ".json"))
        target = server
        if proxy:
            wrapper = Path(temporary) / ("proxy-" + name)
            wrapper.write_text(f"#!{sys.executable}\nimport runpy,sys\nsys.argv={[script, '--proxy', server, state]!r}\nrunpy.run_path({script!r},run_name='__main__')\n")
            wrapper.chmod(0o700)
            target = str(wrapper)
        process = TerminalProcess([client, "--local", "--server", target, "--predict", "never", "--",
                                   sys.executable, script, "--remote", mode, state])
        return process, state

    def saturate(process, state):
        Path(state + ".drop").touch()
        for glyph in [b"A", b"B"]:
            process.send(b"\x1b[200~" + glyph * (128 * 1024) + b"\x1b[201~")
        process.until(b"Input not accepted")

    def reclaimed(process, remote):
        deadline = time.monotonic() + 5
        while True:
            living = []
            for pid in remote.values():
                try:
                    os.kill(pid, 0)
                    living.append(pid)
                except ProcessLookupError:
                    pass
            if not living:
                return
            process.pump(0.02)
            assert time.monotonic() < deadline, ("healthy q left a remote process", living)

    for action in ["help", "detach", "suspend", "close", "signal"]:
        process, state = session(action)
        try:
            process.until(b"WAIT_READY")
            saturate(process, state)
            start = len(process.output)
            if action == "help":
                process.send(b"\x1e?")
                process.until(b"q close", start=start)
                assert b"Input not accepted" in process.output[start:], "help hid the refusal"
            elif action == "suspend":
                process.send(b"\x1e\x1a")
                deadline = time.monotonic() + 5
                while True:
                    status = subprocess.run(["ps", "-o", "stat=", "-p", str(process.process.pid)],
                                            text=True, capture_output=True).stdout
                    if "T" in status:
                        break
                    process.pump(0.01)
                    assert time.monotonic() < deadline, "saturated client did not suspend"
                modes = termios.tcgetattr(process.master)
                saved = process.saved.copy()
                modes[3] &= ~getattr(termios, "PENDIN", 0)
                saved[3] &= ~getattr(termios, "PENDIN", 0)
                assert modes == saved, (modes, saved)
                os.kill(process.process.pid, signal.SIGCONT)
            elif action in ["close", "signal"]:
                process.send(b"\x1eq")
                process.until(b"Close unconfirmed", start=start)
                assert process.process.poll() is None, "q must preserve the writer until remote confirmation"
            if action == "signal":
                os.kill(process.process.pid, signal.SIGTERM)
            else:
                process.send(b"\x1e.")
            process.finish(0)
            os.kill(json.loads(Path(state).read_text())["command"], 0)
            if action in ["close", "signal"]:
                restored = process.output.rfind(b"\x1b[?1049l")
                assert b"remote close is unconfirmed" in process.output[restored:], "missing main-screen close warning"
        finally:
            try:
                os.kill(process.process.pid, signal.SIGCONT)
            except ProcessLookupError:
                pass
            process.close()
            cleanup(state)
        print(f"CLI saturated input: {action} preserved local control", flush=True)

    process, state = session("admission", "saturation")
    try:
        process.until(b"SATURATION_READY")
        original = json.loads(Path(state).read_text())
        saturate(process, state)
        process.send(b"REJECTED_CANARY\r\x1e?")
        process.until(b"q close")
        Path(state + ".drop").unlink()
        process.until(b"Input ready", timeout=30)
        process.until(b"SATURATION_ACCEPTED_ONCE", timeout=30)
        assert Path(state + ".accepted").read_text() == str(128 * 1024)
        process.send(b"finish\r")
        process.until(b"SATURATION_DONE")
        process.finish(9)
        assert json.loads(Path(state).read_text()) == original, "recovery replaced the remote process"
    finally:
        process.close()
        cleanup(state)
    print("CLI admission: accepted A arrived exactly once; B/canary were refused and never replayed; low-water resume required fresh finish input", flush=True)

    process, state = session("atomic", "atomic-admission")
    try:
        process.until(b"SATURATION_READY")
        Path(state + ".drop").touch()
        process.send(b"\x1b[200~" + b"A" * (128 * 1024) + b"\x1b[201~")
        process.send(b"\x1b[200~" + b"D" * ATOMIC_SECOND_PASTE_BYTES + b"\x1b[201~")
        process.send(b"\x1ez")
        process.until(b"Input not accepted")
        Path(state + ".drop").unlink()
        process.until(b"Input ready", timeout=30)
        process.until(b"SATURATION_ACCEPTED_ONCE", timeout=30)
        process.send(b"finish\r")
        process.until(b"SATURATION_DONE")
        process.finish(9)
    finally:
        process.close()
        cleanup(state)
    print("CLI atomic escape admission: neither prefix nor unknown suffix reached the remote at a one-key budget boundary", flush=True)

    process, state = session("paste", "paste-refusal", proxy=False)
    try:
        process.until(b"SATURATION_READY")
        process.send(b"\x1b[200~" + b"a" * (128 * 1024) + b"\x1e.\x1eq\x1e\x1a\x1b[201~")
        process.until(b"Input not accepted: too large")
        process.send(b"\x1b[200~\xff\x1b[201~v")
        process.until(b"Paste not accepted: invalid UTF-8")
        process.until(b"PASTE_TAIL_OK")
        assert Path(state + ".tail").read_text() == "v"
        start = len(process.output)
        process.send(b"\x1b[200~" + b"\x00" * (128 * 1024) + b"\x1b[201~")
        process.until(b"Input not accepted: too large", start=start)
        process.send(b"finish\r")
        process.until(b"SATURATION_DONE")
        process.finish(9)
    finally:
        process.close()
        cleanup(state)
    print("CLI paste: oversized, invalid UTF-8 and JSON-expanded paste refused without body controls; same-send valid tail arrived and rejection remained visible", flush=True)

    process, state = session("unfinished-paste", proxy=False)
    try:
        process.until(b"WAIT_READY")
        process.send(b"\x1b[200~" + b"a" * (128 * 1024 + 1) + b"\x1e.\x1eq")
        process.until(b"Input not accepted: too large")
        assert process.process.poll() is None
        os.kill(process.process.pid, signal.SIGTERM)
        process.finish(0)
        os.kill(json.loads(Path(state).read_text())["command"], 0)
    finally:
        process.close()
        cleanup(state)
    print("CLI unfinished oversized paste ignored body escapes and remained locally interruptible by TERM", flush=True)

    process, state = session("healthy-close", proxy=False)
    try:
        process.until(b"WAIT_READY")
        remote = json.loads(Path(state).read_text())
        process.send(b"\x1eq")
        process.finish(129)
        assert b"remote close is unconfirmed" not in process.output
        reclaimed(process, remote)
    finally:
        process.close()
        cleanup(state)
    print("CLI healthy q confirmed Exit, restored the terminal, and reclaimed worker/command", flush=True)

    process, state = session("close-after-input", "close-after-input", proxy=False)
    try:
        process.until(b"CLOSE_INPUT_READY")
        remote = json.loads(Path(state).read_text())
        process.send(b"\x1b[200~" + b"A" * (128 * 1024) + b"\x1b[201~\x1eq")
        process.until(b"Close unconfirmed")
        deadline = time.monotonic() + 0.5
        while time.monotonic() < deadline:
            process.pump(0.02)
            assert process.process.poll() is None, "q closed before accepted PTY writes could commit"
            os.kill(remote["command"], 0)
        assert not Path(state + ".progress").exists()
        Path(state + ".read").touch()
        process.finish(129)
        assert int(Path(state + ".progress").read_text()) >= 64 * 1024
        reclaimed(process, remote)
    finally:
        process.close()
        cleanup(state)
    print("CLI q waited for accepted input's PTY commit before confirming Exit and reclaiming worker/command", flush=True)


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


def verify(bin_dir, network):
    script = str(Path(__file__).resolve())
    client = str(bin_dir / "mosh-rs")
    server = str(bin_dir / "mosh-rs-server")
    with tempfile.TemporaryDirectory(prefix="mosh-rs-cli-") as temporary:
        verify_delayed_spawn(bin_dir, temporary)
        verify_failed_spawn(bin_dir, temporary)
        verify_ssh_destination(bin_dir, temporary)
        verify_locale(bin_dir, temporary)
        verify_failed_locale(bin_dir, temporary)
        verify_prediction(bin_dir, temporary)
        state = str(Path(temporary) / "session.json")
        prefix = [client, "--local", "--server", server, "--graphics", "kitty", "--clipboard-read", "--"]
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
        shim.write_text(f"""#!{sys.executable}
import subprocess,sys,time
if '-G' in sys.argv:
    print('hostname ::1')
else:
    print('SSH bootstrap welcome banner', flush=True)
    print('SSH stderr diagnostic', file=sys.stderr, flush=True)
    child = subprocess.Popen(['/bin/sh', '-c', sys.argv[-1]], stdin=sys.stdin.buffer, stdout=subprocess.PIPE)
    while True:
        chunk = child.stdout.read(7)
        if not chunk:
            break
        sys.stdout.buffer.write(chunk)
        sys.stdout.buffer.flush()
        time.sleep(0.001)
    print('SSH bootstrap trailing banner', flush=True)
    sys.exit(child.wait())
""")
        shim.chmod(0o700)
        state = str(Path(temporary) / "ssh-bind.json")
        process = TerminalProcess(
            [client, "--server", server, "--bind", "127.0.0.1", "fixture", "--",
             sys.executable, script, "--remote", "ssh-argv", state, SSH_LITERAL_ARG],
            {"PATH": temporary + os.pathsep + os.environ["PATH"],
             "SSH_CONNECTION": "192.0.2.1 40000 ::1 22"})
        try:
            process.until(b"WAIT_READY")
            for diagnostic in [b"SSH bootstrap welcome banner", b"SSH stderr diagnostic",
                               b"SSH bootstrap trailing banner"]:
                assert diagnostic in process.output, "SSH lost a bootstrap diagnostic"
            process.send(b"\x1e.")
            process.finish(0)
        finally:
            process.close()
            cleanup(state)
        print("CLI explicit bind, fragmented grant, SSH banners/stderr, and literal multiline argv passed through a local SSH shim", flush=True)

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
            wrapper.write_text(f"#!{sys.executable}\nimport runpy,sys\nsys.argv={[script, '--proxy', server, state]!r}\nrunpy.run_path({script!r},run_name='__main__')\n")
            wrapper.chmod(0o700)
            process = TerminalProcess([client, "--local", "--server", str(wrapper), "--predict", "never", "--",
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
                for index, offset in enumerate(range(0, len(RAW_NETWORK_INPUT), 256)):
                    process.send(RAW_NETWORK_INPUT[offset:offset + 256])
                    process.until(f"NETWORK_CHUNK_{index}_OK".encode(), timeout=30)
                process.until(b"NETWORK_BURST_OK", timeout=45)
                stats = set_profile({})
                assert stats["dropped"] > 0 and stats["duplicated"] > 0 and stats["reordered"] > 0, stats
                assert stats["overflow"] == 0, stats
                print("CLI paced raw 8192-key input arrived byte-for-byte through UDP delay, jitter, loss, duplication, and reordering", flush=True)
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
            verify_saturated_input(bin_dir, temporary)


def verify_delayed_spawn(bin_dir, temporary):
    script = str(Path(__file__).resolve())
    state = str(Path(temporary) / "startup.json")
    wrapper = Path(temporary) / "startup-bootstrap-proxy"
    wrapper.write_text(f"#!{sys.executable}\nimport runpy,sys\nsys.argv={[script, '--proxy', str(bin_dir / 'mosh-rs-server'), state]!r}\nrunpy.run_path({script!r},run_name='__main__')\n")
    wrapper.chmod(0o700)
    Path(state + ".drop").touch()
    process = TerminalProcess([str(bin_dir / "mosh-rs"), "--local", "--server", str(wrapper),
                               "--", sys.executable, script, "--remote", "startup", state])
    try:
        deadline = time.monotonic() + 10
        while True:
            process.pump(0.01)
            if Path(state + ".stats").exists():
                stats = json.loads(Path(state + ".stats").read_text())
                if stats["dropped"] > 0:
                    break
            assert process.process.poll() is None, bytes(process.output[-4000:])
            assert time.monotonic() < deadline, "initial UDP traffic did not reach the blocking relay"
        deadline = time.monotonic() + 0.3
        while time.monotonic() < deadline:
            process.pump(0.01)
        assert not Path(state).exists(), "bootstrap started a user process before authenticated attach"
        assert not Path(state + ".starts").exists(), "UDP-blocked bootstrap created a command marker"
        assert Path(state + ".prepared").exists(), "grant publication was not checked before relay startup"
        Path(state + ".drop").unlink()
        process.until(b"STARTUP_READY")
        command = json.loads(Path(state).read_text())["command"]
        assert Path(state + ".starts").read_text() == f"{command}\n", "first attach started duplicate commands"
        process.send(b"finish\r")
        process.until(b"STARTUP_DONE")
        process.finish(0)
        assert Path(state + ".starts").read_text() == f"{command}\n", "command restarted during the session"
    finally:
        process.close()
        cleanup(state)
    print("CLI bootstrap completed with blocked UDP and no command marker; restored attach started the command once", flush=True)


def verify_failed_spawn(bin_dir, temporary):
    missing = str(Path(temporary) / "missing-command")
    process = TerminalProcess([str(bin_dir / "mosh-rs"), "--local", "--server",
                               str(bin_dir / "mosh-rs-server"), "--", missing])
    try:
        process.finish(1)
        assert b"remote command startup failed" in process.output, "spawn failure was not shown to the client"
        assert b"retrying" not in process.output, "fatal spawn failure entered the retry loop"
        assert process.output.count(b"remote command startup failed") == 1, "spawn failure was reported repeatedly"
    finally:
        process.close()
    print("CLI missing command exits with its startup error and never retries the dead session", flush=True)


def verify_ssh_destination(bin_dir, temporary):
    script = str(Path(__file__).resolve())
    folder = Path(temporary) / "ssh-destination"
    folder.mkdir()
    shim = folder / "ssh"
    shim.write_text(f"""#!{sys.executable}
import json,os,runpy,subprocess,sys,time
from pathlib import Path
state = os.environ['MOSH_DESTINATION_STATE']
assert sys.argv[sys.argv.index('-p') + 1] == '2222'
assert sys.argv[sys.argv.index('--') + 1] == 'fixture-alias'
with open(state + '.events', 'a') as events:
    events.write('query\\n' if '-G' in sys.argv else 'bootstrap\\n')
if '-G' in sys.argv:
    Path(state + '.query').write_text(str(os.getpid()))
    assert sys.stdin.buffer.read(1) == b''
    mode = os.environ.get('MOSH_DESTINATION_QUERY', 'normal')
    hostname = os.environ.get('MOSH_DESTINATION_HOSTNAME', '127.0.0.1')
    if mode == 'failure': sys.exit(42)
    if mode == 'missing':
        print('user fixture')
        sys.exit(0)
    if mode == 'timeout': time.sleep(30)
    if mode == 'invalid-utf8':
        sys.stdout.buffer.write(b'\\xff')
        sys.exit(0)
    if mode == 'duplicate':
        print('hostname ' + hostname)
    if mode in ['limit', 'oversize']:
        body = ('hostname ' + hostname + '\\n').encode()
        sys.stdout.buffer.write(body + b'x' * (512 * 1024 - len(body) + (mode == 'oversize')))
        sys.stdout.buffer.flush()
        if mode == 'oversize': time.sleep(30)
        sys.exit(0)
    print('hostname ' + hostname)
    print('hostname stderr-only.invalid', file=sys.stderr)
    sys.exit(0)
driver = runpy.run_path({script!r})
request = json.loads(sys.stdin.buffer.readline())
request['bind'] = os.environ.get('MOSH_DESTINATION_WORKER_BIND', '127.0.0.1')
worker = subprocess.Popen([{str(bin_dir / 'mosh-rs-server')!r}, 'worker'],
                          stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                          start_new_session=True)
Path(state + '.worker').write_text(str(worker.pid))
worker.stdin.write((json.dumps(request) + '\\n').encode())
worker.stdin.close()
framed = b''.join(worker.stdout.readline() for _ in range(3))
worker.stdout.close()
grant = driver['decode_grant'](framed)
assert grant['server_cert'], 'fixture must use the real worker certificate'
grant['address'] = os.environ.get('MOSH_DESTINATION_GRANT_ADDRESS', grant['address'])
Path(state + '.grant').write_text(json.dumps({{'address': grant['address'], 'certificate_bytes': len(grant['server_cert'])}}))
sys.stdout.buffer.write(driver['encode_grant'](grant))
sys.stdout.buffer.flush()
""")
    shim.chmod(0o700)
    cases = [
        {"name": "alias", "hostname": "localhost", "grant_address": "192.0.2.20"},
        {"name": "bracket", "hostname": "[::1]", "worker_bind": "::1"},
        {"name": "bind", "bind": "127.0.0.1", "query": "failure", "expect_query": False},
        {"name": "mapped-bind", "bind": "::ffff:127.0.0.1", "query": "failure", "expect_query": False},
        {"name": "mapped-grant", "bind": "127.0.0.1", "grant_address": "::ffff:127.0.0.1", "expect_query": False},
        {"name": "mapped-wildcard", "bind": "::ffff:0.0.0.0", "hostname": "localhost"},
        {"name": "limit", "query": "limit"},
        {"name": "bind-rewrite", "bind": "127.0.0.1", "worker_bind": "::1", "expect_query": False,
         "error": b"server UDP address does not match --bind"},
        {"name": "hostname-rewrite", "hostname": "127.0.0.1", "worker_bind": "::1", "blocked": True},
    ]
    for mode, error in [
        ("failure", b"SSH configuration query exited with"),
        ("missing", b"SSH configuration is missing hostname"),
        ("timeout", b"SSH configuration query timed out"),
        ("oversize", b"SSH configuration output exceeds limit"),
        ("invalid-utf8", b"SSH configuration is not UTF-8"),
        ("duplicate", b"SSH configuration contains duplicate hostname"),
    ]:
        cases.append({"name": mode, "query": mode, "error": error})
    for name, hostname in [("empty", ""), ("whitespace", "127.0.0.1 extra"),
                           ("control", "127.0.0.1\x01"), ("long", "x" * 1025),
                           ("double-bracket", "[[::1]]"), ("unpaired-bracket", "[::1")]:
        cases.append({"name": name, "hostname": hostname, "error": b"invalid SSH hostname"})
    for case in cases:
        state = str(folder / (case["name"] + ".json"))
        environment = {"PATH": str(folder) + os.pathsep + os.environ["PATH"],
                       "MOSH_DESTINATION_STATE": state}
        for key in ["query", "hostname", "worker_bind", "grant_address"]:
            if key in case:
                environment["MOSH_DESTINATION_" + key.upper()] = case[key]
        command = [str(bin_dir / "mosh-rs"), "--server", str(bin_dir / "mosh-rs-server"), "--ssh-port", "2222"]
        if "bind" in case:
            command += ["--bind", case["bind"]]
        process = TerminalProcess(command + ["fixture-alias", "--", sys.executable, script,
                                              "--remote", "startup", state], environment)
        try:
            if "error" in case:
                process.send(b"finish\r")
                deadline = time.monotonic() + 15
                while process.process.poll() is None:
                    process.pump()
                    assert time.monotonic() < deadline, bytes(process.output[-4000:])
                process.pump(0)
                assert process.process.returncode == 1, bytes(process.output[-4000:])
                assert case["error"] in process.output, bytes(process.output[-4000:])
                assert termios.tcgetattr(process.master) == process.saved
                assert not Path(state).exists(), "rejected destination started the command"
                assert not Path(state + ".starts").exists(), "untrusted worker received application input"
            elif case.get("blocked"):
                process.until(b"mosh-rs: UDP 127.0.0.1:")
                process.send(b"finish\r")
                deadline = time.monotonic() + 1
                while time.monotonic() < deadline:
                    process.pump()
                    assert process.process.poll() is None
                assert not Path(state).exists(), "client attached to the worker-selected host"
                assert not Path(state + ".starts").exists(), "untrusted worker received application input"
                process.send(b"\x1e.")
                process.finish(0)
            else:
                process.until(b"STARTUP_READY")
                process.send(b"finish\r")
                process.until(b"STARTUP_DONE")
                process.finish(0)
                assert Path(state + ".starts").read_text().count("\n") == 1
            expected_events = ["query"] if case.get("expect_query", True) else []
            if "error" not in case or not case.get("expect_query", True):
                expected_events.append("bootstrap")
                assert json.loads(Path(state + ".grant").read_text())["certificate_bytes"] > 0
            assert Path(state + ".events").read_text().splitlines() == expected_events
            query = Path(state + ".query")
            if query.exists():
                try:
                    os.kill(int(query.read_text()), 0)
                except ProcessLookupError:
                    pass
                else:
                    raise AssertionError("SSH configuration query was not reaped")
            print(f"CLI SSH destination {case['name']} passed; unauthorized application input=0", flush=True)
        finally:
            process.close()
            cleanup(state)
            for suffix in [".worker", ".query"]:
                pid_file = Path(state + suffix)
                if pid_file.exists():
                    try:
                        os.kill(int(pid_file.read_text()), signal.SIGTERM)
                    except ProcessLookupError:
                        pass


def verify_locale(bin_dir, temporary):
    state = str(Path(temporary) / "locale.json")
    wrapper = Path(temporary) / "locale-server"
    server = str(bin_dir / "mosh-rs-server")
    wrapper.write_text(f"""#!{sys.executable}
import json,os,subprocess,sys
request = json.loads(sys.stdin.buffer.readline())
with open({state + '.candidates'!r}, 'w') as output:
    json.dump(request['locale_candidates'], output)
env = dict(os.environ, LC_ALL='C', LC_CTYPE='C', LANG='C')
result = subprocess.run([{server!r}]+sys.argv[1:], input=(json.dumps(request)+'\\n').encode(), env=env)
sys.exit(result.returncode)
""")
    wrapper.chmod(0o700)
    requested = ["mosh_missing.UTF-8", "C", "en_US.UTF-8"]
    process = TerminalProcess([str(bin_dir / "mosh-rs"), "--local", "--server", str(wrapper),
                               "--", sys.executable, str(Path(__file__).resolve()),
                               "--remote", "locale", state],
                              dict(zip(["LC_ALL", "LC_CTYPE", "LANG"], requested)))
    try:
        process.until(b"LOCALE_CLI_READY")
        assert json.loads(Path(state + ".candidates").read_text()) == requested, "client changed locale candidate order"
        observed = json.loads(Path(state + ".locale").read_text())
        assert observed["LC_ALL"] is None, observed
        assert observed["LC_CTYPE"] == observed["LANG"], observed
        assert observed["codeset"].lower().replace("-", "") == "utf8", observed
        process.send(b"finish\r")
        process.finish(0)
    finally:
        process.close()
        cleanup(state)
    print("CLI ordered client locales override a C server environment with a real UTF-8 child locale", flush=True)


def verify_failed_locale(bin_dir, temporary):
    if sys.platform not in ["darwin", "linux"]:
        return
    source = Path(temporary) / "no-locale.c"
    library = Path(temporary) / "no-locale.so"
    source.write_text("""#define _GNU_SOURCE
#include <locale.h>
#include <langinfo.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>
static locale_t missing_locale(int mask, const char *name, locale_t base) {
    (void)mask; (void)base;
    const char *allowed = getenv("MOSH_RS_TEST_DISCOVERED");
    if (allowed && strcmp(name, allowed) == 0) return duplocale(LC_GLOBAL_LOCALE);
    errno = ENOENT;
    return (locale_t)0;
}
static char *fixture_codeset(nl_item item, locale_t locale) {
    (void)item; (void)locale;
    return "UTF-8";
}
#ifdef __APPLE__
extern char *nl_langinfo_l(nl_item, locale_t);
__attribute__((used)) static const struct { const void *replacement; const void *original; }
interpose[] __attribute__((section("__DATA,__interpose"))) = {
    { (const void *)missing_locale, (const void *)newlocale },
    { (const void *)fixture_codeset, (const void *)nl_langinfo_l }
};
#else
locale_t newlocale(int mask, const char *name, locale_t base) {
    return missing_locale(mask, name, base);
}
char *nl_langinfo_l(nl_item item, locale_t locale) {
    return fixture_codeset(item, locale);
}
#endif
""")
    compiler_args = ["-dynamiclib"] if sys.platform == "darwin" else ["-shared", "-fPIC"]
    subprocess.run(["cc", *compiler_args, str(source), "-o", str(library)], check=True)
    wrapper = Path(temporary) / "no-locale-server"
    server = str(bin_dir / "mosh-rs-server")
    variable = "DYLD_INSERT_LIBRARIES" if sys.platform == "darwin" else "LD_PRELOAD"
    wrapper.write_text(f"#!{sys.executable}\nimport os,sys\nenv=dict(os.environ)\nenv[{variable!r}]={str(library)!r}\nos.execve({server!r}, [{server!r}]+sys.argv[1:], env)\n")
    wrapper.chmod(0o700)
    state = str(Path(temporary) / "failed-locale.json")
    process = TerminalProcess([str(bin_dir / "mosh-rs"), "--local", "--server", str(wrapper),
                               "--", sys.executable, str(Path(__file__).resolve()),
                               "--remote", "startup", state])
    try:
        process.finish(1)
        assert b"no usable UTF-8 locale" in process.output, "locale startup failure was not visible"
        assert b"retrying" not in process.output, "fatal locale failure entered the retry loop"
        assert not Path(state).exists(), "failed locale selection started the command"
        assert not Path(state + ".starts").exists(), "failed locale selection created a command marker"
    finally:
        process.close()
        cleanup(state)
    print("CLI injected locale failure exits without a command marker or retry", flush=True)

    discovery = Path(temporary) / "discovery-tools"
    discovery.mkdir()
    tool = discovery / "locale"
    for mode, output, error in [
        ("missing", None, b"cannot discover server locales"),
        ("oversized", "sys.stdout.write('x' * (64 * 1024 + 1))", b"locale list exceeds"),
        ("timeout", "time.sleep(6)", b"locale discovery timed out"),
        ("discovered", "print('mosh_discovered.UTF-8')", None),
    ]:
        if output is not None:
            tool.write_text(f"#!{sys.executable}\nimport sys,time\n{output}\n")
            tool.chmod(0o700)
        state = str(Path(temporary) / f"locale-{mode}.json")
        environment = {"PATH": str(discovery), "MOSH_RS_TEST_DISCOVERED": "mosh_discovered.UTF-8"}
        process = TerminalProcess([str(bin_dir / "mosh-rs"), "--local", "--server", str(wrapper),
                                   "--", sys.executable, str(Path(__file__).resolve()),
                                   "--remote", "startup", state], environment)
        try:
            if error is None:
                process.until(b"STARTUP_READY")
                process.send(b"finish\r")
                process.finish(0)
                assert len(Path(state + ".starts").read_text().splitlines()) == 1
            else:
                process.finish(1)
                assert error in process.output, (mode, bytes(process.output[-4000:]))
                assert b"retrying" not in process.output
                assert not Path(state).exists(), "locale discovery failure started a command"
        finally:
            process.close()
            cleanup(state)
    print("CLI injected locale discovery covers an extra locale, missing utility, bounded output and timeout", flush=True)


def verify_prediction(bin_dir, temporary):
    script = str(Path(__file__).resolve())
    for control in [b"\r", b"\x1b", b"\x1b[A"]:
        state = str(Path(temporary) / ("prediction-" + control.hex() + ".json"))
        process = TerminalProcess([str(bin_dir / "mosh-rs"), "--local", "--server",
                                   str(bin_dir / "mosh-rs-server"), "--predict", "adaptive", "--",
                                   sys.executable, script, "--remote", "prediction", state, control.hex()])
        try:
            process.until(b"PREDICTION_READY")
            for index, glyph in enumerate(b"abc", 2):
                process.send(bytes([glyph]))
                process.until(f"\x1b[3;{index}H".encode())
            start = len(process.output)
            process.send(b"d")
            process.until(b"\x1b[0;4md", start=start)
            start = len(process.output)
            process.send(control)
            if control == b"\x1b":
                deadline = time.monotonic() + 0.2
                while time.monotonic() < deadline:
                    process.pump(0.01)
            process.send(b"S")
            deadline = time.monotonic() + 5
            while not Path(state + ".hidden").exists():
                process.pump(0.01)
                assert time.monotonic() < deadline, "raw/noecho fixture did not receive hidden input"
            assert b"\x1b[0;4mS" not in process.output[start:], "hidden input reused learned prediction confidence"
            print(f"CLI raw/noecho control={control.hex()} hidden predictions=0 tty_bytes={len(process.output)}", flush=True)
            process.send(b"\r")
            process.until(b"PREDICTION_DONE")
            process.finish(0)
            assert b"\x1b[0;4mS" not in process.output[start:], "hidden prediction appeared in drained terminal output"
        finally:
            process.close()
            cleanup(state)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--remote":
        sys.exit(remote_fixture(sys.argv[2], sys.argv[3]))
    if len(sys.argv) > 1 and sys.argv[1] == "--proxy":
        proxy_bootstrap(sys.argv[2], sys.argv[3])
        sys.exit(0)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path(__file__).resolve().parents[1] / "target" / "debug")
    parser.add_argument("--network", action="store_true", help="also exercise raw input over impaired UDP and automatic reconnect")
    parser.add_argument("--saturated-input", action="store_true", help="exercise input rejection, local control and close during a UDP outage")
    arguments = parser.parse_args()
    if arguments.saturated_input:
        with tempfile.TemporaryDirectory(prefix="mosh-rs-saturated-") as temporary:
            verify_saturated_input(arguments.bin_dir.resolve(), temporary)
    else:
        verify(arguments.bin_dir.resolve(), arguments.network)
