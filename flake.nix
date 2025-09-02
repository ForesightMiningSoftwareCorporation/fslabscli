{
  description = "fslabscli";
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
        overlays = [ ];
        pkgs = import nixpkgs {
          inherit system overlays;
          stdenv = nixpkgs.clangStdenv;
        };
        inherit (pkgs.stdenv) isDarwin isLinux;
        inherit (gitignore.lib) gitignoreSource;
        lib = pkgs.lib;
        fenixPkgs = fenix.packages.${system};
        toolchain = fenixPkgs.fromToolchainFile {
          file = ./rust-toolchain.toml;
          sha256 = "sha256-KUm16pHj+cRedf8vxs/Hd2YWxpOrWZ7UOrwhILdSJBU=";
        };
        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
        rustSrc = craneLib.cleanCargoSource ./.;
        arch2targets =
          let
            generateCross =
              target:
              import nixpkgs {
                system = system;
                crossSystem.config = target;
              };
          in
          {
            "x86_64-linux" = rec {
              rustTarget = "x86_64-unknown-linux-musl";
              pkgsCross = generateCross rustTarget;
              depsBuildBuild = [ ];
            };
            "aarch64-darwin" = rec {
              rustTarget = "aarch64-apple-darwin";
              pkgsCross = generateCross rustTarget;
              depsBuildBuild = [ ];
            };
            "x86_64-windows" =
              let
                pkgsCross = pkgs.pkgsCross.mingwW64;
              in
              {
                inherit pkgsCross;
                rustTarget = "x86_64-pc-windows-gnu";
                depsBuildBuild = [
                  pkgsCross.stdenv.cc
                  pkgsCross.windows.pthreads
                ];
              };
          };

        generateCommonArgs = craneLib': {
          pname = manifest.name;
          version = manifest.version;
          src = rustSrc;
          nativeBuildInputs = [
            pkgs.perl
            pkgs.llvmPackages.libclang
            pkgs.clang
            pkgs.git
            pkgs.installShellFiles # Shell Completions
            pkgs.rustPlatform.bindgenHook
          ];
          buildInputs = [
            pkgs.stdenv.cc
          ]
          ++ lib.optionals isDarwin [
            pkgs.apple-sdk
            pkgs.libiconv
          ];
          # auditable = false;
          doCheck = false;
          # strictDeps = false;

          auditable = true;
          # doCheck = true;
          strictDeps = true;

          LIBCLANG_PATH = pkgs.lib.makeLibraryPath [
            pkgs.llvmPackages.libclang.lib
          ];
        };

        mkRustPackage =
          packageName:
          (craneLib.buildPackage (
            (generateCommonArgs craneLib)
            // {
            }
          ));

        mkCrossRustPackage =
          arch: packageName:
          let
            inherit (arch2targets.${arch}) rustTarget pkgsCross depsBuildBuild;
            toolchain = fenixPkgs.combine [
              fenixPkgs.stable.rustc
              fenixPkgs.stable.cargo
              fenixPkgs.targets.${rustTarget}.stable.rust-std
            ];
            craneLibCross = craneLib.overrideToolchain toolchain;
            TARGET_CC = "${pkgsCross.stdenv.cc}/bin/${pkgsCross.stdenv.cc.targetPrefix}cc";
            commonArgs = (generateCommonArgs craneLibCross) // {
              inherit depsBuildBuild TARGET_CC;

              CARGO_BUILD_TARGET = rustTarget;
              CARGO_BUILD_RUSTFLAGS = [
                "-C"
                "linker=${TARGET_CC}"
                "-C"
                "target-feature=+crt-static"
              ];

              CC = "${pkgsCross.stdenv.cc}/bin/${pkgsCross.stdenv.cc.targetPrefix}cc";
              LD = "${pkgsCross.stdenv.cc}/bin/${pkgsCross.stdenv.cc.targetPrefix}cc";
            };
            cargoArtifacts = craneLibCross.buildDepsOnly (commonArgs // { });

          in
          craneLibCross.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              CARGO_BUILD_RUSTFLAGS = [
                "-C"
                "linker=${TARGET_CC}"
                "-C"
                "target-feature=+crt-static"
              ];

            }
          );

        individualPackages =
          let
            packageName = manifest.name;
            filteredTargets = lib.attrsets.filterAttrs (
              k: _: (k == system || isDarwin || (!lib.strings.hasInfix "darwin" k))
            ) arch2targets;
          in
          lib.attrsets.mapAttrs' (
            arch: _:
            let
              shouldCross = arch != system;
            in
            lib.nameValuePair (packageName + "-" + arch) (
              if shouldCross then (mkCrossRustPackage arch packageName) else (mkRustPackage packageName)
            )
          ) filteredTargets;
      in
      {
        packages = individualPackages // {
          default = mkRustPackage "cargo-fslabscli";
          release = pkgs.runCommand "release-binaries" { } ''
            mkdir -p "$out/bin"
            for pkg in ${
              builtins.concatStringsSep " " (
                map (p: "${p}/bin") (builtins.attrValues (builtins.removeAttrs individualPackages [ "release" ]))
              )
            }; do
              for file in "$pkg"/*; do
                install -Dm755 "$file" "$out/bin/$(basename "$file")"
              done
            done
            (cd "$out/bin" && sha256sum * > sha256.txt)
          '';
        };
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
                packages = with pkgs; [
                  # self.packages.${system}.default
                  # updatecli
                  cargo-deny
                  rustup
                  xunit-viewer
                  protobuf
                ];
                languages = {
                  nix.enable = true;
                  rust = {
                    enable = true;
                  };
                };

                enterShell = ''
                  [ ! -f .env ] || export $(grep -v '^#' .env | xargs)
                  echo 👋 Welcome to fslabscli Development Environment. 🚀
                  echo
                  echo If you see this message, it means your are inside the Nix shell ❄️.
                  echo
                  echo ------------------------------------------------------------------
                  echo
                  echo Commands: available
                  ${pkgs.gnused}/bin/sed -e 's| |••|g' -e 's|=| |' <<EOF | ${pkgs.util-linuxMinimal}/bin/column -t | ${pkgs.gnused}/bin/sed -e 's|^|💪 |' -e 's|••| |g'
                  ${lib.generators.toKeyValue { } (lib.mapAttrs (name: value: value.description) config.scripts)}
                  EOF
                  echo
                  echo Repository:
                  echo  - https://github.com/ForesightMiningSoftwareCorporation/fslabscli
                  echo ------------------------------------------------------------------
                  echo
                '';
              }
            )
          ];
        };
      }
    );
}
