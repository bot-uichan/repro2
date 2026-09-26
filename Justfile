# Local cache demo. Run recipes from the repository root.

cache_dir := "/tmp/repro2-demo-cache"
cache_url := "http://127.0.0.1:8001"

# Start each service in a separate terminal.
registry:
    nix develop -c cargo run -p registry

gateway:
    nix develop -c cargo run -p gateway

# Build .#demo, copy it to the static cache, and report it to Registry.
build-demo:
    nix develop -c cargo run -p builder -- '.#demo' --cache-dir '{{cache_dir}}' --cache-url '{{cache_url}}'

# Serve the static cache in a separate terminal.
serve-cache:
    python3 -m http.server 8001 --bind 127.0.0.1 --directory '{{cache_dir}}'

# Fetch the demo output using the empty demo Store configured by demo-client.nu.
demo-client output_path:
    nu demo-client.nu '{{output_path}}'
