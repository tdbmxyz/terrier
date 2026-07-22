{
  description = "terrier - self-hosted immobilier price tracker";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
  }: let
    system = "x86_64-linux";
    pkgs = import nixpkgs {
      inherit system;
      overlays = [rust-overlay.overlays.default];
    };
    inherit (pkgs) lib;

    version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

    rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
    rustPlatform = pkgs.makeRustPlatform {
      cargo = rustToolchain;
      rustc = rustToolchain;
    };

    # trunk invokes wasm-bindgen; its CLI version must match the crate in
    # Cargo.lock exactly. Same pattern (and hashes) as ferret/chaos.
    hasCargoLock = builtins.pathExists ./Cargo.lock;

    wasm-bindgen-cli = let
      cargoLock = builtins.fromTOML (builtins.readFile ./Cargo.lock);
      wasmBindgen =
        lib.findFirst
        (p: p.name == "wasm-bindgen")
        (throw "wasm-bindgen not found in Cargo.lock")
        cargoLock.package;
    in
      pkgs.buildWasmBindgenCli rec {
        src = pkgs.fetchCrate {
          pname = "wasm-bindgen-cli";
          version = wasmBindgen.version;
          hash = "sha256-H6Is3fiZVxZCfOMWK5dWMSrtn50VGv0sfdnsT+cTtyk=";
        };

        cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
          inherit src;
          inherit (src) pname version;
          hash = "sha256-VucqkXbCi4qtQzY/HrXiDnbSURsagPsdNVMn1Tw3UiY=";
        };
      };

    terrier-server = rustPlatform.buildRustPackage {
      pname = "terrier-server";
      inherit version;
      src = self;

      cargoLock.lockFile = ./Cargo.lock;

      # no .git in the sandbox — hand the commit to build.rs directly
      GIT_COMMIT = self.shortRev or self.dirtyShortRev or "unknown";

      cargoBuildFlags = ["-p" "terrier-server"];
      cargoTestFlags = ["-p" "terrier-server"];

      meta = {
        description = "terrier backend: immo scraper, pipeline, API";
        mainProgram = "terrier-server";
      };
    };

    terrier-web = pkgs.stdenv.mkDerivation {
      pname = "terrier-web";
      inherit version;
      src = self;

      cargoDeps = pkgs.rustPlatform.importCargoLock {lockFile = ./Cargo.lock;};

      GIT_COMMIT = self.shortRev or self.dirtyShortRev or "unknown";

      nativeBuildInputs = [
        rustToolchain
        pkgs.trunk
        pkgs.binaryen
        wasm-bindgen-cli
        pkgs.rustPlatform.cargoSetupHook
      ];

      buildPhase = ''
        runHook preBuild
        export HOME=$TMPDIR
        cd crates/terrier-web
        trunk build --release --offline true --dist dist
        runHook postBuild
      '';

      installPhase = ''
        runHook preInstall
        cp -r dist $out
        runHook postInstall
      '';

      meta.description = "terrier web frontend (static trunk dist)";
    };
  in {
    packages.${system} = {
      inherit terrier-server terrier-web;
      default = terrier-server;
    };

    nixosModules.terrier = import ./nix/module.nix self;

    devShells.${system}.default = pkgs.mkShell {
      name = "terrier";

      packages = with pkgs;
        [
          rustToolchain
          just
          trunk
          binaryen
        ]
        ++ lib.optional hasCargoLock wasm-bindgen-cli;
    };
  };
}
