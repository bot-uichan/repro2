#!/usr/bin/env python3
"""Opt-in REAL Nix build/hook/GC/readers/copy in isolated diverted local stores.
Run: REPRO2_REAL_NIX=/absolute/path/to/nix python3 tests/nix_sender_slice.py
No host installation, daemon, config, authentication, or live service changes.
TEST-ONLY proxy injects Alice identity; N=1 is deliberate for this one-builder smoke.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import threading
from http.server import ThreadingHTTPServer
from urllib.request import Request
from sender_slice import TestIdentityProxy, SENDER, wait_for
from http_slice import HTTP, ROOT, REGISTRY, GATEWAY, ready, request


def main():
    binary = os.environ.get("REPRO2_REAL_NIX")
    assert binary and Path(binary).is_absolute(), "set REPRO2_REAL_NIX to an actual Nix executable/launcher"
    children = []
    proxy = None
    try:
        with tempfile.TemporaryDirectory(prefix="real-nix-slice-", dir=ROOT / "target") as directory:
            tmp = Path(directory)
            local = tmp / "store"
            store = f"local?root={local}"
            common_nix = [binary, "--extra-experimental-features", "nix-command", "--option", "build-users-group", "", "--option", "build-hook", "", "--option", "sandbox", "false", "--option", "substituters", ""]
            def nix(args, target=store):
                return subprocess.run([*common_nix, "--store", target, *args], check=True, capture_output=True, text=True).stdout
            print(nix(["--version"]).strip())
            nix(["store", "info"])
            spool, roots = tmp / "spool", local / "nix/var/nix/gcroots/repro2"
            for path in (spool, roots):
                path.mkdir(mode=0o700, parents=True)
            hook = tmp / "hook"
            hook.write_text("#!/usr/bin/bash\n" + shlex.join([str(SENDER), "--spool", str(spool), "--gc-roots", str(roots), "hook"]) + "\nexit 0\n")
            hook.chmod(0o700)
            expr = 'let dep = builtins.toFile "real-reference" "dependency bytes"; in builtins.derivation { name = "real-repro2-multi"; system = "x86_64-linux"; builder = "/usr/bin/bash"; outputs = [ "out" "dev" ]; testRoot = ' + json.dumps(str(local)) + '; args = [ "-c" "printf \'built with %s\\n\' ${dep} > $testRoot$out; printf dev-output > $testRoot$dev" ]; }'
            result = json.loads(nix(["--option", "post-build-hook", str(hook), "build", "--no-link", "--json", "--expr", expr]))[0]
            drv, outputs = result["drvPath"], result["outputs"]
            assert len(list(spool.glob("*/job.json"))) == 1, "real Nix did not automatically invoke hook"
            assert len(list(roots.glob("*/*"))) == 3, "not all multioutputs/derivation retained"
            # No Nix process/temp-root protects the outputs now: exercise actual collection.
            nix(["store", "gc"])
            infos = json.loads(nix(["path-info", "--recursive", "--json", *outputs.values()]))
            assert drv in json.loads(nix(["path-info", "--json", drv]))
            assert len(infos) == 3, infos
            dependency = next(path for path in infos if path not in outputs.values())
            TestIdentityProxy.blocked = True
            TestIdentityProxy.calls = []
            port_probe = ThreadingHTTPServer(("127.0.0.1", 0), TestIdentityProxy)
            blob_port = port_probe.server_address[1]
            port_probe.server_close()
            env = {**os.environ, "DATABASE_URL": f"sqlite://{tmp}/reports.sqlite?mode=rwc", "REGISTRY_URL": REGISTRY,
                   "REQUIRED_USERS": "1", "BLOB_BASE_URL": f"http://127.0.0.1:{blob_port}/",
                   "FILE_SERVER_ROOT": str(tmp / "blobs"), "FILE_SERVER_BIND": f"127.0.0.1:{blob_port}"}
            subprocess.run([str(ROOT / "target/debug/migration"), "up"], env=env, check=True, stdout=subprocess.DEVNULL)
            with (tmp / "services.log").open("w+") as logs:
                for name, url in (("registry", f"{REGISTRY}/nar-info/{'0'*32}"), ("file-server", f"{env['BLOB_BASE_URL']}nar/{'0'*64}.nar"), ("gateway", f"{GATEWAY}/nix-cache-info")):
                    child = subprocess.Popen([str(ROOT / "target/debug" / name)], env=env, stdout=logs, stderr=logs)
                    children.append(child)
                    ready(child, url)
                TestIdentityProxy.blob = env["BLOB_BASE_URL"].rstrip("/")
                proxy = ThreadingHTTPServer(("127.0.0.1", 0), TestIdentityProxy)
                threading.Thread(target=proxy.serve_forever, daemon=True).start()
                base = f"http://127.0.0.1:{proxy.server_address[1]}"
                run = [str(SENDER), "--spool", str(spool), "--gc-roots", str(roots), "run", "--registry-url", base + "/registry", "--blob-url", base + "/blob", "--nix", binary, "--store", store]
                worker = subprocess.Popen(run, env=env, stdout=logs, stderr=logs)
                children.append(worker)
                wait_for(lambda: bool(list(spool.glob("*/retry.json"))) or worker.poll() is not None, "real Nix reader never reached retry")
                if worker.poll() is not None:
                    logs.flush()
                    raise AssertionError((tmp / "services.log").read_text())
                assert list(spool.glob("*/retry.json")), (tmp / "services.log").read_text()
                worker.kill()
                worker.wait(timeout=5)
                assert list(spool.glob("*/job.json")), "SIGKILL lost persistent job"
                TestIdentityProxy.blocked = False
                worker = subprocess.Popen(run, env=env, stdout=logs, stderr=logs)
                children.append(worker)
                try:
                    wait_for(lambda: not list(spool.glob("*/job.json")), "real Nix restarted worker failed to drain", timeout=90)
                except AssertionError:
                    logs.flush()
                    raise AssertionError((tmp / "services.log").read_text())
                assert not list(roots.iterdir())
                for name, path in outputs.items():
                    hash_part = Path(path).name.split("-", 1)[0]
                    rows = json.loads(request(f"{REGISTRY}/nar-info/{hash_part}")[1])
                    assert len(rows) == 1 and rows[0]["output_name"] == name
                    assert rows[0]["metadata"]["references"] == infos[path]["references"]
                    assert rows[0]["metadata"]["deriver"] == drv
                    assert request(f"{GATEWAY}/{hash_part}.narinfo")[0] == 200
                    with HTTP.open(env["BLOB_BASE_URL"] + "nar/" + rows[0]["artifact"]["file_hash"] + ".nar") as response:
                        assert hashlib.sha256(response.read()).hexdigest() == rows[0]["artifact"]["file_hash"]
                dep_hash = Path(dependency).name.split("-", 1)[0]
                assert request(f"{REGISTRY}/nar-info/{dep_hash}")[0] == 404, "substituted dependency fabricated a build vote"
                # Reference-free dev output is downloadable/importable without another path's votes.
                destination = f"local?root={tmp / 'consumer'}"
                nix(["--option", "require-sigs", "false", "copy", "--from", GATEWAY, "--to", destination, outputs["dev"]], target=destination)
                imported = json.loads(nix(["path-info", "--json", outputs["dev"]], target=destination))
                assert imported[outputs["dev"]]["narHash"] == infos[outputs["dev"]]["narHash"]
                assert (tmp / "consumer" / outputs["dev"].lstrip("/")).read_text() == "dev-output"
                # Policy is per-path: out's dependency lacks votes, so complete closure import must fail.
                blocked = subprocess.run([*common_nix, "--store", destination, "--option", "require-sigs", "false", "copy", "--from", GATEWAY, "--to", destination, outputs["out"]], text=True, capture_output=True)
                assert blocked.returncode != 0 and dep_hash in blocked.stderr, blocked.stderr
                worker.terminate()
                worker.wait(timeout=5)
                # Nix 2.34.8 fixed-CA v4 outputs have {method, hash}, not path.
                # Exercise the actual resolver, NAR dump, publication and acknowledgement.
                fixed_expr = 'builtins.derivation { name = "real-repro2-fixed-ca"; system = "x86_64-linux"; builder = "/usr/bin/bash"; outputHashAlgo = "sha256"; outputHashMode = "flat"; outputHash = builtins.hashString "sha256" "fixed bytes"; testRoot = ' + json.dumps(str(local)) + '; args = [ "-c" "printf \'fixed bytes\' > $testRoot$out" ]; }'
                fixed = json.loads(nix(["--option", "post-build-hook", str(hook), "build", "--no-link", "--json", "--expr", fixed_expr]))[0]
                fixed_json = json.loads(nix(["derivation", "show", fixed["drvPath"]]))
                assert fixed_json["version"] == 4
                fixed_spec = fixed_json["derivations"][Path(fixed["drvPath"]).name]["outputs"]["out"]
                assert set(fixed_spec) == {"method", "hash"}, fixed_spec
                subprocess.run([*run, "--once"], env=env, stdout=logs, stderr=logs, check=True)
                fixed_out = fixed["outputs"]["out"]
                fixed_hash = Path(fixed_out).name.split("-", 1)[0]
                fixed_rows = json.loads(request(f"{REGISTRY}/nar-info/{fixed_hash}")[1])
                assert len(fixed_rows) == 1 and fixed_rows[0]["output_name"] == "out"
                assert not list(spool.glob("*/job.json")) and not list(roots.iterdir())
                # Re-enqueue a distinct job over an already delivered closure. A real
                # Nix wrapper rejects ANY dump, so HEAD presence must precede it.
                subprocess.run([str(SENDER), "--spool", str(spool), "--gc-roots", str(roots), "hook"],
                               env={**env, "DRV_PATH": drv, "OUT_PATHS": " ".join(outputs.values())}, check=True)
                no_dump = tmp / "nix-no-dump"
                no_dump.write_text("#!/usr/bin/bash\ncase \"$*\" in *dump-path*) exit 99;; esac\nexec " + shlex.quote(binary) + " \"$@\"\n")
                no_dump.chmod(0o700)
                replay = [str(no_dump) if item == binary else item for item in run]
                # Only replace --nix's value; the selected binary is not the sender.
                replay[replay.index("--nix") + 1] = str(no_dump)
                for info in infos.values():
                    # Check the actual server/proxy capability, not just claimed size.
                    digest = info["narHash"]

                    key = base64.b64decode(digest.removeprefix("sha256-")).hex()
                    with HTTP.open(Request(base + "/blob/nar/" + key + ".nar", method="HEAD")) as response:
                        assert response.headers.get_all("X-Repro2-Durable") == ["file-and-parent-v1"]
                        assert int(response.headers["Content-Length"]) == info["narSize"]
                before = len(TestIdentityProxy.calls)
                subprocess.run([*replay, "--once"], env=env, stdout=logs, stderr=logs, check=True)
                replay_calls = TestIdentityProxy.calls[before:]
                assert sum(method == "HEAD" for method, _ in replay_calls) == len(infos)
                assert not any(method == "PUT" for method, _ in replay_calls), replay_calls
                assert not list(spool.glob("*/job.json")) and not list(roots.iterdir())
                nix(["store", "gc"])
                absent = subprocess.run([*common_nix, "--store", store, "path-info", outputs["dev"]], capture_output=True)
                assert absent.returncode != 0, "delivered outputs remain artificially GC rooted"
                print("REAL Nix slice PASS: 2.34.8 build automatically invoked hook; multioutput/deriver/references; actual GC retention; real path-info/dump; SIGKILL/restart/backoff; actual registry/file-server/gateway; named fixed-CA output resolved by actual Nix and acknowledged; pre-existing closure delivered with dump forbidden and zero PUTs; dev output imported via nix copy with matching hash/content; GC releases after success.")
                print("BOUNDARY: isolated diverted local stores + host Bash builder; no daemon; TEST-ONLY identity proxy; N=1 smoke, no independent rebuild/auth proof. Dependency blob delivered but full out closure correctly blocked by its missing per-path votes. Consumer explicitly trusts unsigned gateway (require-sigs=false).")
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
