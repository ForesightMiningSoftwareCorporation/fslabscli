{
  description = "fslabscli";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix.url = "github:nix-community/fenix";
    flake-utils.url = "github:numtide/flake-utils";
    naersk.url = "github:nix-community/naersk";
    gitignore.url = "github:hercules-ci/gitignore.nix";
    devenv.url = "github:cachix/devenv";
    treefmt-nix.url = "github:numtide/treefmt-nix";
  };
  outputs =
    inputs@{
      self,
      nixpkgs,
      flake-utils,
      naersk,
      fenix,
      devenv,
      gitignore,
      treefmt-nix,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        inherit (pkgs.stdenv) isDarwin;
        inherit (gitignore.lib) gitignoreSource;
        lib = pkgs.lib;
        fenixPkgs = fenix.packages.${system};
        toolchain = fenixPkgs.combine [
          fenixPkgs.stable.rustc
          fenixPkgs.stable.cargo
        ];
        naersk' = pkgs.callPackage naersk {
          rustc = toolchain;
          cargo = toolchain;
        };
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;

        rustSrc = gitignoreSource ./.;

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
        mkRustPackage =
          packageName:
          naersk'.buildPackage {
            pname = packageName;
            cargoBuildOptions =
              x:
              x
              ++ [
                "--package"
                packageName
              ];
            version = manifest.version;
            src = pkgs.lib.cleanSource ./.;
            nativeBuildInputs = [
              pkgs.perl # Needed to build vendored OpenSSL.
              pkgs.installShellFiles # Shell Completions
            ];
            buildInputs = pkgs.lib.optionals isDarwin [
              pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
            ];
            auditable = false; # Avoid cargo-auditable failures.
            doCheck = false; # Disable test as it requires network access.
            postInstall =
              let
                rustTarget = arch2targets.${system}.rustTarget;
              in
              ''
                $out/bin/${packageName} man-page > ${packageName}.man
                installManPage ${packageName}.man
                installShellCompletion --cmd ${packageName} \
                 --bash <($out/bin/${packageName} completions bash) \
                 --fish <($out/bin/${packageName} completions fish) \
                 --zsh <($out/bin/${packageName} completions zsh)

                cd "$out"/bin
                for f in "$(ls)"; do
                  if ext="$(echo "$f" | grep -oP '\.[a-z]+$')"; then
                    base="$(echo "$f" | cut -d. -f1)"
                    cp "$f" "$base-${rustTarget}$ext"
                  else
                    cp "$f" "$f-${rustTarget}"
                  fi
                done
              '';

          };

        mkCrossRustPackage =
          arch: packageName:
          let
            inherit (arch2targets.${arch}) rustTarget pkgsCross depsBuildBuild;
            toolchain = fenixPkgs.combine [
              fenixPkgs.stable.rustc
              fenixPkgs.stable.cargo
              fenixPkgs.targets.${rustTarget}.stable.rust-std
            ];
            naersk-lib = pkgs.callPackage naersk {
              cargo = toolchain;
              rustc = toolchain;
            };
          in
          naersk-lib.buildPackage rec {
            inherit depsBuildBuild;
            pname = packageName;
            cargoBuildOptions =
              x:
              x
              ++ [
                "--package"
                packageName
              ];
            version = manifest.version;
            strictDeps = true;
            src = rustSrc;
            nativeBuildInputs = [
              pkgs.perl # Needed to build vendored OpenSSL.
            ];
            auditable = false; # Avoid cargo-auditable failures.
            doCheck = false; # Disable test as it requires network access.

            CARGO_BUILD_TARGET = rustTarget;
            TARGET_CC = "${pkgsCross.stdenv.cc}/bin/${pkgsCross.stdenv.cc.targetPrefix}cc";
            CARGO_BUILD_RUSTFLAGS = [
              "-C"
              "linker=${TARGET_CC}"
            ];

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

            CC = "${pkgsCross.stdenv.cc}/bin/${pkgsCross.stdenv.cc.targetPrefix}cc";
            LD = "${pkgsCross.stdenv.cc}/bin/${pkgsCross.stdenv.cc.targetPrefix}cc";
          };

        individualPackages =
          let
            packageName = "cargo-fslabscli";
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

        treefmt = treefmt-nix.lib.evalModule pkgs {
          projectRootFile = "flake.nix";
          programs = {
            alejandra.enable = true;
            nixfmt.enable = true;
            rustfmt.enable = true;
          };
        };
      in
      {
        formatter = treefmt.config.build.wrapper;

        packages = individualPackages // {
          default = individualPackages."cargo-fslabscli-${system}";
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
          devenv-up = self.devShells.${system}.default.config.procfileScript;
          devenv-test = self.devShells.${system}.default.config.test;
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
                  self.packages.${system}.default
                  updatecli
                  cargo-deny
                  xunit-viewer
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
