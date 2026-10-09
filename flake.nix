{
  description = "repro2 NAR publisher, NixOS sender module and Rust devShell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    {
      nixosModules.default = import ./nix/module.nix { inherit self; };
      nixosModules.repro2-sender = self.nixosModules.default;
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
      in
      {
        devShells.default =
          with pkgs;
          mkShell {
            buildInputs = [
              rust-bin.stable.latest.default
              openssl
              pkg-config
            ];
            shellHook = ''
              export PATH="$HOME/.cargo/bin:$PATH"
              export DATABASE_URL="sqlite://db.sqlite?mode=rwc"
            '';
          };
      }
      // nixpkgs.lib.optionalAttrs (nixpkgs.lib.hasSuffix "-linux" system) {
        packages.repro2-sender = pkgs.callPackage ./nix/package.nix { };
        packages.default = self.packages.${system}.repro2-sender;
        checks.nixos-module = import ./tests/nixos-module.nix {
          inherit self nixpkgs system;
        };
      }
    );
}
