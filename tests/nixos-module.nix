# Evaluate with: nix eval --json .#checks.x86_64-linux.nixos-module.passthru.results
{ self, nixpkgs, system }:
let
  lib = nixpkgs.lib;
  pkgs = import nixpkgs { inherit system; };
  evaluate = extra: lib.nixosSystem {
    inherit system;
    modules = [
      self.nixosModules.default
      {
        system.stateVersion = "26.05";
        boot.isContainer = true;
      }
      extra
    ];
  };
  disabled = (evaluate { }).config;
  enabled = (evaluate {
    services.repro2-sender = {
      enable = true;
      registryUrl = "https://registry.example.test:8443/";
      blobUrl = "https://blobs.example.test:8444/";
    };
  }).config;
  invalid = overrides:
    let
      result = evaluate {
        services.repro2-sender = {
          enable = true;
          registryUrl = "https://registry.example.test/";
          blobUrl = "https://blobs.example.test/";
        } // overrides;
      };
    in builtins.any (a: !a.assertion) result.config.assertions;
  conflict = (evaluate {
    services.repro2-sender = {
      enable = true;
      registryUrl = "https://registry.example.test/";
      blobUrl = "https://blobs.example.test/";
    };
    nix.settings.post-build-hook = "/nix/store/existing-hook";
  }).config;
  rawConflict = (evaluate {
    services.repro2-sender = {
      enable = true;
      registryUrl = "https://registry.example.test/";
      blobUrl = "https://blobs.example.test/";
    };
    nix.extraOptions = "post-build-hook = /nix/store/existing-hook";
  }).config;
  daemonDisabled = (evaluate {
    services.repro2-sender = {
      enable = true;
      registryUrl = "https://registry.example.test/";
      blobUrl = "https://blobs.example.test/";
    };
    nix.daemon.enable = false;
  }).config;
  nonRootDaemon = (evaluate {
    services.repro2-sender = {
      enable = true;
      registryUrl = "https://registry.example.test/";
      blobUrl = "https://blobs.example.test/";
    };
    nix.daemonUser = "nixdaemon";
    nix.daemonGroup = "nixdaemon";
  }).config;
  sender = enabled.systemd.services.repro2-sender;
  package = self.packages.${system}.repro2-sender;
  results = {
    disabledIsInert = !(disabled.systemd.services ? repro2-sender)
      && !(disabled.nix.settings ? post-build-hook)
      && !(builtins.any (r: lib.hasInfix "repro2" r) disabled.systemd.tmpfiles.rules);
    defaultsHaveNoEndpoints = disabled.services.repro2-sender.registryUrl == null
      && disabled.services.repro2-sender.blobUrl == null;
    enabledSystemEvaluates = lib.hasPrefix "/nix/store/" enabled.system.build.toplevel.drvPath;
    enabledHookIsStorePath = lib.hasPrefix "/nix/store/" enabled.nix.settings.post-build-hook;
    unitUsesAbsoluteBinaries = lib.hasInfix (builtins.unsafeDiscardStringContext "${package}/bin/repro2-sender") sender.serviceConfig.ExecStart
      && lib.hasInfix (builtins.unsafeDiscardStringContext "${enabled.nix.package}/bin/nix") sender.serviceConfig.ExecStart
      && lib.hasInfix "--store" sender.serviceConfig.ExecStart
      && lib.hasInfix "daemon" sender.serviceConfig.ExecStart;
    residentRootWithPrivateState = sender.serviceConfig.User == "root"
      && sender.serviceConfig.UMask == "0077"
      && sender.serviceConfig.Restart == "on-failure"
      && builtins.elem "multi-user.target" sender.wantedBy
      && builtins.elem "d /var/lib/repro2-sender 0700 root root - -" enabled.systemd.tmpfiles.rules
      && builtins.elem "d /nix/var/nix/gcroots/repro2 0700 root root - -" enabled.systemd.tmpfiles.rules;
    hookDirectoriesBeforeDaemon = builtins.elem "systemd-tmpfiles-setup.service" enabled.systemd.services.nix-daemon.after;
    endpointArguments = lib.hasInfix "https://registry.example.test:8443/" sender.serviceConfig.ExecStart
      && lib.hasInfix "https://blobs.example.test:8444/" sender.serviceConfig.ExecStart;
    senderDoesNotChangeConsumerTrust = enabled.nix.settings.require-sigs == disabled.nix.settings.require-sigs
      && enabled.nix.settings.substituters == disabled.nix.settings.substituters
      && enabled.nix.settings.trusted-public-keys == disabled.nix.settings.trusted-public-keys;
    rawExistingHookRejected = builtins.any (a: !a.assertion && lib.hasInfix "post-build-hook" a.message) rawConflict.assertions;
    existingHookRejected = builtins.any (a: !a.assertion && lib.hasInfix "post-build-hook" a.message) conflict.assertions;
    disabledDaemonRejected = builtins.any (a: !a.assertion && lib.hasInfix "root system Nix daemon" a.message) daemonDisabled.assertions;
    nonRootDaemonRejected = builtins.any (a: !a.assertion && lib.hasInfix "root system Nix daemon" a.message) nonRootDaemon.assertions;
    missingEndpointsRejected = invalid { registryUrl = null; blobUrl = null; };
    credentialsRejected = invalid { registryUrl = "https://user:secret@example.test/"; };
    queryRejected = invalid { blobUrl = "https://example.test/?token=secret"; };
    fragmentRejected = invalid { blobUrl = "https://example.test/#secret"; };
    nonHttpRejected = invalid { registryUrl = "file:///etc/passwd"; };
    whitespaceRejected = invalid { registryUrl = "https://example.test/\n"; };
    relativeSpoolRejected = invalid { spoolDirectory = "relative"; };
    traversalRejected = invalid { spoolDirectory = "/var/lib/../unsafe"; };
    unsafeSpoolRejected = invalid { spoolDirectory = "/tmp/repro2-sender"; };
    nonGcRootRejected = invalid { gcRootsDirectory = "/var/lib/repro2-roots"; };
  };
  failures = lib.attrNames (lib.filterAttrs (_: passed: !passed) results);
in
assert lib.assertMsg (failures == [ ]) "NixOS module tests failed: ${lib.concatStringsSep ", " failures}";
pkgs.runCommand "repro2-nixos-module-tests" { passthru = { inherit results; }; } ''
  touch "$out"
''
