"""Exercise real transient/floating GTK clients in a private River/Emacs session.

Use the tools from integration-tools.nix and pass the packaged libreka.so with
--module. Host compositor sockets, input backends and session bus are excluded.
"""

import argparse
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from contextlib import suppress
from pathlib import Path

from integration_keyboard import wait_until
from PIL import Image, ImageChops


class Session:
    def __init__(self, root):
        self.root = root
        self.runtime = root / "reka-floating-runtime"
        self.runtime.mkdir(mode=0o700, parents=True)
        self.environment = os.environ.copy()
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
            self.environment.pop(name, None)
        self.environment.update(
            {
                "XDG_RUNTIME_DIR": str(self.runtime),
                "XDG_CONFIG_HOME": str(root / "config"),
                "XDG_CACHE_HOME": str(root / "cache"),
                "XDG_DATA_HOME": str(root / "data"),
                "XDG_STATE_HOME": str(root / "state"),
                "WLR_BACKENDS": "headless",
                "WLR_RENDERER": "pixman",
                "WLR_HEADLESS_OUTPUTS": "1",
                "GDK_BACKEND": "wayland",
                "GTK_A11Y": "none",
                "GSETTINGS_BACKEND": "memory",
                "RUST_LOG": "info",
            }
        )
        self.children = []
        self.logs = []

    def start(self, module):
        root = self.root
        source = Path(__file__).resolve().parents[1]
        startup = root / "startup.el"
        startup.write_text(
            f"(add-to-list 'load-path {json.dumps(str(module.parent))})\n"
            f"(add-to-list 'load-path {json.dumps(str(source / 'lisp'))})\n"
            "(require 'reka)\n(require 'reka-simulation)\n(require 'json)\n"
            '(setq tab-bar-new-tab-choice "*reka-empty-tab*")\n'
            '(get-buffer-create "*reka-empty-tab*")\n'
            "(reka-enable)\n"
            "(defvar reka-it-roles nil)\n"
            "(defun reka-it-buffer (role) (cdr (assoc role reka-it-roles)))\n"
            "(defun reka-it-register (role) (or (reka-it-buffer role) "
            "(when-let* ((b (seq-find (lambda (b) (not (rassq b reka-it-roles))) (reka--list-buffers)))) "
            "(push (cons role b) reka-it-roles) b)))\n"
            "(defun reka-it-state () (json-serialize "
            "`((buffers . ,(vconcat (mapcar (lambda (b) (with-current-buffer b "
            "`((name . ,(buffer-name)) (app-id . ,reka-app-id) (role . ,(car (rassq b reka-it-roles))) (floating . ,(if reka-floating t :false)) "
            "(geometry . ,(vconcat reka-floating-geometry))))) (reka--list-buffers)))) "
            "(windows . ,(vconcat (mapcar (lambda (w) "
            "`((buffer . ,(buffer-name (window-buffer w))) (role . ,(car (rassq (window-buffer w) reka-it-roles))) (edges . ,(vconcat (window-pixel-edges w))))) "
            "(window-list nil 'nomini)))) "
            "(focus . ,(car (rassq reka--focused-floating-buffer reka-it-roles))) "
            "(minibuffer . ,(minibuffer-depth)) "
            "(floating-keys . ,(if reka--floating-keymap-active t :false))) "
            ":null-object nil :false-object :false))\n"
            '(require \'server)\n(setq server-name "reka-test")\n'
            f"(setq server-socket-dir {json.dumps(str(self.runtime))})\n(server-start)\n"
        )
        launcher = root / "start-emacs.py"
        command = [shutil.which("emacs"), "-Q", "-l", str(startup)]
        launcher.write_text(
            f"#!{sys.executable}\nimport os\nos.execv({command[0]!r}, {command!r})\n"
        )
        launcher.chmod(0o700)
        self.river = self.launch(
            ["river", "-no-xwayland", "-c", shlex.quote(str(launcher))], "river"
        )
        wait_until(lambda: (self.runtime / "reka-test").exists(), "Emacs server")
        wait_until(
            lambda: (
                self.evaluate("(reka-virtual-keyboard-available-p reka-handle)") == "t"
            ),
            "synchronized keyboard",
        )
        self.environment["WAYLAND_DISPLAY"] = next(
            path.name
            for path in sorted(self.runtime.glob("wayland-*"))
            if path.is_socket()
        )
        self.launch(["wtype", "-k", "F35", "-s", "300000"], "keyboard")
        time.sleep(0.2)
        self.pointer = self.launch(["reka-pointer-fixture"], "pointer", pipe=True)

    def launch(self, command, name, pipe=False):
        log = (self.root / f"{name}.log").open("w")
        self.logs.append(log)
        process = subprocess.Popen(
            command,
            env=self.environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE if pipe else log,
            stderr=log,
            text=True,
            start_new_session=True,
        )
        self.children.append(process)
        return process

    def evaluate(self, form):
        result = subprocess.run(
            [
                "emacsclient",
                "--socket-name",
                str(self.runtime / "reka-test"),
                "--alternate-editor",
                shutil.which("false"),
                "--eval",
                form,
            ],
            env=self.environment,
            capture_output=True,
            check=False,
            text=True,
            timeout=5,
        )
        if result.returncode:
            raise AssertionError(result.stderr + result.stdout)
        return result.stdout.strip()

    def state(self):
        return json.loads(json.loads(self.evaluate("(reka-it-state)")))

    def role(self, role):
        return next(
            (buffer for buffer in self.state()["buffers"] if buffer["role"] == role),
            None,
        )

    def wait_role(self, role, floating=None):
        def check():
            self.evaluate(f'(progn (reka-it-register "{role}") t)')
            buffer = self.role(role)
            return (
                buffer
                if buffer
                and len(buffer["geometry"]) == 4
                and (floating is None or buffer["floating"] == floating)
                else None
            )

        return wait_until(check, f"{role} floating={floating}")

    def native(self, role, function, *arguments):
        args = " ".join(map(str, arguments))
        return self.evaluate(
            f'(progn ({function} reka-handle (buffer-local-value \'reka-window (reka-it-buffer "{role}")) {args}) t)'
        )

    def point(self, operation, x, y):
        self.pointer.stdin.write(f"{operation} {x} {y}\n")
        self.pointer.stdin.flush()
        assert self.pointer.stdout.readline().strip() == "OK", (
            self.root / "pointer.log"
        ).read_text()

    def click(self, role):
        x, y, width, height = self.wait_role(role)["geometry"]
        self.point("absolute", x + width - 15, y + height - 15)
        self.point("button", 272, 1)
        self.point("button", 272, 0)
        wait_until(
            lambda: (
                self.state()["focus"] == role
                or self.state()["windows"][0]["role"] == role
            ),
            f"{role} focus",
        )
        time.sleep(0.15)

    def keys(self, *arguments):
        subprocess.run(["wtype", *arguments], env=self.environment, check=True)
        time.sleep(0.15)

    def regions(self, name):
        path = self.root / f"{name}.png"
        time.sleep(0.15)
        subprocess.run(["grim", str(path)], env=self.environment, check=True)
        channels = Image.open(path).convert("RGB").split()
        regions = {}
        for index, role in enumerate(("parent", "dialog", "second")):
            mask = channels[index].point(lambda value: 255 if value > 200 else 0)
            for other in range(3):
                if other != index:
                    mask = ImageChops.multiply(
                        mask,
                        channels[other].point(lambda value: 255 if value < 60 else 0),
                    )
            regions[role] = mask.getbbox()
        return regions

    def close(self):
        for process in reversed(self.children):
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
        for process in self.children:
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        for log in self.logs:
            log.close()


