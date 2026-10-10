{ self, nixpkgs, system }:
let
  pkgs = import nixpkgs { inherit system; };
  evaluated = import ./lifecycle-eval.nix { inherit self nixpkgs system; };
  guardians = builtins.filter (name: nixpkgs.lib.hasPrefix "repro2-gc-guardian-" name)
    (builtins.attrNames evaluated.units);
  guardian = builtins.head guardians;
in
assert builtins.length guardians == 1;
assert !(evaluated.disabledUnits ? ${guardian});
assert !(evaluated.removedUnits ? ${guardian});
pkgs.runCommand "repro2-nixos-lifecycle-tests" {
  nativeBuildInputs = [ pkgs.python3 ];
  passthru = { inherit (evaluated) script customScript; };
} ''
  # A nested process chroot needs uid 0. fakeroot cannot implement os.chroot.
  # Run the runtime fixture suite with nix-chroot --exec; this derivation only
  # validates generated Python syntax without pretending to boot systemd.
  python3 -c 'import ast; ast.parse(open("${evaluated.script}").read())'
  mkdir -p "$out"
  ln -s ${pkgs.python3}/bin/python3 "$out/python3"
  ln -s ${pkgs.systemd}/bin/systemd-analyze "$out/systemd-analyze"
  cp ${evaluated.script} "$out/lifecycle.py"
  cp ${evaluated.customScript} "$out/custom-lifecycle.py"
  cp ${./lifecycle.py} "$out/test-lifecycle.py"
  cp ${evaluated.units.${guardian}.unit}/${guardian} "$out/${guardian}"
  cp ${evaluated.units."repro2-sender.service".unit}/repro2-sender.service "$out/repro2-sender.service"
''
