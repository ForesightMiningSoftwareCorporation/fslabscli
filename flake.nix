{
  description = "fslabscli - A CLI tool for FSLABS CI/CD operations";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    fenix.url = "github:nix-community/fenix";
    flake-utils.url = "github:numtide/flake-utils";
    gitignore.url = "github:hercules-ci/gitignore.nix";
    devenv.url = "github:cachix/devenv";
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      flake-utils,
      fenix,
      crane,
      gitignore,
      devenv,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        # Use standard stdenv (better compatibility with native deps)
        pkgs = import nixpkgs { inherit system; };
        inherit (pkgs.stdenv) isDarwin isLinux;
        inherit (gitignore.lib) gitignoreSource;
        lib = pkgs.lib;

        # Extract Rust channel from rust-toolchain.toml
        rustToolchainToml = lib.importTOML ./rust-toolchain.toml;
        rustChannel = rustToolchainToml.toolchain.channel;

        # Rust toolchain from rust-toolchain.toml
        fenixPkgs = fenix.packages.${system};
        toolchain = fenixPkgs.fromToolchainName {
          name = (lib.importTOML ./rust-toolchain.toml).toolchain.channel;
          sha256 = "sha256-+9FmLhAOezBZCOziO0Qct1NOrfpjNsXxc/8I0c7BdKE=";
        };

        # Crane library for building Rust projects
        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

        # Package metadata
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
        rustSrc = craneLib.cleanCargoSource ./.;

        # Helper to create Rust target toolchain
        mkToolchain =
          target:
          fenixPkgs.combine [
            toolchain.rustc
            toolchain.cargo
            fenixPkgs.targets.${target}.stable.rust-std
          ];

        # Native build for current system
        nativeTarget =
          if system == "x86_64-linux" then
            "x86_64-unknown-linux-musl"
          else if system == "aarch64-linux" then
            "aarch64-unknown-linux-musl"
          else if system == "x86_64-darwin" then
            "x86_64-apple-darwin"
          else if system == "aarch64-darwin" then
            "aarch64-apple-darwin"
          else
            throw "Unsupported system: ${system}";

        # Common args for all builds
        mkCommonArgs =
          {
            target,
            pkgsCross ? pkgs,
          }:
          {
            pname = manifest.name;
            version = manifest.version;
            src = rustSrc;
            strictDeps = true;
            doCheck = false;
            auditable = true;

            CARGO_BUILD_TARGET = target;
            CARGO_BUILD_RUSTFLAGS =
              if lib.hasInfix "linux-musl" target then
                [
                  "-C"
                  "target-feature=+crt-static"
                  "-C"
                  "link-arg=-static"
                ]
              else if lib.hasInfix "windows" target then
                [
                  "-C"
                  "target-feature=+crt-static"
                ]
              else
                [ ];

            nativeBuildInputs =
              with pkgs;
              [
                pkg-config
                perl
                git
                installShellFiles
              ]
              ++ lib.optionals isDarwin [
                libiconv
              ];

            buildInputs =
              if lib.hasInfix "linux-musl" target then
                # Static Linux: use musl
                [ pkgs.pkgsStatic.openssl ]
              else if lib.hasInfix "darwin" target then
                # macOS: use modern apple-sdk (includes frameworks)
                [ pkgs.apple-sdk ]
              else
                [ ];
          };

        # Build a Rust package for a specific target
        mkPackage =
          {
            target,
            pkgsCross ? pkgs,
          }:
          let
            toolchainForTarget = mkToolchain target;
            craneLibForTarget = craneLib.overrideToolchain toolchainForTarget;
            commonArgs = mkCommonArgs { inherit target pkgsCross; };
            cargoArtifacts = craneLibForTarget.buildDepsOnly commonArgs;
          in
          craneLibForTarget.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;

              postInstall = ''
                cd "$out"/bin
                for f in *; do
                  if [[ -f "$f" ]]; then
                    if [[ "$f" == *.exe ]]; then
                      base="''${f%.exe}"
                      [[ "$base" =~ -${target} ]] || mv "$f" "''${base}-${target}.exe"
                    else
                      [[ "$f" =~ -${target} ]] || mv "$f" "$f-${target}"
                    fi
                  fi
                done
              '';
            }
          );

        # Cross-compilation for Windows from Linux
        mkWindowsPackage =
          let
            target = "x86_64-pc-windows-gnu";
            pkgsCross = pkgs.pkgsCross.mingwW64;
            toolchainWindows = mkToolchain target;
            craneLibWindows = craneLib.overrideToolchain toolchainWindows;

            TARGET_CC = "${pkgsCross.stdenv.cc}/bin/${pkgsCross.stdenv.cc.targetPrefix}cc";

            commonArgs = (mkCommonArgs { inherit target pkgsCross; }) // {
              depsBuildBuild = with pkgsCross; [
                stdenv.cc
                windows.pthreads
              ];

              nativeBuildInputs = (mkCommonArgs { inherit target pkgsCross; }).nativeBuildInputs ++ [
                pkgsCross.stdenv.cc
              ];

              buildInputs = [ ]; # Static linking

              CARGO_BUILD_RUSTFLAGS = [
                "-C"
                "linker=${TARGET_CC}"
                "-C"
                "target-feature=+crt-static"
              ];

              TARGET_CC = TARGET_CC;
              CC = TARGET_CC;
              HOST_CC = "${pkgs.stdenv.cc}/bin/cc";
            };

            cargoArtifacts = craneLibWindows.buildDepsOnly commonArgs;
          in
          craneLibWindows.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;

              postInstall = ''
                cd "$out"/bin
                for f in *.exe; do
                  if [[ -f "$f" ]]; then
                    base="''${f%.exe}"
                    [[ "$base" =~ -${target} ]] || mv "$f" "''${base}-${target}.exe"
                  fi
                done
              '';
            }
          );

        # Cross-compilation for Linux ARM64 from Linux x86_64
        mkLinuxAarch64Package =
          let
            target = "aarch64-unknown-linux-musl";
            pkgsCross = pkgs.pkgsCross.aarch64-multiplatform-musl;
            toolchainAarch64 = mkToolchain target;
            craneLibAarch64 = craneLib.overrideToolchain toolchainAarch64;

            TARGET_CC = "${pkgsCross.stdenv.cc}/bin/${pkgsCross.stdenv.cc.targetPrefix}cc";

            commonArgs = (mkCommonArgs { inherit target pkgsCross; }) // {
              depsBuildBuild = [
                pkgsCross.stdenv.cc
              ];

              nativeBuildInputs = (mkCommonArgs { inherit target pkgsCross; }).nativeBuildInputs ++ [
                pkgsCross.stdenv.cc
              ];

              buildInputs = [
                pkgsCross.pkgsStatic.openssl
              ];

              CARGO_BUILD_RUSTFLAGS = [
                "-C"
                "linker=${TARGET_CC}"
                "-C"
                "target-feature=+crt-static"
                "-C"
                "link-arg=-static"
              ];

              TARGET_CC = TARGET_CC;
              CC = TARGET_CC;
              HOST_CC = "${pkgs.stdenv.cc}/bin/cc";
            };

            cargoArtifacts = craneLibAarch64.buildDepsOnly commonArgs;
          in
          craneLibAarch64.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;

              postInstall = ''
                cd "$out"/bin
                for f in *; do
                  if [[ -f "$f" ]]; then
                    [[ "$f" =~ -${target} ]] || mv "$f" "$f-${target}"
                  fi
                done
              '';
            }
          );

        # Cross-compilation packages based on system
        crossPackages =
          if system == "x86_64-linux" then
            {
              # Linux runner builds: x86_64-linux, aarch64-linux, windows
              cargo-fslabscli-aarch64-linux = mkLinuxAarch64Package;
              cargo-fslabscli-windows = mkWindowsPackage;
            }
          # macOS builds only native architecture (ARM64 only)
          # x86_64-darwin is not supported (Intel Macs can use Rosetta 2)
          else
            { };

        # Native package
        nativePackage = mkPackage { target = nativeTarget; };

      in
      {
        packages = {
          default = nativePackage;
          cargo-fslabscli = nativePackage;
        }
        // crossPackages
        // {
          # Release bundle: combines native + all cross-compiled builds for this runner
          release = pkgs.runCommand "release-binaries" { } ''
            mkdir -p "$out/bin"

            # Copy native build
            for file in ${nativePackage}/bin/*; do
              if [ -f "$file" ]; then
                cp -L "$file" "$out/bin/"
              fi
            done

            # Copy all cross-compiled builds
            ${lib.concatMapStringsSep "\n" (name: ''
              for file in ${crossPackages.${name}}/bin/*; do
                if [ -f "$file" ]; then
                  cp -L "$file" "$out/bin/"
                fi
              done
            '') (lib.attrNames crossPackages)}

            # Generate checksums
            cd "$out/bin"
            if ls * >/dev/null 2>&1; then
              sha256sum * > sha256.txt
            fi
          '';
        };

        # Development shell
        devShells.default = devenv.lib.mkShell {
          inherit inputs pkgs;
          modules = [
            (
              {
                pkgs,
                config,
                lib,
                ...
              }:
              {
                # Required for devenv flake integration
                packages =
                  with pkgs;
                  [
                    # Development tools
                    cargo-deny
                    cargo-nextest
                    xunit-viewer
                    protobuf
                    trunk
                  ]
                  ++ [
                    # Use fenix's rust-analyzer
                    fenixPkgs.rust-analyzer
                  ];

                languages = {
                  nix.enable = true;
                  rust = {
                    enable = true;
                    toolchainPackage = toolchain.toolchain;
                  };
                };

                enterShell = ''
                  # Load .env if it exists
                  [ ! -f .env ] || export $(grep -v '^#' .env | xargs)

                  echo "👋 Welcome to fslabscli Development Environment 🚀"
                  echo ""
                  echo "If you see this message, it means you are inside the Nix shell ❄️"
                  echo ""
                  echo "──────────────────────────────────────────────────────────"
                  echo ""
                  echo "Commands available:"
                  ${pkgs.gnused}/bin/sed -e 's| |••|g' -e 's|=| |' <<EOF | ${pkgs.util-linuxMinimal}/bin/column -t | ${pkgs.gnused}/bin/sed -e 's|^|💪 |' -e 's|••| |g'
                  ${lib.generators.toKeyValue { } (lib.mapAttrs (name: value: value.description) config.scripts)}
                  EOF
                  echo ""
                  echo "Quick start:"
                  echo "  - cargo build              # Build the project"
                  echo "  - cargo test               # Run tests"
                  echo "  - cargo nextest run        # Run tests with nextest"
                  echo "  - nix build                # Build native (${nativeTarget})"
                  echo "  - nix build .#release      # Build all targets for this runner"
                  echo "  - nix flake check          # Run all checks"
                  echo ""
                  echo "This runner builds:"
                  ${
                    if system == "x86_64-linux" then
                      ''
                        echo "  ✓ x86_64-unknown-linux-musl (native)"
                        echo "  ✓ aarch64-unknown-linux-musl (cross)"
                        echo "  ✓ x86_64-pc-windows-gnu (cross)"
                      ''
                    else if system == "aarch64-darwin" then
                      ''
                        echo "  ✓ aarch64-apple-darwin (native only)"
                        echo "  Note: x86_64-darwin not supported (use Rosetta 2)"
                      ''
                    else
                      ''
                        echo "  ✓ ${nativeTarget} (native)"
                      ''
                  }
                  echo ""
                  echo "Repository:"
                  echo "  https://github.com/ForesightMiningSoftwareCorporation/fslabscli"
                  echo "──────────────────────────────────────────────────────────"
                  echo ""
                '';
              }
            )
          ];
        };
      }
    );
}
