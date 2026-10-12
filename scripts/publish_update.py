#!/usr/bin/env python3
"""Publish an auto-update: sign the CI-built .app.tar.gz and commit updates/latest.json.

Usage (from disco/, with GitHub credentials injected):
    python3 scripts/publish_update.py 0.4.0 "release notes"

The updater private key never leaves this machine (default /workspace/.keys/disco-updater.key).
"""
import json
import os
import subprocess
import sys
import tempfile

REPO = "Cosmoto-jian/disco-a2a"
KEY = os.environ.get("DISCO_UPDATER_KEY", "/workspace/.keys/disco-updater.key")
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def gh(path, accept="application/vnd.github+json", out=None):
    cmd = ["curl", "-sfL", "-H", f"Accept: {accept}", f"https://api.github.com/repos/{REPO}{path}"]
    if out:
        subprocess.run(cmd + ["-o", out], check=True)
        return None
    return json.loads(subprocess.run(cmd, check=True, capture_output=True, text=True).stdout)


def main():
    ver, notes = sys.argv[1], (sys.argv[2] if len(sys.argv) > 2 else "")
    rel = gh(f"/releases/tags/v{ver}")
    name = f"Disco_{ver}_universal.app.tar.gz"
    asset = next((a for a in rel["assets"] if a["name"] == name), None)
    if not asset:
        sys.exit(f"release v{ver} has no {name} yet (CI still running?)")
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, name)
        gh(f"/releases/assets/{asset['id']}", "application/octet-stream", out=path)
        subprocess.run(["npx", "-y", "@tauri-apps/cli@2", "signer", "sign", "-f", KEY, "-p", "", path],
                       check=True, capture_output=True)
        sig = open(path + ".sig").read().strip()
    url = asset["browser_download_url"]
    platform = {"signature": sig, "url": url}
    manifest = {
        "version": ver,
        "notes": notes,
        # RFC 3339, which the Tauri updater requires.
        "pub_date": subprocess.run(["date", "-u", "+%Y-%m-%dT%H:%M:%SZ"], capture_output=True, text=True).stdout.strip(),
        "platforms": {"darwin-aarch64": platform, "darwin-x86_64": platform},
    }
    os.makedirs(os.path.join(ROOT, "updates"), exist_ok=True)
    with open(os.path.join(ROOT, "updates", "latest.json"), "w") as f:
        json.dump(manifest, f, indent=2, ensure_ascii=False)
        f.write("\n")
    print(f"updates/latest.json -> {ver}")


if __name__ == "__main__":
    main()
