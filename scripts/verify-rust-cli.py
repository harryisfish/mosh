#!/usr/bin/env python3
"""Exercise the Rust binaries through real local PTYs and an optional UDP outage."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
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
    if mode == "network":
        os.write(1, b"\x1b[2J\x1b[HNETWORK_READY\r\n")
        exact(b"resume-once\r", timeout=60)
        os.write(1, b"\r\nNETWORK_ONCE_OK\r\n")
        return 9
    assert sys.argv[4] == "literal ' quotes $HOME $(printf unsafe) ; &"
    os.write(1, b"\x1b[2J\x1b[H\x1b[?1h\x1b[?2004h\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[>31u\x1b[38;2;12;34;56mREADY_CLI\x1b[0m " + "e\u0301中文".encode())
    exact(b"\x1b[200~paste \xe4\xb8\xad\xe6\x96\x87\x1b[201~")
    os.write(1, b"\r\nPASTE_CLI_OK\r\n")
    exact(b"\x1b[97:65:97;5:2;65:98u")
    os.write(1, b"KEY_CLI_OK\r\n")
    exact(b"\x1b[<0;3;2M")
    exact(b"\x1b[<0;3;2m")
    exact(b"\x1b[<66;3;2M")
    exact(b"\x1b[O\x1b[I")
    os.write(1, b"\x1b[<u\r\nINPUT_CLI_OK\r\n")
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
        while True:
            for source in select.select([front, back], [], [], 0.1)[0]:
                packet, address = source.recvfrom(65536)
                if source is front:
                    peer = address
                if Path(state + ".drop").exists():
                    continue
                if source is front:
                    back.sendto(packet, destination)
                elif peer is not None:
                    front.sendto(packet, peer)
    Path(state + ".proxy").write_text(str(pid))
    print(json.dumps(grant), flush=True)


class TerminalProcess:
    def __init__(self, command):
        self.master, self.slave = pty.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 800, 480))
        self.saved = termios.tcgetattr(self.slave)
        self.output = bytearray()
        self.closing = False
        environment = dict(os.environ, TERM="xterm-kitty", COLORTERM="truecolor")

        def session():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        self.process = subprocess.Popen(command, stdin=self.slave, stdout=self.slave,
                                        stderr=self.slave, env=environment, preexec_fn=session)

    def send(self, data):
        os.write(self.master, data)

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

    def until(self, marker, timeout=15):
        deadline = time.monotonic() + timeout
        while marker not in self.output:
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


def verify(bin_dir, network):
    script = str(Path(__file__).resolve())
    client = str(bin_dir / "mosh-rs")
    server = str(bin_dir / "mosh-rs-server")
    with tempfile.TemporaryDirectory(prefix="mosh-rs-cli-") as temporary:
        state = str(Path(temporary) / "session.json")
        prefix = [client, "--local", "--server", server, "--graphics", "kitty", "--clipboard-read", "--"]
        process = TerminalProcess(prefix + [sys.executable, script, "--remote", "modern", state,
                                             "literal ' quotes $HOME $(printf unsafe) ; &"])
        try:
            process.until(b"READY_CLI")
            assert b"38;2;12;34;56" in process.output and "e\u0301中文".encode() in process.output
            process.send(b"\x1b[200~paste \xe4\xb8\xad\xe6\x96\x87\x1b[201~")
            process.send(b"\x1b[97:65:97;5:2;65:98u\x1b[<0;3;2M\x1b[<0;3;2m\x1b[<66;3;2M\x1b[O\x1b[I")
            process.until(b"PASTE_CLI_OK")
            process.until(b"KEY_CLI_OK")
            process.until(b"INPUT_CLI_OK")
            fcntl.ioctl(process.master, termios.TIOCSWINSZ, struct.pack("HHHH", 12, 60, 600, 240))
            process.send(b"\r")
            process.until(b"\x1b]52;c;?\x1b\\")
            assert b"\x1b]52;c;cmVtb3Rl\x1b\\" in process.output
            process.send(b"\x1b]52;c;bG9jYWw=\x1b\\")
            process.until(b"CLIPBOARD_CLI_OK")
            process.finish(7)
        finally:
            process.close()
            cleanup(state)
        print("CLI modern input, clipboard, literal argv, resize, exit code, and termios restoration passed", flush=True)

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

        if network:
            state = str(Path(temporary) / "network.json")
            wrapper = Path(temporary) / "bootstrap-proxy"
            wrapper.write_text(f"#!{sys.executable}\nimport runpy,sys\nsys.argv={[script, '--proxy', server, state]!r}\nrunpy.run_path({script!r},run_name='__main__')\n")
            wrapper.chmod(0o700)
            process = TerminalProcess([client, "--local", "--server", str(wrapper), "--predict", "never", "--",
                                       sys.executable, script, "--remote", "network", state])
            try:
                process.until(b"NETWORK_READY")
                Path(state + ".drop").touch()
                process.send(b"resume-once\r")
                deadline = time.monotonic() + 22
                while time.monotonic() < deadline:
                    process.pump()
                    assert process.process.poll() is None, bytes(process.output[-4000:])
                Path(state + ".drop").unlink()
                process.until(b"NETWORK_ONCE_OK", timeout=30)
                process.finish(9)
                assert bytes(process.output).count(b"NETWORK_ONCE_OK") == 1
            finally:
                process.close()
                cleanup(state)
            print("CLI recovered through a UDP outage with queued input applied once and the original command still running", flush=True)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--remote":
        sys.exit(remote_fixture(sys.argv[2], sys.argv[3]))
    if len(sys.argv) > 1 and sys.argv[1] == "--proxy":
        proxy_bootstrap(sys.argv[2], sys.argv[3])
        sys.exit(0)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path(__file__).resolve().parents[1] / "target" / "debug")
    parser.add_argument("--network", action="store_true", help="also exercise automatic reconnect through a UDP outage")
    arguments = parser.parse_args()
    verify(arguments.bin_dir.resolve(), arguments.network)
