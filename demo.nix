{ pkgs }:

pkgs.runCommand "repro2-cache-demo" { } ''
  printf '...repro2 cache demo ...\n' > "$out"
''
