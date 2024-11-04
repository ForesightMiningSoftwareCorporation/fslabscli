PHONY: build-artifacts
build-artifacts:
    nix flake show --json | jq  '.packages."x86_64-linux"|keys[]'| xargs -I {} nix build .#{}