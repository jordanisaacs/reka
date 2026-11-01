"""Verify native click focus, client input and Emacs selection in private River."""

import argparse
import json
import tempfile
import time
from pathlib import Path

from integration_floating import Session, command, events
from integration_keyboard import wait_until


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--module", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument(
        "--lisp-dir", type=Path, default=Path(__file__).resolve().parents[1] / "lisp"
    )
    arguments = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="reka-floating-focus-") as temporary:
        root = (
            arguments.output_dir.resolve() if arguments.output_dir else Path(temporary)
        )
        root.mkdir(parents=True, exist_ok=True)
        session = Session(root)
        try:
            session.start(arguments.module.resolve(), arguments.lisp_dir.resolve())
            parent = session.launch(["reka-gtk-fixture", "parent"], "parent")
            session.wait_role("parent", False)
            second = session.launch(["reka-gtk-fixture", "second"], "second")
            session.wait_role("second", False)
            session.evaluate(
                '(progn (delete-other-windows) (switch-to-buffer (reka-it-buffer "parent")) '
                '(set-window-buffer (split-window-right) (reka-it-buffer "second")) (redisplay t))'
            )
            time.sleep(0.2)

            def active():
                return json.loads(
                    json.loads(
                        session.evaluate(
                            "(json-encode (with-current-buffer (window-buffer (selected-window)) "
                            "(car (rassq (reka--active-application-buffer) reka-it-roles))))"
                        )
                    )
                )

            def verify_click(role, log, character, button=272):
                x, y, width, height = session.wait_role(role)["geometry"]
                before = {
                    name: len(events(root, name)) for name in ("parent", "second")
                }
                selected = active()
                session.point("absolute", x + width - 15, y + height - 15)
                time.sleep(0.1)
                assert active() == selected, "Hover must preserve keyboard focus"
                session.point("button", button, 1)
                session.point("button", button, 0)
                session.keys(character)
                wait_until(
                    lambda: any(
                        event.get("kind") == "key"
                        and event.get("keyval") == ord(character)
                        and event.get("role") == role
                        for event in events(root, log)[before[log] :]
                    ),
                    f"physical keyboard input delivered to clicked {role}",
                )
                wait_until(
                    lambda: active() == role, f"clicked {role} selected in Emacs"
                )
                received = events(root, log)[before[log] :]
                assert (
                    sum(
                        event.get("kind") == "key"
                        and event.get("keyval") == ord(character)
                        and event.get("role") == role
                        for event in received
                    )
                    == 1
                ), received
                buttons = [
                    event
                    for event in received
                    if event.get("kind") == "button"
                    and event.get("role") == role
                    # GTK synthesizes extra double/triple-click events.
                    and event.get("event_type") in (4, 7)
                ]
                assert [(event["button"], event["pressed"]) for event in buttons] == [
                    ({272: 1, 273: 3, 274: 2}[button], True),
                    ({272: 1, 273: 3, 274: 2}[button], False),
                ], buttons
                other = "parent" if log == "second" else "second"
                assert not any(
                    event.get("kind") == "key" and event.get("keyval") == ord(character)
                    for event in events(root, other)[before[other] :]
                ), "Keyboard input leaked to the previously focused client"

            verify_click("second", "second", "a")
            verify_click("parent", "parent", "b")
            verify_click("second", "second", "c", button=273)
            print(
                "PASS: tiled clicks select the client and forward each button/key once; hover preserves focus"
            )

            command(parent, "dialog")
            session.wait_role("dialog", True)
            verify_click("second", "second", "d")
            verify_click("dialog", "parent", "e")
            verify_click("parent", "parent", "f")
            print(
                "PASS: clicks switch between a floating dialog, its same-client parent and another tiled client"
            )

            # The compositor owns one output while PGTK can retain another frame.
            # Duplicate placements must choose the output's frame, and a click
            # must not select the matching buffer in the spare frame.
            session.evaluate(
                "(progn (defvar focus-test-primary-frame (selected-frame)) "
                "(defvar focus-test-spare-frame (make-frame)) "
                # Frame/tab restoration can produce an existing duplicate.
                # Avoid the ordinary move-buffer advice in this setup only.
                "(advice-remove 'set-window-buffer #'reka--set-window-buffer-advice) "
                "(unwind-protect "
                '(set-window-buffer (frame-selected-window focus-test-spare-frame) (reka-it-buffer "second")) '
                "(advice-add 'set-window-buffer :around #'reka--set-window-buffer-advice)) "
                "(select-frame focus-test-primary-frame) "
                '(select-window (get-buffer-window (reka-it-buffer "parent") focus-test-primary-frame)) '
                "(redisplay t) t)"
            )
            time.sleep(0.3)
            verify_click("second", "second", "g")
            assert (
                session.evaluate("(eq (selected-frame) focus-test-primary-frame)")
                == "t"
            )
            session.evaluate(
                "(progn (select-frame focus-test-spare-frame) (redisplay t) t)"
            )
            verify_click("second", "second", "h")
            assert (
                session.evaluate("(eq (selected-frame) focus-test-primary-frame)")
                == "t"
            ), "Click selected the duplicate buffer in the spare frame"
            session.evaluate("(delete-frame focus-test-spare-frame)")
            print(
                "PASS: a duplicated app buffer in a spare Emacs frame cannot steal click focus"
            )
            assert session.river.poll() is None
            (root / "final-state.json").write_text(
                json.dumps(session.state(), indent=2)
            )
            command(second, "quit")
            command(parent, "quit")
        except Exception:
            print("FAILURE STATE:", json.dumps(session.state(), indent=2), flush=True)
            print(
                "FRAMES:",
                session.evaluate(
                    "(mapcar (lambda (f) (list (frame-parameter f (quote name)) "
                    "(mapcar (lambda (w) (buffer-name (window-buffer w))) "
                    "(window-list f (quote no-minibuf))))) (frame-list))"
                ),
                flush=True,
            )
            print((root / "river.log").read_text(), flush=True)
            print(
                "MESSAGES:",
                session.evaluate('(with-current-buffer "*Messages*" (buffer-string))'),
                flush=True,
            )
            raise
        finally:
            session.close()


if __name__ == "__main__":
    main()
