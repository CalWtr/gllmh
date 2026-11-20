{ acceptance, runCommand }:

let
  cfg = acceptance.config;
  toplevel = cfg.system.build.toplevel;
  # Force full system evaluation without building an unrelated kernel or bootloader.
  evaluatedSystem = builtins.unsafeDiscardStringContext toplevel.drvPath;
  units = cfg.environment.etc."systemd/system".source;
in
assert cfg.users.users.agent.isSystemUser && cfg.users.users.worker_agent.createHome;
assert cfg.environment.etc."gllmh/llm/anthropic.json".mode == "0640";
assert cfg.environment.etc."gllmh/llm/anthropic.json".group == "gllmh";
runCommand "gllmh-module-eval" { inherit evaluatedSystem; } ''
  units=${units}
  test -e "$units/multi-user.target.wants/gllmh.target"
  dependencies="$units/gllmh.target.d/10-dependencies.conf"
  grep -Fx 'Wants=gllmh-broker.service' "$dependencies"
  grep -Fx 'Wants=gllmh-user-agent.socket' "$dependencies"
  grep -Fx 'Wants=gllmh-valet-agent.service' "$dependencies"
  grep -Fx 'Requires=gllmh-service-agent::timer.service' "$dependencies"
  grep -Fx 'Requires=gllmh-service-dispatch::draft_dispatcher.service' "$dependencies"
  grep -Fx 'WorkingDirectory=/var/lib/gllmh-users/agent/.gllmh' "$units/gllmh-valet-agent.service"
  grep -E '^ExecStart=/nix/store/[^ ]+/bin/gllmh-timer$' "$units/gllmh-service-agent::timer.service"
  grep -E '^ExecStart=/nix/store/[^ ]+/bin/gllmh-draft-dispatcher$' "$units/gllmh-service-dispatch::draft_dispatcher.service"
  printf '%s\n' "$evaluatedSystem" > "$out"
''
