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
      user.agent.services.timer = {
        command = "${services}/bin/gllmh-timer";
        config = {
          to = "agent";
          delay_s = 0;
          body = ''
            Use send_directive to ask worker_agent for a two-sentence outline of a garden plan.
            When the result arrives, use send_message to forward it to user.
          '';
        };
      };
      user.dispatch.services.draft_dispatcher = {
        command = "${services}/bin/gllmh-draft-dispatcher";
        config = {
          to = "worker_agent";
          context = [{
            name = "instructions";
            content = "Draft a short outline for a guide to planning a library reading group.";
          }];
        };
      };
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
