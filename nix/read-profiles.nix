{ lib, dir }:

lib.genAttrs [ "runtime" "user" "llm" "composer" "tool" ] (
  type:
  let
    path = dir + "/${type}";
    files = if builtins.pathExists path then builtins.readDir path else { };
  in
  lib.mapAttrs' (
    file: _: lib.nameValuePair (lib.removeSuffix ".json" file) (
      builtins.fromJSON (builtins.readFile (path + "/${file}"))
    )
  ) (lib.filterAttrs (file: kind: kind != "directory" && lib.hasSuffix ".json" file) files)
)
