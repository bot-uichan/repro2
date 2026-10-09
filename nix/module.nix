{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.repro2-sender;
  safePath = path: builtins.match "(/[A-Za-z0-9._-]+)+" path != null
    && !(builtins.any (part: part == "." || part == "..") (lib.splitString "/" path));
  endpoint = value: value != null
    && builtins.match "https?://[A-Za-z0-9][A-Za-z0-9.-]*(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?" value != null
    && !(builtins.any (part: part == "." || part == "..") (lib.splitString "/" value));
  sender = "${cfg.package}/bin/repro2-sender";
  hook = pkgs.writeShellScript "repro2-post-build-hook" ''
    # Do not let launch/loader/signal failures fail an otherwise successful build.
    ${lib.escapeShellArgs [ sender "--spool" cfg.spoolDirectory "--gc-roots" cfg.gcRootsDirectory "hook" ]}
    status=$?
    if [ "$status" -ne 0 ]; then
      printf '%s\n' "CRITICAL repro2 post-build hook could not record job (exit $status); publication and GC retention NOT guaranteed" >&2
    fi
    exit 0
  '';
in
{
  options.services.repro2-sender = {
    enable = lib.mkEnableOption "automatic publication of local Nix build outputs";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.repro2-sender;
      defaultText = lib.literalExpression "repro2.packages.\${pkgs.stdenv.hostPlatform.system}.repro2-sender";
      description = "Sender package. The default uses this flake's pinned Rust toolchain.";
    };
    registryUrl = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "https://registry-host.example-tailnet.ts.net/";
      description = "Trusted authenticated Serve registry base URL, without credentials, query or fragment. Required when enabled; only simple unencoded URL paths are supported.";
    };
    blobUrl = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "https://blob-host.example-tailnet.ts.net/";
      description = "Trusted authenticated Serve blob base URL, without credentials, query or fragment. Required when enabled; only simple unencoded URL paths are supported.";
    };
    spoolDirectory = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/repro2-sender";
      description = "Persistent root-owned private spool under /var/lib. All ancestors must be real directories without group/world writers; runtime validation rejects unsafe existing directories.";
    };
    gcRootsDirectory = lib.mkOption {
      type = lib.types.str;
      default = "/nix/var/nix/gcroots/repro2";
      description = "Private direct GC roots under the standard system Nix store's /nix/var/nix/gcroots. Alternative stores/state directories are not supported by this module.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      { assertion = pkgs.stdenv.hostPlatform.isLinux && config.nix.enable
          && (config.nix.daemon.enable or true)
          && (config.nix.daemonUser or "root") == "root";
        message = "services.repro2-sender requires Linux and an enabled root system Nix daemon."; }
      { assertion = endpoint cfg.registryUrl && endpoint cfg.blobUrl;
        message = "services.repro2-sender registryUrl and blobUrl must be HTTP(S) base URLs without credentials, whitespace, query, fragment, encoding or dot path components."; }
      { assertion = safePath cfg.spoolDirectory && lib.hasPrefix "/var/lib/" cfg.spoolDirectory;
        message = "services.repro2-sender spoolDirectory must be a normalized private path below /var/lib."; }
      { assertion = safePath cfg.gcRootsDirectory && lib.hasPrefix "/nix/var/nix/gcroots/" cfg.gcRootsDirectory;
        message = "services.repro2-sender gcRootsDirectory must be a normalized path below /nix/var/nix/gcroots."; }
      { assertion = !(builtins.any (line:
          builtins.match "[[:space:]]*post-build-hook[[:space:]]*=.*" line != null
        ) (lib.splitString "\n" config.nix.extraOptions));
        message = "services.repro2-sender conflicts with post-build-hook in nix.extraOptions; manage this setting through nix.settings instead."; }
      { assertion = config.nix.settings.post-build-hook == toString hook;
        message = "services.repro2-sender conflicts with another nix.settings.post-build-hook; remove the existing hook or disable this module. Hook composition is not implicit."; }
    ];

    # A normal-priority/forced existing hook wins the merge, then the assertion
    # rejects it instead of silently dropping either hook.
    nix.settings.post-build-hook = lib.mkDefault (toString hook);
    systemd.tmpfiles.rules = [
      "d ${cfg.spoolDirectory} 0700 root root - -"
      "d ${cfg.gcRootsDirectory} 0700 root root - -"
    ];
    systemd.services.nix-daemon.after = [ "systemd-tmpfiles-setup.service" ];
    systemd.services.nix-daemon.requires = [ "systemd-tmpfiles-setup.service" ];
    systemd.services.repro2-sender = {
      description = "repro2 resident build-output publisher";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" "nix-daemon.service" "systemd-tmpfiles-setup.service" ];
      requires = [ "nix-daemon.service" "systemd-tmpfiles-setup.service" ];
      environment.SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
      serviceConfig = {
        Type = "simple";
        User = "root";
        Group = "root";
        UMask = "0077";
        ExecStart = lib.escapeShellArgs [ sender "--spool" cfg.spoolDirectory
          "--gc-roots" cfg.gcRootsDirectory "run"
          "--registry-url" (if cfg.registryUrl == null then "" else cfg.registryUrl)
          "--blob-url" (if cfg.blobUrl == null then "" else cfg.blobUrl)
          "--nix" "${config.nix.package}/bin/nix" "--store" "daemon" ];
        Restart = "on-failure";
        RestartSec = "5s";
        TimeoutStopSec = "150s";
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectHome = true;
        ProtectSystem = "strict";
        ReadWritePaths = [ cfg.spoolDirectory cfg.gcRootsDirectory ];
        RestrictSUIDSGID = true;
      };
    };
  };
}
