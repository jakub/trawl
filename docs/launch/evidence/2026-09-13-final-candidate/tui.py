"""Exercise the documented TUI connection in a private PTY and home."""
import fcntl
import json
import os
from pathlib import Path
import pty
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

binary, config, output = map(lambda p: Path(p).resolve(strict=True), sys.argv[1:])
with tempfile.TemporaryDirectory(prefix="trawl-launch-tui-") as temporary:
    home = Path(temporary)
    socket = home / "driver.sock"
    env = {"HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config"),
           "PATH": "/usr/bin:/bin", "TERM": "xterm-256color", "LANG": "C.UTF-8"}
    pid, master = pty.fork()
    if pid == 0:
        os.execve(binary, [str(binary), "--config", str(config), "--driver", str(socket)], env)
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

    def drain():
        try:
            while os.read(master, 65536):
                pass
        except OSError:
            pass

    reader = threading.Thread(target=drain, daemon=True)
    reader.start()
    reaped = False

    def driver(*args):
        return subprocess.check_output(
            [str(binary), "--config", str(config), "driver", "--socket", str(socket), *args],
            env=env, text=True, timeout=30,
        )

    try:
        deadline = time.monotonic() + 20
        while not socket.exists():
            ended, status = os.waitpid(pid, os.WNOHANG)
            if ended:
                reaped = True
                raise AssertionError(f"TUI exited before driver startup: {status}")
            assert time.monotonic() < deadline, "TUI driver did not start"
            time.sleep(0.05)
        status = json.loads(driver("status"))
        (output / "tui-initial-status.json").write_text(json.dumps(status, indent=2) + "\n")
        count = driver("query", "service=tutorial last=1h | stats count() by service", "--timeout", "15000", "--format", "json")
        assert [json.loads(line) for line in count.splitlines()] == [{"service": "tutorial", "count": 3}], count
        error = driver("query", "service=tutorial _severity>=error last=1h | table message, duration", "--timeout", "15000", "--format", "json")
        assert [json.loads(line) for line in error.splitlines()] == [{"message": "connection refused", "duration": 1500}], error
        capture = driver("capture", "--width", "120", "--height", "40")
        assert "connection refused" in capture and "1500" in capture, capture
        (output / "tui-capture.txt").write_text(capture)
        driver("quit")
        deadline = time.monotonic() + 10
        while True:
            ended, status = os.waitpid(pid, os.WNOHANG)
            if ended:
                reaped = True
                assert os.waitstatus_to_exitcode(status) == 0, status
                break
            assert time.monotonic() < deadline, "TUI did not quit cleanly"
            time.sleep(0.05)
        assert not socket.exists(), "TUI left its driver socket"
        report = {"status": "passed", "count": 3, "error_row": {"message": "connection refused", "duration": 1500},
                  "text_capture": "120x40", "clean_exit": True, "socket_removed": True,
                  "limits": "No pixel, color, clipboard, or native macOS evidence"}
        (output / "tui-report.json").write_text(json.dumps(report, indent=2) + "\n")
        print("Owned TUI: both exact queries, text rendering, clean exit and socket cleanup passed")
    finally:
        if not reaped:
            os.kill(pid, signal.SIGTERM)
            os.waitpid(pid, 0)
        os.close(master)
        reader.join(timeout=1)
