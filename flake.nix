{
  description = "fslabscli";
  inputs = {
    fenix.url = "github:nix-community/fenix";
    flake-utils.url = "github:numtide/flake-utils";
    naersk.url = "github:nix-community/naersk";
    nixpkgs.url = "github:nixos/nixpkgs/nixos-24.11";
    gitignore.url = "github:hercules-ci/gitignore.nix";
  };
  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      naersk,
      fenix,
      gitignore,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        inherit (pkgs.stdenv) isDarwin;
        inherit (gitignore.lib) gitignoreSource;
        fenixPkgs = fenix.packages.${system};
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;

        rustSrc = gitignoreSource ./.;

        # Map from architecture name to rust targets and nixpkgs targets.
        arch2targets = {
          "x86_64-linux" = {
            rustTarget = "x86_64-unknown-linux-musl";
            crossTarget = "x86_64-unknown-linux-musl";
          };
          "aarch64-linux" = {
            rustTarget = "aarch64-unknown-linux-musl";
            crossTarget = "aarch64-unknown-linux-musl";
          };
          "i686-linux" = {
            rustTarget = "i686-unknown-linux-musl";
            crossTarget = "i686-unknown-linux-musl";
          };
          "aarch64-darwin" = {
            rustTarget = "aarch64-apple-darwin";
            crossTarget = "aarch64-darwin";
          };
        };
        pkgsWin64 = pkgs.pkgsCross.mingwW64;
        mkWin64RustPackage =
          packageName:
          let
            rustTarget = "x86_64-pc-windows-gnu";
            toolchainWin = fenixPkgs.combine [
              fenixPkgs.stable.rustc
              fenixPkgs.stable.cargo
              fenixPkgs.targets.${rustTarget}.stable.rust-std
            ];
            naerskWin = pkgs.callPackage naersk {
              cargo = toolchainWin;
              rustc = toolchainWin;
            };
          in
          naerskWin.buildPackage rec {
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
            src = pkgs.lib.cleanSource ./.;
            nativeBuildInputs = [
              pkgs.perl # Needed to build vendored OpenSSL.
            ];
            depsBuildBuild = [
              pkgsWin64.stdenv.cc
              pkgsWin64.windows.pthreads
            ];
            auditable = false; # Avoid cargo-auditable failures.
            doCheck = false; # Disable test as it requires network access.
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
            CARGO_BUILD_TARGET = rustTarget;
            TARGET_CC = "${pkgsWin64.stdenv.cc}/bin/${pkgsWin64.stdenv.cc.targetPrefix}cc";
            CARGO_BUILD_RUSTFLAGS = [
              "-C"
              "linker=${TARGET_CC}"
            ];

            CC = "${pkgsWin64.stdenv.cc}/bin/${pkgsWin64.stdenv.cc.targetPrefix}cc";
            LD = "${pkgsWin64.stdenv.cc}/bin/${pkgsWin64.stdenv.cc.targetPrefix}cc";
          };

        pkgsWin32 = pkgs.pkgsCross.mingw32;
        mkWin32RustPackage =
          packageName:
          let
            rustTarget = "i686-pc-windows-gnu";
          in
          let
            toolchainWin = fenixPkgs.combine [
              fenixPkgs.stable.rustc
              fenixPkgs.stable.cargo
              fenixPkgs.targets.${rustTarget}.stable.rust-std
            ];
            naerskWin = pkgs.callPackage naersk {
              cargo = toolchainWin;
              rustc = toolchainWin;
            };

            # Get rid of MCF Gthread library.
            # See <https://github.com/NixOS/nixpkgs/issues/156343>
            # and <https://discourse.nixos.org/t/statically-linked-mingw-binaries/38395>
            # for details.
            #
            # Use DWARF-2 instead of SJLJ for exception handling.
            winCC = pkgsWin32.buildPackages.wrapCC (
              (pkgsWin32.buildPackages.gcc-unwrapped.override ({
                threadsCross = {
                  model = "win32";
                  package = null;
                };
              })).overrideAttrs
                (oldAttr: {
                  configureFlags = oldAttr.configureFlags ++ [
                    "--disable-sjlj-exceptions --with-dwarf2"
                  ];
                })
            );
          in
          naerskWin.buildPackage rec {
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
            src = pkgs.lib.cleanSource ./.;
            nativeBuildInputs = [
              pkgs.perl # Needed to build vendored OpenSSL.
            ];
            depsBuildBuild = [
              winCC
              pkgsWin32.windows.pthreads
            ];
            auditable = false; # Avoid cargo-auditable failures.
            doCheck = false; # Disable test as it requires network access.

            CARGO_BUILD_TARGET = rustTarget;
            TARGET_CC = "${winCC}/bin/${winCC.targetPrefix}cc";
            CARGO_BUILD_RUSTFLAGS = [
              "-C"
              "linker=${TARGET_CC}"
            ];

            CC = "${winCC}/bin/${winCC.targetPrefix}cc";
            LD = "${winCC}/bin/${winCC.targetPrefix}cc";
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
          };

        mkCrossRustPackage =
          arch: packageName:
          let
            rustTarget = arch2targets."${arch}".rustTarget;
            crossTarget = arch2targets."${arch}".crossTarget;
            pkgsCross = import nixpkgs {
              system = system;
              crossSystem.config = crossTarget;
            };
          in
          let
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

        mkRustPackages =
          arch:
          let
            cargo-fslabscli = mkCrossRustPackage arch "cargo-fslabscli";
          in
          {
            "cargo-fslabscli-${arch}" = cargo-fslabscli;
          };
        individualPackages =
          mkRustPackages "aarch64-linux"
          // mkRustPackages "x86_64-linux"
          // mkRustPackages "aarch64-darwin"
          // {
            cargo-fslabscli-win64 = mkWin64RustPackage "cargo-fslabscli";
            cargo-fslabscli-win32 = mkWin32RustPackage "cargo-fslabscli";
          };
      in
      {
        formatter = pkgs.nixpkgs-fmt;

        packages = individualPackages // {
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

        devShells.default =
          let
            pkgs = import nixpkgs {
              system = system;
              overlays = [ fenix.overlays.default ];
            };
          in
          pkgs.mkShell {

            buildInputs = with pkgs; [
              (fenix.packages.${system}.complete.withComponents [
                "cargo"
                "clippy"
                "rust-src"
                "rustc"
                "rustfmt"
              ])
              cargo-deny
              rust-analyzer-nightly
              cargo-nextest
              perl # needed to build vendored OpenSSL
              git-cliff
            ];
          };
      }
    );
}
