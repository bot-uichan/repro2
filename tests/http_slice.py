#!/usr/bin/env python3
"""Real registry/gateway HTTP exercise with SQLite and MOCK upstream narinfo.

Run after cargo build --workspace. No Tailscale daemon or Nix build is involved.
The locally injected header models the trusted Serve proxy, not real IdP proof.
Uses existing service ports 3000/3001; refuses to run if either is occupied.
"""
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.request import Request, build_opener, ProxyHandler

ROOT = Path(__file__).resolve().parents[1]
HASH = "y1a49lg2ja68djssigz14lhdxvxcwbxa"
STORE = f"/nix/store/{HASH}-hello-2.12.3"
NAR_HASH = "sha256-rS0qEqEXArxnAdzxNkNv+4PaHxXcQ/JdN0Kjkuq6XSY="
NAR_SIZE = 226640
REGISTRY = "http://127.0.0.1:3001"
GATEWAY = "http://127.0.0.1:3000"
HTTP = build_opener(ProxyHandler({}))


def request(url, report=None, user=None):
    headers = {}
    data = None
    if report is not None:
        data = json.dumps(report).encode()
        headers["Content-Type"] = "application/json"
    if user is not None:
        headers["Tailscale-User-Login"] = user
    try:
        with HTTP.open(Request(url, data=data, headers=headers), timeout=3) as response:
            return response.status, response.read().decode()
    except HTTPError as error:
        return error.code, error.read().decode()


def ready(child, url):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if child.poll() is not None:
            raise AssertionError(f"service exited: {child.returncode}")
        try:
            request(url)
            return
        except OSError:
            time.sleep(0.05)
    raise AssertionError("service did not become ready")


class MockCache(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != f"/{HASH}.narinfo":
            self.send_error(404)
            return
        body = (f"StorePath: {STORE}\nURL: nar/mock.nar\nCompression: none\n"
                f"NarHash: {NAR_HASH}\nNarSize: {NAR_SIZE}\n").encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


def main():
    for port in (3000, 3001):
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", port))
    for binary in ("migration", "registry", "gateway"):
        assert (ROOT / "target/debug" / binary).is_file(), "run cargo build --workspace first"
    cache = ThreadingHTTPServer(("127.0.0.1", 0), MockCache)
    threading.Thread(target=cache.serve_forever, daemon=True).start()
    cache_url = f"http://127.0.0.1:{cache.server_port}/"
    report = dict(drv_path="/nix/store/example.drv", output_name="out", store_path_hash=HASH,
                  store_path=STORE, nar_hash=NAR_HASH, nar_size=NAR_SIZE, cache_url=cache_url)
    children = []
    try:
        with tempfile.TemporaryDirectory(prefix="http-slice-", dir=ROOT / "target") as tmp:
            database = Path(tmp) / "reports.sqlite"
            env = {**os.environ, "DATABASE_URL": f"sqlite://{database}?mode=rwc",
                   "REGISTRY_URL": REGISTRY, "REQUIRED_USERS": "2"}
            subprocess.run([str(ROOT / "target/debug/migration"), "up"], env=env, check=True,
                           stdout=subprocess.DEVNULL)
            with sqlite3.connect(database) as db:
                db.execute("INSERT INTO build_reports (store_path_hash,store_path,nar_hash,nar_size,cache_url) VALUES (?,?,?,?,?)",
                           (HASH, STORE, NAR_HASH, NAR_SIZE, cache_url))
            with open(Path(tmp) / "services.log", "w+") as logs:
                registry = subprocess.Popen([str(ROOT / "target/debug/registry")], env=env, stdout=logs, stderr=logs)
                children.append(registry)
                ready(registry, f"{REGISTRY}/nar-info/{HASH}")
                gateway = subprocess.Popen([str(ROOT / "target/debug/gateway")], env=env, stdout=logs, stderr=logs)
                children.append(gateway)
                ready(gateway, f"{GATEWAY}/nix-cache-info")
                narinfo = f"{GATEWAY}/{HASH}.narinfo"
                post = f"{REGISTRY}/build-reports"
                assert request(narinfo)[0] == 404, "legacy NULL owner counted as a user"
                for user in (None, "", "   "):
                    assert request(post, report, user)[0] == 401
                assert request(post, {**report, "cache_url": "file:///etc/passwd"}, "alice@example.com")[0] == 400
                for _ in range(3):
                    assert request(post, {**report, "user_id": "forged@example.com"}, "alice@example.com")[0] == 201
                status, body = request(f"{REGISTRY}/nar-info/{HASH}")
                rows = json.loads(body)
                assert status == 200 and len(rows) == 2, rows
                assert sorted(str(row["user_id"]) for row in rows) == ["None", "alice@example.com"]
                assert request(narinfo)[0] == 404, "same user's duplicate posts reached N=2"
                assert request(post, {**report, "nar_size": NAR_SIZE + 1}, "alice@example.com")[0] == 201
                assert request(narinfo)[0] == 404, "one user's different results counted twice"
                assert request(post, {**report, "cache_url": None}, "bob@example.com")[0] == 201
                status, body = request(narinfo)
                assert status == 200 and f"URL: {cache_url}nar/mock.nar" in body, (status, body)
                assert request(post, {**report, "nar_size": NAR_SIZE + 1}, "charlie@example.com")[0] == 201
                status, body = request(narinfo)
                assert status == 200, "dissenting result vetoed a qualifying result"
                assert f"NarSize: {NAR_SIZE}\n" in body
                assert request(f"{GATEWAY}/nar/mock.nar")[0] == 404, "gateway unexpectedly proxies NAR"
                with sqlite3.connect(database) as db:
                    count = db.execute("SELECT COUNT(*) FROM build_reports WHERE user_id=? AND nar_size=?",
                                       ("alice@example.com", NAR_SIZE)).fetchone()[0]
                    assert count == 1
                print("HTTP slice PASS: real SQLite/registry/gateway; missing/empty identity=401; invalid URL=400; legacy+duplicates=404; N=2 distinct users=200; dissent/tie=200; direct upstream NAR URL preserved.")
                print("MOCK upstream only; real Tailscale identity and real Nix builds NOT verified.")
    finally:
        for child in reversed(children):
            if child.poll() is None:
                child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        cache.shutdown()
        cache.server_close()


if __name__ == "__main__":
    main()
