# Export real evaluated module scripts/units for isolated lifecycle tests.
{ self, nixpkgs, system }:
let
  evaluate = imported: enabled: roots: nixpkgs.lib.nixosSystem {
    inherit system;
    modules = (nixpkgs.lib.optional imported self.nixosModules.default) ++ [ {
      system.stateVersion = "26.05";
      boot.isContainer = true;
    } ] ++ (nixpkgs.lib.optional enabled {
      services.repro2-sender = {
        enable = true;
        registryUrl = "https://registry.example.test/";
        blobUrl = "https://blobs.example.test/";
        gcRootsDirectory = roots;
      };
    });
  };
  config = (evaluate true true "/nix/var/nix/gcroots/repro2").config;
  custom = (evaluate true true "/nix/var/nix/gcroots/custom/repro2").config;
in {
  script = config.system.build.repro2-lifecycle;
  customScript = custom.system.build.repro2-lifecycle;
  units = config.systemd.units;
  disabledUnits = (evaluate true false "").config.systemd.units;
  removedUnits = (evaluate false false "").config.systemd.units;
}
