"""Exercise installed Lisp and native modules using disposable batch Emacs."""

import os
import subprocess
import sys
import tempfile
from pathlib import Path

emacs, package, native, tests = sys.argv[1:]
load_paths = sorted({str(path.parent) for path in Path(package).rglob("reka.el")})
if not load_paths:
    raise SystemExit("Installed Reka Lisp was not found")
command = [emacs, "--batch", "-Q", "-L", f"{native}/share/emacs/site-lisp"]
for directory in load_paths:
    command.extend(["-L", directory])
command.extend(["-l", "ert"])
for test in sorted(Path(tests).glob("*.el")):
    command.extend(["-l", str(test)])
command.extend(["-f", "ert-run-tests-batch-and-exit"])
with tempfile.TemporaryDirectory(prefix="reka-ert-") as home:
    subprocess.run(command, check=True, env=os.environ | {"HOME": home})
