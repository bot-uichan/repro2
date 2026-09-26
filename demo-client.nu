#!/usr/bin/env nu

def main [output_path: string] {
    let client_store = (^mktemp -d /tmp/repro2-demo-client-store-XXXXXX | str trim)

    ^nix build --store $client_store --substituters http://127.0.0.1:3000 --option require-sigs false --no-link -v $output_path
    ^nix path-info --store $client_store $output_path
}