def command(process, text):
    process.stdin.write(text + "\n")
    process.stdin.flush()


def events(root, name):
    return [
        json.loads(line)
        for line in (root / f"{name}.log").read_text().splitlines()
        if line.startswith("{")
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--module", type=Path, default=Path("target/debug/libreka.so"))
    parser.add_argument("--output-dir", type=Path)
    arguments = parser.parse_args()
    module = arguments.module.resolve()
    assert module.is_file(), "Build libreka.so before running integration tests"
    temporary = tempfile.TemporaryDirectory(prefix="reka-floating-")
    root = (
        arguments.output_dir.resolve() if arguments.output_dir else Path(temporary.name)
    )
    root.mkdir(parents=True, exist_ok=True)
    session = None
    try:
        session = Session(root)
        session.start(module)
        parent = session.launch(["reka-gtk-fixture", "parent"], "parent")
        session.wait_role("parent", False)
        session.evaluate(
            '(progn (switch-to-buffer (reka-it-buffer "parent")) (delete-other-windows) (redisplay t))'
        )
        time.sleep(0.2)
        before = session.wait_role("parent")["geometry"]
        command(parent, "dialog")
        session.wait_role("dialog", True)
        assert session.wait_role("parent")["geometry"] == before
        assert len(session.state()["windows"]) == 1, session.state()
        assert all(
            "dialog" not in window["buffer"] for window in session.state()["windows"]
        )
        regions = session.regions("automatic-dialog")
        assert regions["parent"] and regions["dialog"], regions
        print("PASS: real transient auto-floats without retile or an empty Emacs tile")

        second = session.launch(["reka-gtk-fixture", "second"], "second")
        session.wait_role("second", False)
        session.evaluate(
            '(progn (delete-other-windows) (switch-to-buffer (reka-it-buffer "parent")) (set-window-buffer (split-window-right) (reka-it-buffer "second")) (redisplay t))'
        )
        time.sleep(0.2)
        session.click("second")
        session.keys("-M", "logo", "-k", "f", "-m", "logo")
        floating = session.wait_role("second", True)["geometry"]
        assert len(session.state()["windows"]) == 1, session.state()
        assert session.regions("second-floating")["second"]
        (root / "first-toggle-state.json").write_text(
            json.dumps(session.state(), indent=2)
        )
        assert session.state()["focus"] == "second", session.state()
        session.keys("-M", "logo", "-k", "f", "-m", "logo")
        session.wait_role("second", False)
        wait_until(lambda: len(session.state()["windows"]) == 2, "restored real tile")
        assert session.regions("second-tiled")["second"]
        session.click("second")
        session.keys("-M", "logo", "-k", "f", "-m", "logo")
        assert session.wait_role("second", True)["geometry"] == floating
        assert session.state()["focus"] == "second", session.state()
        session.keys("-M", "logo", "-k", "f", "-m", "logo")
        session.wait_role("second", False)
        print(
            "PASS: native Super+f tile/float/tile restores visibility, tile membership and saved geometry"
        )

        session.click("dialog")
        previous = session.wait_role("dialog")["geometry"]
        modifier = session.launch(["wtype", "-M", "logo", "-s", "300000"], "modifier")
        time.sleep(0.2)
        x, y, width, height = previous
        session.point("absolute", x + width // 2, y + height // 2)
        session.point("button", 272, 1)
        time.sleep(0.15)
        session.point("motion", 40, 25)
        wait_until(
            lambda: session.wait_role("dialog")["geometry"][:2] != previous[:2],
            "native Super+pointer move",
        )
        session.point("button", 272, 0)
        modifier.terminate()
        modifier.wait(timeout=5)
        time.sleep(0.2)

        previous = session.wait_role("dialog")["geometry"]
        command(parent, "client-move")
        x, y, width, height = previous
        session.point("absolute", x + width // 2, y + height // 2)
        session.point("button", 272, 1)
        time.sleep(0.15)
        session.point("motion", -25, -20)
        wait_until(
            lambda: session.wait_role("dialog")["geometry"][:2] != previous[:2],
            "xdg client-requested move",
        )
        session.point("button", 272, 0)
        for delta, expected in ((900, (160, 120)), (-900, (500, 360))):
            previous = session.wait_role("dialog")["geometry"]
            command(parent, "client-resize")
            session.point("absolute", previous[0] + 10, previous[1] + 10)
            session.point("button", 272, 1)
            time.sleep(0.15)
            session.point("motion", delta, delta)
            resized = wait_until(
                lambda expected=expected: (
                    value
                    if (value := session.wait_role("dialog")["geometry"])[2:]
                    == list(expected)
                    else None
                ),
                "client resize constrained by actual GTK size hints",
            )
            session.point("button", 272, 0)
            assert resized[0] + resized[2] == previous[0] + previous[2], (
                previous,
                resized,
            )
            assert resized[1] + resized[3] == previous[1] + previous[3], (
                previous,
                resized,
            )
            time.sleep(0.15)
        configured = [
            event
            for event in events(root, "parent")
            if event["kind"] == "configure" and event["role"] == "dialog"
        ]
        assert any(
            (event["width"], event["height"]) == (160, 120) for event in configured
        ), configured
        previous = session.wait_role("dialog")["geometry"]
        modifier = session.launch(
            ["wtype", "-M", "logo", "-s", "300000"], "resize-modifier"
        )
        time.sleep(0.2)
        session.point(
            "absolute", previous[0] + previous[2] - 15, previous[1] + previous[3] - 15
        )
        session.point("button", 273, 1)
        time.sleep(0.15)
        session.point("motion", -100, -80)
        wait_until(
            lambda: session.wait_role("dialog")["geometry"][2:] != previous[2:],
            "native Super+right-pointer resize",
        )
        session.point("button", 273, 0)
        modifier.terminate()
        modifier.wait(timeout=5)
        assert any(
            (event["width"], event["height"]) == (500, 360) for event in configured
        ), configured
        print(
            "PASS: native pointer binding and GTK xdg move/resize requests honor min/max hints and opposite-edge anchoring"
        )

        session.click("dialog")
        session.evaluate('(progn (reka-send-keys "q") t)')
        wait_until(
            lambda: any(
                event["kind"] == "key"
                and event["role"] == "dialog"
                and event["keyval"] == ord("q")
                for event in events(root, "parent")
            ),
            "virtual keyboard into focused detached float",
        )
        session.evaluate(
            '(progn (switch-to-buffer (get-buffer-create "*editor-fixture*")) (erase-buffer) (redisplay t))'
        )
        time.sleep(0.2)
        before_keys = len(
            [event for event in events(root, "parent") if event["kind"] == "key"]
        )
        session.keys("editor-only")
        assert "editor-only" in session.evaluate(
            '(with-current-buffer "*editor-fixture*" (buffer-string))'
        )
        assert (
            len([event for event in events(root, "parent") if event["kind"] == "key"])
            == before_keys
        )
        session.click("dialog")
        session.evaluate(
            '(progn (run-at-time 0.1 nil (lambda () (setq reka-it-minibuffer (read-string "Fixture: ")))) t)'
        )
        wait_until(lambda: session.state()["minibuffer"] == 1, "real active minibuffer")
        assert session.state()["floating-keys"] is False, session.state()
        before_keys = len(
            [event for event in events(root, "parent") if event["kind"] == "key"]
        )
        session.keys("minibuffer-only", "-k", "Return")
        wait_until(lambda: session.state()["minibuffer"] == 0, "minibuffer completion")
        assert session.evaluate("reka-it-minibuffer") == '"minibuffer-only"'
        assert (
            len([event for event in events(root, "parent") if event["kind"] == "key"])
            == before_keys
        )
        assert not [event for event in events(root, "second") if event["kind"] == "key"]
        print(
            "PASS: focused float simulation, explicit Emacs editing focus and real minibuffer keyboard isolation"
        )

        for role in ("parent", "dialog", "second"):
            metadata = session.wait_role(role)
            assert metadata["name"].startswith("Reka fixture " + role), metadata
            assert metadata["app-id"] == "org.reka.FloatingFixture", metadata
        print(
            "PASS: initial real client title and app-id survive asynchronous buffer creation"
        )

        session.evaluate(
            '(progn (tab-bar-new-tab) (switch-to-buffer "*reka-empty-tab*") (delete-other-windows) (redisplay t))'
        )
        hidden = session.regions("different-tab")
        assert all(bounds is None for bounds in hidden.values()), hidden
        session.evaluate("(progn (tab-bar-select-tab 1) (redisplay t))")
        shown = session.regions("original-tab")
        assert all(bounds is not None for bounds in shown.values()), shown
        command(parent, "close-parent")
        wait_until(
            lambda: not session.role("parent") and not session.role("dialog"),
            "parent and transient destruction",
        )
        closed = session.regions("closed-parent")
        assert (
            closed["parent"] is None and closed["dialog"] is None and closed["second"]
        ), closed
        assert session.role("second") and session.river.poll() is None
        command(second, "quit")
        print(
            "PASS: tabs hide/show real surfaces; closing the parent destroys its transient without damaging another app"
        )
        print(f"Integration logs and screenshots: {root}")
    finally:
        if session:
            with suppress(Exception):
                (root / "last-state.json").write_text(
                    json.dumps(session.state(), indent=2)
                )
            session.close()
        temporary.cleanup()


if __name__ == "__main__":
    main()
