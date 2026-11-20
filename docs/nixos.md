# GLLMH on NixOS 26.05

The flake packages GLLMH and the acceptance services for `x86_64-linux`. It
pins the NixOS 26.05 channel in `flake.lock`. Nix uses the machine's configured
proxy and substituters, including PC-1's USTC mirror.

## Enable the acceptance runtime on PC-1

Add this input to PC-1's existing host flake. Use `ref=main` after A3 is merged.

```nix
inputs.gllmh.url = "git+file:///home/caleb/repositories/gllmh?ref=gllmh/a3";
```

Bind the input in the existing `outputs` arguments, for example
`outputs = { self, nixpkgs, gllmh, ... }: ...`. Add this entry to the
`modules` argument of the `nixosSystem` call for PC-1.

```nix
gllmh.nixosModules.acceptance
```

The module enables `services.gllmh` with the `acceptance` runtime. It creates
the `gllmh`, `user`, `agent`, `worker_agent` and `dispatch` system accounts
with matching primary groups. Participant homes default to
`/var/lib/gllmh-users/<account>`. NixOS adds `gllmh.target` to
`multi-user.target` and installs `gllmh` in the system PATH.

A4 owns the first activation. Build the host generation with
`nixos-rebuild build --flake .#<PC-1-hostname>` before activating it.

## Install the provider keys

Once the accounts exist, install each agent's Anthropic key from a private
local file. Both source paths may refer to the same provider key if the agents
share a credential. Keep the source files outside the flake.

```sh
sudo install -d -m 0711 -o root -g root /etc/gllmh-keys
sudo install -m 0400 -o agent -g agent /path/to/agent-key /etc/gllmh-keys/anthropic-agent
sudo install -m 0400 -o worker_agent -g worker_agent /path/to/worker-key /etc/gllmh-keys/anthropic-worker
sudo gllmh check --profile acceptance
```

The broker needs search permission on the key directory to verify file
ownership and mode. The file permissions give read access to the owning
agent. If the first activation starts the target before the keys exist,
clear any systemd start limits and restart the target after the check passes.

```sh
sudo systemctl reset-failed 'gllmh-*.service'
sudo systemctl restart gllmh.target
```

## Operate the runtime

```sh
sudo systemctl start gllmh.target
systemctl status gllmh.target gllmh-broker.service
sudo journalctl -u gllmh-broker.service -u gllmh-valet-agent.service
sudo journalctl -u 'gllmh-service-agent::timer.service'
sudo systemctl stop gllmh.target
```

Use `sudo -u user -H gllmh attach` to attach as the acceptance user. The timer
submits one message per service start, and the dispatcher writes its result
to the journal under `gllmh-service-dispatch::draft_dispatcher.service`.

## Configure a different runtime

Import `gllmh.nixosModules.gllmh` in the host's module list and add the
following to a host module.

```nix
services.gllmh = {
  enable = true;
  profile = "workflow";
  profiles = ./profiles;
  systemUsers.scout.home = "/var/lib/scout";
};
```

The directory contains files such as `profiles/runtime/workflow.json` and
`profiles/user/scout.json`. As an alternative, supply the equivalent Nix
attributes under `profiles.<type>.<name>`. The module declares an account for
each `system_user` and lets `systemUsers.<account>.extraGroups` grant additional
group memberships. The `package` option defaults to the bundled static build.

Use a dedicated private directory for each `user_data_store`. The module
creates the store at mode `0700` so systemd can enter it before the valet starts.
It expands `~` in each user's store and working directory paths using that
account's configured home. Create any explicit working directories separately
and give their system users access. Use Nix store paths for executables in
profiles. Provider keys belong in agent-owned files outside the store.

NixOS installs the profile copies under `/etc/gllmh` as `root:gllmh`, with
`0750` directories and `0640` files. Change profiles in the host configuration
and build a new generation so the generated units and installed profiles
come from the same source. After activating a changed profile, restart
`gllmh.target` so the broker and valets load it.
