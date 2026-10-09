#!/usr/bin/env python3
"""Real SQLite + registry + gateway + file-server; injected LOCAL identity only.
Generates a regular-file NAR using the wire encoding, not a Nix command.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
from urllib.error import HTTPError
from urllib.request import Request
from http_slice import HTTP, ROOT, HASH, STORE, REGISTRY, GATEWAY, ready, request


def binary_request(url, method="GET", data=None, user=None):
    headers = {"Tailscale-User-Login": user} if user else {}
    try:
        with HTTP.open(Request(url, method=method, data=data, headers=headers), timeout=3) as res:
            return res.status, res.read(), res.headers
    except HTTPError as error:
        return error.code, error.read(), error.headers


def regular_nar(contents):
    result = bytearray()
    for value in (b"nix-archive-1", b"(", b"type", b"regular", b"contents", contents, b")"):
        result.extend(struct.pack("<Q", len(value)))
        result.extend(value)
        result.extend(b"\0" * (-len(result) % 8))
    return bytes(result)


def main():
    for port in (3000, 3001):
        with socket.socket() as probe:
            probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            probe.bind(("127.0.0.1", port))
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        blob_port = probe.getsockname()[1]
    children = []
    try:
        with tempfile.TemporaryDirectory(prefix="blob-slice-", dir=ROOT / "target") as tmp:
            env = {**os.environ, "DATABASE_URL": f"sqlite://{tmp}/reports.sqlite?mode=rwc",
                   "REGISTRY_URL": REGISTRY, "REQUIRED_USERS": "2",
                   "BLOB_BASE_URL": f"http://127.0.0.1:{blob_port}/",
                   "FILE_SERVER_ROOT": f"{tmp}/blobs", "FILE_SERVER_BIND": f"127.0.0.1:{blob_port}"}
            subprocess.run([str(ROOT / "target/debug/migration"), "up"], env=env,
                           check=True, stdout=subprocess.DEVNULL)
            with open(Path(tmp) / "services.log", "w+") as logs:
                for name, url in (("registry", f"{REGISTRY}/nar-info/{HASH}"),
                                  ("file-server", f"{env['BLOB_BASE_URL']}nar/{'0' * 64}.nar"),
                                  ("gateway", f"{GATEWAY}/nix-cache-info")):
                    child = subprocess.Popen([str(ROOT / "target/debug" / name)], env=env,
                                             stdout=logs, stderr=logs)
                    children.append(child)
                    ready(child, url)
                nar = regular_nar(b"phase two real download\n")
                digest = hashlib.sha256(nar).digest()
                key = digest.hex()
                sri = "sha256-" + base64.b64encode(digest).decode()
                metadata = {"references": [STORE], "deriver": f"/nix/store/{HASH}-hello.drv"}
                artifact = {"file_hash": key, "file_size": len(nar), "compression": "none"}
                report = dict(drv_path=metadata["deriver"], output_name="out", store_path_hash=HASH,
                              store_path=STORE, nar_hash=sri, nar_size=len(nar), metadata=metadata,
                              artifact=artifact)
                blob = f"{env['BLOB_BASE_URL']}nar/{key}.nar"
                post = f"{REGISTRY}/build-reports"
                narinfo = f"{GATEWAY}/{HASH}.narinfo"
                assert request(post, {**report, "user_id": "forged"})[0] == 401
                for _ in range(3):
                    assert request(post, {**report, "artifact": None, "user_id": "forged"}, "alice@example.com")[0] == 201
                assert request(narinfo)[0] == 404, "same user's repeats counted twice"
                assert request(post, {**report, "artifact": None}, "bob@example.com")[0] == 201
                assert request(narinfo)[0] == 404, "votes invented artifact metadata"
                assert request(post, report, "alice@example.com")[0] == 201
                rows = json.loads(request(f"{REGISTRY}/nar-info/{HASH}")[1])
                assert len(rows) == 2 and {r["user_id"] for r in rows} == {"alice@example.com", "bob@example.com"}
                assert request(narinfo)[0] == 404, "missing blob was advertised"
                assert binary_request(blob, "PUT", nar, "alice@example.com")[0] == 201
                status, body = request(f"{GATEWAY}/{HASH}.narinfo")
                assert status == 200, (status, body)
                lines = dict(line.split(": ", 1) for line in body.splitlines())
                assert lines["URL"] == blob, body
                assert lines["StorePath"] == STORE
                assert lines["Compression"] == "none"
                assert lines["FileSize"] == lines["NarSize"] == str(len(nar))
                assert lines["FileHash"] == lines["NarHash"]
                assert lines["References"] == STORE.removeprefix("/nix/store/")
                assert lines["Deriver"] == metadata["deriver"].removeprefix("/nix/store/")
                assert "CA" not in lines and "Sig" not in lines
                status, downloaded, _ = binary_request(lines["URL"])
                assert status == 200 and downloaded == nar
                assert hashlib.sha256(downloaded).hexdigest() == key
                assert binary_request(blob, "HEAD")[2]["Content-Length"] == str(len(nar))
                assert request(f"{GATEWAY}/nar/{key}.nar")[0] == 404
                assert request(post, {**report, "metadata": {"references": [], "deriver": metadata["deriver"]}}, "carol@example.com")[0] == 201
                assert request(narinfo)[0] == 200, "metadata dissent vetoed a qualifying result"
                other_nar = regular_nar(b"a different result at the same IA path\n")
                other_digest = hashlib.sha256(other_nar).digest()
                other_key = other_digest.hex()
                other_blob = f"{env['BLOB_BASE_URL']}nar/{other_key}.nar"
                other = {**report, "nar_hash": "sha256-" + base64.b64encode(other_digest).decode(),
                         "nar_size": len(other_nar), "artifact": {"file_hash": other_key,
                         "file_size": len(other_nar), "compression": "none"}}
                assert binary_request(other_blob, "PUT", other_nar, "carol@example.com")[0] == 201
                assert request(post, other, "carol@example.com")[0] == 201
                assert request(post, {**other, "artifact": None}, "dan@example.com")[0] == 201
                rows = json.loads(request(f"{REGISTRY}/nar-info/{HASH}")[1])
                assert len({r["nar_hash"] for r in rows}) == 2, "IA path did not retain multiple NAR candidates"
                status, tied = request(narinfo)
                assert status == 200, "disagreement/tie halted selection"
                tied_lines = dict(line.split(": ", 1) for line in tied.splitlines())
                assert tied_lines["URL"] in (blob, other_blob)
                assert binary_request(tied_lines["URL"])[1] in (nar, other_nar)
                assert binary_request(blob, "PUT", nar, "bob@example.com")[0] == 200
                children[-1].terminate()
                children[-1].wait(timeout=5)
                restarted = subprocess.Popen([str(ROOT / "target/debug/gateway")], env=env, stdout=logs, stderr=logs)
                children.append(restarted)
                ready(restarted, f"{GATEWAY}/nix-cache-info")
                assert request(narinfo) == (status, tied), "candidate changed after gateway restart"
                print("Blob slice PASS: real SQLite/registry/gateway/file-server; auth+dedup; unpublished votes; missing artifact/blob=404; N=2; multiple IA candidates; dissent/tie=200; direct GET/HEAD+SHA256; gateway restart; no CA/Sig/proxy.")
                print("Identity headers injected locally, NOT real Serve authentication. NAR generated by wire encoding, NOT real Nix build/import/signature verification.")
    finally:
        for child in reversed(children):
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()


if __name__ == "__main__":
    main()
