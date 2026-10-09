#!/usr/bin/env python3
"""Resident process + real services. Fake Nix fixture, TEST-ONLY identity proxy.
No normal worker flag/environment can inject an identity.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.request import Request
from blob_slice import regular_nar
from http_slice import HTTP, ROOT, HASH, STORE, REGISTRY, GATEWAY, ready, request

SENDER = ROOT / "target/debug/repro2-sender"
DRV = f"/nix/store/{HASH}-hello.drv"
DEP = f"/nix/store/{HASH}-dependency"


def wait_for(predicate, message, timeout=15):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError(message)


class TestIdentityProxy(BaseHTTPRequestHandler):
    """ONLY the test harness injects the trusted upstream identity header."""
    blocked = True
    calls = []
    registry = REGISTRY
    blob = ""

    def log_message(self, format, *args):
        pass

    def forward(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length) if length else None
        assert not self.headers.get("Tailscale-User-Login"), "worker self-claimed identity"
        self.calls.append((self.command, self.path))
        if self.blocked:
            self.send_response(503)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        base, suffix = (self.registry, self.path[len("/registry"):]) if self.path.startswith("/registry/") else (self.blob, self.path[len("/blob"):])
        headers = {"Tailscale-User-Login": "alice@example.com"}
        if self.headers.get("Content-Type"):
            headers["Content-Type"] = self.headers["Content-Type"]
        req = Request(base + suffix, data=body, method=self.command, headers=headers)
        try:
            response = HTTP.open(req, timeout=5)
        except HTTPError as error:
            response = error
        with response:
            data = response.read()
            self.send_response(response.code)
            self.send_header("Content-Length", response.headers.get("Content-Length", str(len(data))))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(data)

    do_GET = forward
    do_HEAD = forward
    do_POST = forward
    do_PUT = forward


def fake_nix(tmp, outputs):
    infos = {}
    nars = {}
    for path in [*outputs.values(), DEP]:
        nar = regular_nar((path + "\n").encode())
        digest = hashlib.sha256(nar).digest()
        infos[path] = {"path": path, "narHash": "sha256-" + base64.b64encode(digest).decode(),
                       "narSize": len(nar), "references": [DEP] if path in outputs.values() else [],
                       "deriver": DRV if path in outputs.values() else None}
        nars[path] = base64.b64encode(nar).decode()
    fixture = tmp / "fixture.json"
    fixture.write_text(json.dumps({"infos": infos, "nars": nars, "outputs": outputs}))
    program = tmp / "fake-nix"
    program.write_text(f"#!{sys.executable}\n" + '''import sys,json,base64
v=json.load(open(''' + repr(str(fixture)) + '''))
a=sys.argv[1:]
assert '--option' in a and a[a.index('--option')+1:a.index('--option')+3] == ['post-build-hook', '']
if 'path-info' in a: print(json.dumps(v['infos']))
elif 'derivation' in a: print(json.dumps({''' + repr(DRV) + ''': {'outputs': {name: {'path': path} for name,path in v['outputs'].items()}}}))
elif 'dump-path' in a: sys.stdout.buffer.write(base64.b64decode(v['nars'][a[-1]]))
else: sys.exit('unexpected fake nix invocation: '+repr(a))
''')
    program.chmod(0o700)
    return program, infos


def main():
    children = []
    proxy = None
    try:
        with tempfile.TemporaryDirectory(prefix="sender-slice-", dir=ROOT / "target") as directory:
            tmp = Path(directory)
            spool, roots = tmp / "spool", tmp / "roots"
            for path in (spool, roots):
                path.mkdir(mode=0o700)
            outputs = {"out": STORE}
            nix, infos = fake_nix(tmp, outputs)
            blob_listener = ThreadingHTTPServer(("127.0.0.1", 0), TestIdentityProxy)
            blob_port = blob_listener.server_address[1]
            blob_listener.server_close()
            env = {**os.environ, "DATABASE_URL": f"sqlite://{tmp}/reports.sqlite?mode=rwc",
                   "REGISTRY_URL": REGISTRY, "REQUIRED_USERS": "2",
                   "BLOB_BASE_URL": f"http://127.0.0.1:{blob_port}/",
                   "FILE_SERVER_ROOT": str(tmp / "blobs"), "FILE_SERVER_BIND": f"127.0.0.1:{blob_port}"}
            subprocess.run([str(ROOT / "target/debug/migration"), "up"], env=env, check=True, stdout=subprocess.DEVNULL)
            with (tmp / "logs").open("w+") as logs:
                for name, url in (("registry", f"{REGISTRY}/nar-info/{HASH}"),
                                  ("file-server", f"{env['BLOB_BASE_URL']}nar/{'0'*64}.nar"),
                                  ("gateway", f"{GATEWAY}/nix-cache-info")):
                    child = subprocess.Popen([str(ROOT / "target/debug" / name)], env=env, stdout=logs, stderr=logs)
                    children.append(child)
                    ready(child, url)
                TestIdentityProxy.blocked = True
                TestIdentityProxy.calls = []
                TestIdentityProxy.blob = env["BLOB_BASE_URL"].rstrip("/")
                proxy = ThreadingHTTPServer(("127.0.0.1", 0), TestIdentityProxy)
                threading.Thread(target=proxy.serve_forever, daemon=True).start()
                base = f"http://127.0.0.1:{proxy.server_address[1]}"
                common = [str(SENDER), "--spool", str(spool), "--gc-roots", str(roots)]
                subprocess.run([*common, "hook"], env={**env, "DRV_PATH": DRV, "OUT_PATHS": " ".join(outputs.values()), "PATH": "/not-present"}, check=True)
                assert len(list(spool.glob("*/job.json"))) == 1
                run = [*common, "run", "--registry-url", base + "/registry", "--blob-url", base + "/blob", "--nix", str(nix)]
                worker = subprocess.Popen(run, env=env, stdout=logs, stderr=logs)
                children.append(worker)
                wait_for(lambda: bool(TestIdentityProxy.calls) or worker.poll() is not None, "worker never attempted delivery")
                assert worker.poll() is None, "resident worker exited instead of retrying"
                wait_for(lambda: bool(list(spool.glob("*/retry.json"))), "retry not persisted")
                retry = json.loads(next(spool.glob("*/retry.json")).read_text())
                assert retry["attempts"] >= 1 and retry["next_attempt"] > 0
                assert len(list(roots.glob("*/*"))) == 2, "failure released retention"
                worker.terminate()
                worker.wait(timeout=5)
                assert worker.returncode == 0, "SIGTERM not graceful"
                assert list(spool.glob("*/job.json")), "shutdown lost job"
                TestIdentityProxy.blocked = False
                worker = subprocess.Popen(run, env=env, stdout=logs, stderr=logs)
                children.append(worker)
                wait_for(lambda: not list(spool.glob("*/job.json")), "restart failed to drain retained job")
                assert not list(roots.iterdir()), "successful publication leaked roots"
                assert worker.poll() is None, "normal workflow not resident"
                rows = json.loads(request(f"{REGISTRY}/nar-info/{HASH}")[1])
                assert len(rows) == 1 and rows[0]["user_id"] == "alice@example.com", rows
                row = rows[0]
                assert row["output_name"] == "out"
                assert row["metadata"] == {"references": [DEP], "deriver": DRV}
                assert row["artifact"]["compression"] == "none"
                for path, info in infos.items():
                    key = base64.b64decode(info["narHash"].split("-",1)[1]).hex()
                    with HTTP.open(env["BLOB_BASE_URL"] + f"nar/{key}.nar") as response:
                        data = response.read()
                    assert hashlib.sha256(data).hexdigest() == key and len(data) == info["narSize"]
                assert request(f"{GATEWAY}/{HASH}.narinfo")[0] == 404, "dependency copies/retries fabricated votes"
                report = {key: row[key] for key in ("drv_path", "output_name", "store_path_hash", "store_path", "nar_hash", "nar_size", "metadata", "artifact")}
                assert request(f"{REGISTRY}/build-reports", report, "bob@example.com")[0] == 201
                assert request(f"{GATEWAY}/{HASH}.narinfo")[0] == 200
                # A running resident must discover later builds without a CLI send/restart.
                fresh = "/nix/store/00000000000000000000000000000000-fresh"
                fake_nix(tmp, {"fresh": fresh})
                subprocess.run([*common, "hook"], env={**env, "DRV_PATH": DRV, "OUT_PATHS": fresh}, check=True)
                wait_for(lambda: not list(spool.glob("*/job.json")), "resident missed newly queued build")
                fresh_rows = json.loads(request(f"{REGISTRY}/nar-info/{'0'*32}")[1])
                assert len(fresh_rows) == 1 and fresh_rows[0]["output_name"] == "fresh"
                # Still only Alice's vote; copying its dependency must not add voters.
                assert request(f"{GATEWAY}/{'0'*32}.narinfo")[0] == 404
                worker.terminate()
                worker.wait(timeout=5)
                print("Sender slice PASS: local hook; durable roots/queue; resident 503 retry; SIGTERM/restart; actual services; blob/readback/metadata; closure blobs without copied votes; N=2 still enforced.")
                print("BOUNDARY: fake Nix path-info/dump/derivation fixture; TEST-ONLY proxy identity injection, NOT real Tailscale Serve.")
    finally:
        if proxy:
            proxy.shutdown()
            proxy.server_close()
        for child in reversed(children):
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()


if __name__ == "__main__":
    main()
