{
  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    fenix = {
          url = "github:nix-community/fenix";
          inputs.nixpkgs.follows = "nixpkgs";
        };
  };

  outputs = { self, flake-utils, nixpkgs, crane, fenix }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = (import nixpkgs) {
          inherit system;
        };

        makePackage = rustTarget:
          args@{ ccPackage, nativeBuildInputs ? [ ], depsBuildBuild ? [ ], ...
          }:
          let
            toolchain = let
              fenixPkgs = fenix.packages.${system};
              fenixToolchain = fenixTarget:
                (builtins.getAttr "toolchainOf" fenixTarget) {
                  channel = "1.80.0";
                  sha256 =
                    "sha256-6eN/GKzjVSjEhGO9FhWObkRFaE1Jf+uqMSdQnb8lcB4=";
                };
            in fenixPkgs.combine [
              (fenixToolchain fenixPkgs).rustc
              (fenixToolchain fenixPkgs).rustfmt
              (fenixToolchain fenixPkgs).cargo
              (fenixToolchain fenixPkgs).clippy
              (fenixToolchain (fenixPkgs.targets).${rustTarget}).rust-std
            ];
             craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;
          in craneLib.buildPackage {
            src = craneLib.cleanCargoSource ./.;
            strictDeps = true;
            doCheck = false;
            release = true;
            nativeBuildInputs = nativeBuildInputs ++ [ ccPackage ];
            depsBuildBuild = depsBuildBuild ++ [ ccPackage ];
            TARGET_CC = "${ccPackage}/bin/${ccPackage.targetPrefix}cc";
            CARGO_BUILD_TARGET = rustTarget;
            postInstall = ''
              cd "$out"/bin
              for f in "$(ls)"; do
                if ext="$(echo "$f" | grep -oP '\.[a-z]+$')"; then
                  base="$(echo "$f" | cut -d. -f1)"
                  mv "$f" "$base-${rustTarget}$ext"
                else
                  mv "$f" "$f-${rustTarget}"
                fi
              done
            '';

          } // args;
        targets = {
          x86_64-unknown-linux-musl = {
            ccPackage = pkgs.pkgsStatic.stdenv.cc;
            CARGO_BUILD_RUSTFLAGS = [
              "-C"
              "target-feature=+crt-static"
              "-C"
              "link-args=-static -latomic"
            ];
          };
#          x86_64-pc-windows-gnu = {
#            ccPackage = pkgs.pkgsCross.mingwW64.stdenv.cc;
#            CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS =
#              "-L native=${pkgs.pkgsCross.mingwW64.windows.pthreads}/lib";
#
#            depsBuildBuild = with pkgs; [ pkgsCross.mingwW64.windows.pthreads ];
#          };
        };
      in rec {
        packages =
          (nixpkgs.lib.mapAttrs (name: value: (makePackage name value)) targets
            // {
              release = pkgs.runCommand "release-binaries" { } ''
                mkdir -p "$out"/bin
                for pkg in ${
                  builtins.concatStringsSep " " (map (p: "${p}/bin")
                    (builtins.attrValues
                      (builtins.removeAttrs packages [ "release" ])))
                }; do
                  cp -r "$pkg"/* "$out"/bin/
                done
                (cd "$out"/bin && sha256sum * > sha256.txt)
              '';
            });
      });
}
