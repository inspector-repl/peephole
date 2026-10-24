{
  description = "Stop a running Rust program and explore its variables from a REPL";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      forAllSystems = lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
    in
    {
      packages = forAllSystems (system: {
        default = nixpkgs.legacyPackages.${system}.callPackage ./nix/package.nix { };
      });

      formatter = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        pkgs.treefmt.withConfig {
          configFile = ./treefmt.toml;
          runtimeInputs = [
            pkgs.rustfmt
            pkgs.nixfmt
          ];
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.rust-analyzer
            ];
          };
        }
      );

      checks = forAllSystems (
        system:
        let
          package = self.packages.${system}.default;
        in
        {
          inherit package;
          clippy = package.overrideAttrs (old: {
            pname = "peephole-clippy";
            nativeBuildInputs = old.nativeBuildInputs ++ [ nixpkgs.legacyPackages.${system}.clippy ];
            buildPhase = "cargo clippy --offline --all-targets -- -D warnings";
            installPhase = "touch $out";
            doCheck = false;
            dontFixup = true;
          });
          treefmt = self.formatter.${system}.check self;
          devShell = self.devShells.${system}.default;
        }
      );
    };
}
