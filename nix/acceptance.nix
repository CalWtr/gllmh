{ lib, pkgs, ... }:

# The section-9 acceptance runtime on a real NixOS host (A4, PC-1).
#
# GLLMH user names stay as the spec writes them (user, agent, worker_agent,
# dispatch); their Linux accounts are gllmh-* so they cannot collide with a
# host's existing users or groups (PC-1 already has a shared `agent` group).
# agent speaks the Anthropic protocol and worker_agent the OpenAI protocol,
# both against DeepSeek, so one run exercises both provider clients.

let
  fixtures = import ./read-profiles.nix { inherit lib; dir = ../tests/fixtures/acceptance; };
  services = pkgs.pkgsStatic.callPackage ./package.nix { kind = "acceptance-services"; };

  deepseek = {
    model_id = "deepseek-chat";
    thinking = { native = "never"; on = false; returned = "none"; replay = "never"; };
    context_window = 64000;
    max_output = 2048;
    tuning = [ ];
    timeout_s = 120;
    max_retries = 3;
  };
in
{
  services.gllmh = {
    enable = true;
    profile = "acceptance";

    # The fixture's own llm profile points at a placeholder key; replace it.
    profiles = lib.recursiveUpdate (fixtures // { llm = { }; }) {
      user.user.system_user = "gllmh-user";
      user.agent.system_user = "gllmh-agent";
      user.worker_agent.system_user = "gllmh-worker";
      user.dispatch.system_user = "gllmh-dispatch";

      user.agent.llm_profiles = [ "deepseek-anthropic" ];
      user.worker_agent.llm_profiles = [ "deepseek-openai" ];

      user.agent.services.timer = {
        command = "${services}/bin/gllmh-timer";
        config = {
          to = "agent";
          delay_s = 0;
          body = ''
            Use send_directive to ask worker_agent for a two-sentence outline of a garden plan.
            Do not message user yet. When worker_agent's result arrives, use send_message to
            forward it to user, then stop.
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

      # Each key must be owned by the account that reads it (gllmh check, 0400).
      llm.deepseek-anthropic = deepseek // {
        name = "deepseek-anthropic";
        protocol = "anthropic";
        base_url = "https://api.deepseek.com/anthropic";
        api_key_file = "/etc/gllmh-keys/deepseek-agent";
      };
      llm.deepseek-openai = deepseek // {
        name = "deepseek-openai";
        protocol = "openai";
        base_url = "https://api.deepseek.com";
        api_key_file = "/etc/gllmh-keys/deepseek-worker";
      };
    };
  };
}
