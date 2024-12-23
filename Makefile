PHONY: build-artifacts
build-artifacts:
	nix flake show
	nix build .#release

publish: build-artifacts
	echo 'Publishing'
