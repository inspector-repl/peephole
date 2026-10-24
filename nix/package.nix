{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "peephole";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../protocol
      ../macros
      ../peephole
      ../cli
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # The end-to-end tests build the demo with cargo themselves.
  cargoTestFlags = [ "--workspace" ];

  meta = {
    description = "Stop a running Rust program and explore its variables from a REPL";
    license = lib.licenses.mit;
    mainProgram = "peephole";
  };
}
