{ lib, rustPlatform, bash, fakeroot, cacert }:
rustPlatform.buildRustPackage {
  pname = "repro2-sender";
  version = "0.1.0";

  # Keep the workspace manifests/lock together; only build the sender binary.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../examples/repro2-post-build-hook
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [ "-p" "builder" "--bin" "repro2-sender" ];
  cargoTestFlags = [
    "-p" "builder" "-p" "nar-metadata"
    # Nix sandbox / can be owned by an unmapped UID, unlike the deployed host.
    # Model the root service's ownership only while running test executables.
    # Permission/symlink checks remain active; the installed binary is unchanged.
    "--config" "target.'cfg(unix)'.runner='${fakeroot}/bin/fakeroot'"
  ];
  postPatch = ''
    # Existing process-test fixtures must use a store shell in the sandbox.
    substituteInPlace crates/builder/tests/hook.rs crates/builder/tests/sender.rs \
      --replace-fail /usr/bin/bash ${bash}/bin/bash
  '';
  preBuild = ''
    # Cargo may create its home even with vendored/offline dependencies.
    export CARGO_HOME="$NIX_BUILD_TOP/cargo-home"
    mkdir -m 0700 "$CARGO_HOME"
  '';
  preCheck = ''
    # Client construction loads CA roots even for local HTTP-only test endpoints.
    # Never rely on the host's /etc/ssl/certs leaking into the build environment.
    export SSL_CERT_FILE="${cacert}/etc/ssl/certs/ca-bundle.crt"
    # Queue tests intentionally reject writable ancestors (including /build).
    chmod go-w "$NIX_BUILD_TOP"
    export TMPDIR="$NIX_BUILD_TOP/repro2-tests"
    mkdir -m 0700 "$TMPDIR"
  '';

  meta = {
    description = "Durable Nix post-build queue and resident NAR publisher";
    mainProgram = "repro2-sender";
    platforms = lib.platforms.linux;
  };
}
