"""Exercise real keyboard delivery in a private headless River/Emacs session.

Run after `cargo build`, with river, emacs-pgtk, wev, wtype and coreutils on PATH.
Only synthetic test keys enter these isolated receivers; no physical input
backend or existing compositor socket is used.
"""

import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def wait_until(check, description, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.1)
    raise AssertionError(f"Timed out waiting for {description}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--module", type=Path, default=Path("target/debug/libreka.so"))
    parser.add_argument("--output-dir", type=Path)
    arguments = parser.parse_args()
    source = Path(__file__).resolve().parents[1]
    module = arguments.module.resolve()
    assert module.is_file(), "Build libreka.so before running integration tests"
    temporary = tempfile.TemporaryDirectory(prefix="reka-keyboard-")
    root = (
        arguments.output_dir.resolve() if arguments.output_dir else Path(temporary.name)
    )
    root.mkdir(parents=True, exist_ok=True)
    runtime = root / "runtime"
    runtime.mkdir(mode=0o700, exist_ok=True)
    runtime.chmod(0o700)
    environment = os.environ.copy()
    for name in (
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "SWAYSOCK",
        "RIVER_SOCKET",
        "OXBOW_SOCKET",
        "DBUS_SESSION_BUS_ADDRESS",
        "XAUTHORITY",
        "ALTERNATE_EDITOR",
    ):
        environment.pop(name, None)
    environment.update(
        {
            "XDG_RUNTIME_DIR": str(runtime),
            "XDG_CONFIG_HOME": str(root / "config"),
            "XDG_CACHE_HOME": str(root / "cache"),
            "XDG_DATA_HOME": str(root / "data"),
            "XDG_STATE_HOME": str(root / "state"),
            "WLR_BACKENDS": "headless",
            "WLR_RENDERER": "pixman",
            "WLR_HEADLESS_OUTPUTS": "1",
            "RUST_LOG": "info",
        }
    )
    startup = root / "startup.el"
    startup.write_text(
        f"(add-to-list 'load-path {json.dumps(str(module.parent))})\n"
        f"(add-to-list 'load-path {json.dumps(str(source / 'lisp'))})\n"
        "(require 'reka)\n(require 'reka-simulation)\n"
        "(reka-enable)\n"
        '(require \'server)\n(setq server-name "reka-test")\n'
        f"(setq server-socket-dir {json.dumps(str(runtime))})\n(server-start)\n"
    )
    launcher = root / "start-emacs.py"
    command = [shutil.which("emacs"), "-Q", "-l", str(startup)]
    launcher.write_text(
        f"#!{sys.executable}\nimport os\nos.execv({command[0]!r}, {command!r})\n"
    )
    launcher.chmod(0o700)
    children = []
    logs = []

    def launch(command, log):
        stream = (root / log).open("wb")
        logs.append(stream)
        process = subprocess.Popen(
            command,
            env=environment,
            stdout=stream,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        children.append(process)
        return process

    def evaluate(form):
        process = subprocess.run(
            [
                "emacsclient",
                "--socket-name",
                str(runtime / "reka-test"),
                "--alternate-editor",
                shutil.which("false"),
                "--eval",
                form,
            ],
            env=environment,
            capture_output=True,
            text=True,
            timeout=5,
        )
        if process.returncode:
            with (root / "emacs-eval-errors.log").open("a") as log:
                log.write(process.stderr)
            return ""
        return process.stdout.strip()

    try:
        river = launch(
            ["river", "-no-xwayland", "-c", shlex.quote(str(launcher))], "river.log"
        )
        wait_until(lambda: (runtime / "reka-test").exists(), "Emacs server")
        wait_until(
            lambda: evaluate("(reka-virtual-keyboard-available-p reka-handle)") == "t",
            "synchronized virtual keyboard",
        )
        sockets = sorted(runtime.glob("wayland-*"))
        environment["WAYLAND_DISPLAY"] = next(
            path.name for path in sockets if path.is_socket()
        )
        # A headless seat has no physical keyboard. Keep a synthetic keyboard
        # alive so receivers request wl_keyboard, as they do on a real laptop.
        # Its one harmless press happens before either receiver exists.
        launch(["wtype", "-k", "F35", "-s", "300000"], "keyboard-fixture.log")
        time.sleep(0.2)
        launch(["stdbuf", "-oL", "-eL", "wev"], "first.log")
        buffers = "(seq-filter (lambda (buf) (with-current-buffer buf (derived-mode-p 'reka-mode))) (buffer-list))"
        wait_until(
            lambda: evaluate(f"(length {buffers})") == "1", "first receiver buffer"
        )
        evaluate(f"(progn (switch-to-buffer (car {buffers})) (redisplay t))")
        time.sleep(0.4)
        assert (
            evaluate(
                f'(with-current-buffer (car {buffers}) (reka-send-keys "a C-b A RET 🙂 C-x") t)'
            )
            == "t"
        )
        first = root / "first.log"

        def initial_delivery():
            output = first.read_text()
            events = re.findall(r"key: (\d+); state: (\d+)", output)
            return output if len(events) >= 12 and "🙂" in output else None

        first_output = wait_until(initial_delivery, "Unicode keyboard delivery")
        assert "Return" in first_output, first_output
        assert "Control" in first_output and "Shift" in first_output, first_output
        modifiers = re.findall(r"depressed: ([0-9a-fA-F]{8})", first_output)
        assert modifiers and int(modifiers[-1], 16) == 0, first_output
        # Every synthetic press must be followed by its release.
        events = re.findall(r"key: (\d+); state: (\d+)", first_output)
        assert len(events) == 12, first_output
        assert all(
            events[index][0] == events[index + 1][0]
            and events[index][1] == "1"
            and events[index + 1][1] == "0"
            for index in range(0, len(events), 2)
        ), events
        print("PASS: Unicode, modifiers, balanced releases and intercepted-key bypass")

        launch(["stdbuf", "-oL", "-eL", "wev"], "second.log")
        wait_until(
            lambda: evaluate(f"(length {buffers})") == "2", "second receiver buffer"
        )
        assert (
            evaluate(
                f"(let ((buffers {buffers})) (delete-other-windows) "
                "(switch-to-buffer (car buffers)) "
                "(set-window-buffer (split-window-right) (cadr buffers)) (redisplay t) t)"
            )
            == "t"
        )
        time.sleep(0.4)
        second = root / "second.log"
        before = [path.stat().st_size for path in (first, second)]
        assert (
            evaluate(
                f"(let ((buffers {buffers})) "
                '(with-current-buffer (car buffers) (reka-send-keys "z")) '
                '(with-current-buffer (cadr buffers) (reka-send-keys "x")) t)'
            )
            == "t"
        )

        def routed():
            outputs = [
                path.read_bytes()[offset:].decode()
                for path, offset in zip((first, second), before)
            ]
            return outputs if all("state: 0" in output for output in outputs) else None

        outputs = wait_until(routed, "separate manage-sequence destinations")
        symbols = [set(re.findall(r"sym: +(\S+)", output)) for output in outputs]
        assert sorted(symbols, key=str) == sorted([{"x"}, {"z"}], key=str), outputs
        print("PASS: separate queued destinations receive only their own keys")
        # Destruction must release the old keyboard/keymap rather than leaving
        # a null KeyboardGroup or leaking state until the WM disconnects.
        fd_counts = []
        for _ in range(4):
            assert (
                evaluate("(progn (reka-reset-virtual-keyboard reka-handle) t)") == "t"
            )
            wait_until(
                lambda: (
                    evaluate("(reka-virtual-keyboard-available-p reka-handle)") == "t"
                ),
                "recreated synchronized keyboard",
            )
            assert evaluate("(progn (reka-set-focus-request reka-handle nil) t)") == "t"
            time.sleep(0.15)
            assert (
                evaluate(
                    f'(with-current-buffer (car {buffers}) (reka-send-keys "q") t)'
                )
                == "t"
            )
            time.sleep(0.15)
            assert river.poll() is None, (root / "river.log").read_text()
            fd_counts.append(len(list(Path(f"/proc/{river.pid}/fd").iterdir())))
        assert max(fd_counts) - min(fd_counts) <= 1, fd_counts
        print(
            "PASS: focus and virtual-keyboard destruction/recreation preserve compositor lifetime and resources"
        )
        assert river.poll() is None, (root / "river.log").read_text()
        # Disconnect the WM client while its keyboard still exists, then let
        # the compositor cleanly destroy clients/resources in its own order.
        evaluate("(run-at-time 0.05 nil #'kill-emacs 0)")
        wait_until(lambda: not (runtime / "reka-test").exists(), "WM disconnection")
        assert river.poll() is None, (root / "river.log").read_text()
        river.terminate()
        assert river.wait(timeout=10) == 0, (root / "river.log").read_text()
        compositor_log = (root / "river.log").read_text()
        assert (
            "panic:" not in compositor_log
            and "Segmentation fault" not in compositor_log
        ), compositor_log
        print(
            "PASS: WM disconnection and compositor shutdown release live keyboard callbacks safely"
        )
        print(f"Integration logs: {root}")
    finally:
        for process in reversed(children):
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
        for process in children:
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        for stream in logs:
            stream.close()
        temporary.cleanup()


if __name__ == "__main__":
    main()
