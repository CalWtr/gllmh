{ lib, pkgs, ... }:

let
  fixtures = import ./read-profiles.nix { inherit lib; dir = ../tests/fixtures/acceptance; };
  services = pkgs.pkgsStatic.callPackage ./package.nix { kind = "acceptance-services"; };
  anthropic = fixtures.llm.anthropic;
in
{
  services.gllmh = {
    enable = true;
    profile = "acceptance";

    profiles = lib.recursiveUpdate fixtures {
      user.agent.services.timer.command = "${services}/bin/gllmh-timer";
      user.dispatch.services.draft_dispatcher.command = "${services}/bin/gllmh-draft-dispatcher";
      user.worker_agent.llm_profiles = [ "anthropic-worker" ];

      # Each key must be owned by the user who reads it.
      llm.anthropic.api_key_file = "/etc/gllmh-keys/anthropic-agent";
      llm.anthropic-worker = anthropic // {
        name = "anthropic-worker";
        api_key_file = "/etc/gllmh-keys/anthropic-worker";
      };
    };
  };
}
