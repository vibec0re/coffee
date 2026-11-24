{
  description = "coffeefs: FUSE passthrough filesystem whose contents are linked in at runtime";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        coffeefs = pkgs.callPackage ./nix/package.nix { };
        default = coffeefs;
      });

      overlays.default = final: _prev: { coffeefs = final.callPackage ./nix/package.nix { }; };

      nixosModules.default = import ./nix/module.nix self;

      checks = forAll (
        pkgs:
        let
          pkg = self.packages.${pkgs.stdenv.hostPlatform.system}.coffeefs;
        in
        {
          # unit tests run in the package's checkPhase
          package = pkg;
          clippy = pkg.overrideAttrs (o: {
            pname = "coffeefs-clippy";
            nativeBuildInputs = o.nativeBuildInputs ++ [ pkgs.clippy ];
            buildPhase = ''
              runHook preBuild
              cargo clippy --offline --all-targets -- -D warnings
              runHook postBuild
            '';
            doCheck = false;
            installPhase = "touch $out";
          });
          fmt = pkgs.runCommand "coffeefs-fmt" { nativeBuildInputs = [ pkgs.cargo pkgs.rustfmt ]; } ''
            cd ${pkg.src}
            HOME=$TMPDIR cargo fmt --check
            touch $out
          '';
          # real kernel, nspawn container, three instances, config switch
          vm = pkgs.testers.runNixOSTest (import ./nix/test.nix self);
        }
      );

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
          ];
        };
      });
    };
}
