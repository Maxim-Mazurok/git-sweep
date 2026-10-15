#!/usr/bin/env python3
"""Render the SVG source and build a complete macOS ICNS using native tools."""

import shutil
import subprocess
import tempfile
from pathlib import Path


def main():
    root = Path(__file__).resolve().parent.parent
    swift = shutil.which("swift")
    iconutil = shutil.which("iconutil")
    if not swift or not iconutil:
        raise SystemExit("Run on macOS with Xcode Command Line Tools installed.")

    source = root / "assets/icon.svg"
    preview = root / "assets/icon.png"
    destination = root / "macos/GitSweep.icns"
    with tempfile.TemporaryDirectory(prefix="git-sweep-icon-") as temporary:
        temporary = Path(temporary)
        rendered = temporary / "icon.png"
        iconset = temporary / "GitSweep.iconset"
        iconset.mkdir()
        subprocess.run(
            [swift, str(root / "scripts/render_icon.swift"), str(source), str(iconset), str(rendered)],
            check=True,
        )
        compiled = temporary / "GitSweep.icns"
        subprocess.run([iconutil, "--convert", "icns", str(iconset), "--output", str(compiled)], check=True)
        shutil.copyfile(rendered, preview)
        shutil.copyfile(compiled, destination)
    print(f"Generated {preview} and {destination}")


if __name__ == "__main__":
    main()
