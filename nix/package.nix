{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "coffeefs";
  version = (lib.importTOML ../Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  meta = {
    description = "FUSE passthrough filesystem whose contents are linked in at runtime";
    mainProgram = "coffeefs";
    platforms = lib.platforms.linux;
  };
}
