{ lib, stdenv, rustPlatform, kind ? "gllmh" }:

let
  tests = kind == "tests";
  services = kind == "acceptance-services";
in
rustPlatform.buildRustPackage ({
  pname = if tests then "gllmh-cargo-test" else if services then "gllmh-acceptance-services" else "gllmh";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../tests
      ../examples
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  cargoBuildFlags = if services then [
    "--example" "timer_service" "--example" "draft_dispatcher"
  ] else [ "--bin" "gllmh" ];
  # Leave the host-only systemd-analyze verifier out of the sandbox PATH.
  doCheck = tests;

  preCheck = ''
    # The acceptance service tests invoke Cargo to build the examples.
    export CARGO_NET_OFFLINE=true
    export CARGO_BUILD_TARGET=${lib.escapeShellArg stdenv.hostPlatform.rust.rustcTargetSpec}
  '';

  installPhase = lib.optionalString services ''
    runHook preInstall
    install -Dm755 target/*/release/examples/timer_service "$out/bin/gllmh-timer"
    install -Dm755 target/*/release/examples/draft_dispatcher "$out/bin/gllmh-draft-dispatcher"
    runHook postInstall
  '';

  meta = {
    description = "Linux system-user harness for humans, agents and services";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  } // lib.optionalAttrs (!services && !tests) { mainProgram = "gllmh"; };
} // lib.optionalAttrs tests { buildType = "debug"; })
