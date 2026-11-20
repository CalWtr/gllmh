{ config, lib, pkgs, ... }:

let
  cfg = config.services.gllmh;
  json = pkgs.formats.json { };
  profileTypes = [ "runtime" "user" "llm" "composer" "tool" ];
  profiles =
    if builtins.isAttrs cfg.profiles then cfg.profiles
    else import ./read-profiles.nix { inherit lib; dir = cfg.profiles; };

  accountNames = lib.unique (lib.mapAttrsToList (_: profile: profile.system_user) (profiles.user or { }));
  accountConfig = name: cfg.systemUsers.${name} or { home = null; extraGroups = [ ]; };

  expandHome = home: path:
    if path == "~" then home
    else if lib.hasPrefix "~/" path then home + "/" + lib.removePrefix "~/" path
    else path;

  # Use the effective NixOS home for both the installed JSON and the unit build.
  # The renderer's --dev cannot look up the final system users in the sandbox.
  userProfiles = lib.mapAttrs (_: profile:
    let home = config.users.users.${profile.system_user}.home;
    in
    profile // {
      user_data_store = expandHome home (profile.user_data_store or "~/.gllmh");
    } // lib.optionalAttrs (profile ? cwd && profile.cwd != null) {
      cwd = expandHome home profile.cwd;
    } // lib.optionalAttrs (profile ? services && profile.services != null) {
      services = lib.mapAttrs (_: service:
        service // lib.optionalAttrs (service ? cwd && service.cwd != null) {
          cwd = expandHome home service.cwd;
        }
      ) profile.services;
    }
  ) (profiles.user or { });
  resolvedProfiles = profiles // { user = userProfiles; };

  relativeProfiles = lib.concatLists (lib.mapAttrsToList (type: entries:
    lib.mapAttrsToList (name: value: {
      path = "${type}/${name}.json";
      text = builtins.toJSON value;
    }) entries
  ) resolvedProfiles);
  profileTree = pkgs.linkFarm "gllmh-profiles" (map (file: {
    name = file.path;
    path = pkgs.writeText (builtins.baseNameOf file.path) file.text;
  }) relativeProfiles);

  bin = "${cfg.package}/bin/gllmh";
  renderedUnits = pkgs.runCommand "gllmh-systemd-units" { } ''
    units="$out/lib/systemd/system"
    ${bin} build --dev --profile ${lib.escapeShellArg cfg.profile} \
      --bin ${bin} --config-dir ${profileTree} --out "$units"

    # systemd.packages installs unit files without running systemctl enable.
    # Preserve the renderer's [Install] relationships as target dependencies.
    cd "$units"
    mkdir gllmh.target.d
    {
      echo '[Unit]'
      for unit in *.service *.socket; do
        if [ -f "$unit" ]; then
          awk '
            /^WantedBy=gllmh[.]target$/ { print "Wants=" FILENAME }
            /^RequiredBy=gllmh[.]target$/ { print "Requires=" FILENAME }
          ' "$unit"
        fi
      done
    } > gllmh.target.d/10-dependencies.conf
  '';

  configDirs = [ "/etc/gllmh" ] ++ map (type: "/etc/gllmh/${type}") profileTypes;
  # Quote tmpfiles paths and escape systemd specifiers.
  tmpfilesPath = path: builtins.toJSON (lib.replaceStrings [ "%" ] [ "%%" ] path);
  validProfileName = name: name != "." && name != ".." && builtins.match "[^/]+" name != null;
in
{
  options.services.gllmh = {
    enable = lib.mkEnableOption "the GLLMH broker, valets and services";
    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.pkgsStatic.callPackage ./package.nix { };
      defaultText = lib.literalMD "The bundled static GLLMH package.";
      description = "The GLLMH package to install and use to render systemd units.";
    };

    profile = lib.mkOption {
      type = lib.types.str;
      description = "Name of the runtime profile to activate.";
    };

    profiles = lib.mkOption {
      type = lib.types.either (lib.types.attrsOf (lib.types.attrsOf json.type)) lib.types.path;
      default = { };
      example = lib.literalExpression "./profiles";
      description = "Profiles as type-to-name-to-JSON attributes or a directory of JSON profiles.";
    };

    systemUsers = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule {
        options = {
          home = lib.mkOption {
            type = lib.types.nullOr lib.types.path;
            default = null;
            description = "The system user's home. Defaults to /var/lib/gllmh-users/<name>.";
          };
          extraGroups = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            description = "Additional groups for this system user.";
          };
        };
      });
      default = { };
      description = "Overrides keyed by the system_user values in the user profiles.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = builtins.hasAttr cfg.profile (profiles.runtime or { });
        message = "services.gllmh.profile must name a runtime in services.gllmh.profiles.";
      }
      {
        assertion = lib.all (type: builtins.elem type profileTypes) (builtins.attrNames profiles)
          && lib.all (entries: lib.all validProfileName (builtins.attrNames entries)) (builtins.attrValues profiles);
        message = "services.gllmh.profiles must use known profile types and names that are single path components.";
      }
      {
        assertion = lib.all (name: name != "root" && name != "gllmh") accountNames;
        message = "GLLMH participants must use accounts other than root and the gllmh broker account.";
      }
      {
        assertion = lib.all (name: builtins.elem name accountNames) (builtins.attrNames cfg.systemUsers);
        message = "services.gllmh.systemUsers keys must match system_user values in the user profiles.";
      }
    ];

    users.groups = lib.genAttrs ([ "gllmh" ] ++ accountNames) (_: { });
    users.users = lib.genAttrs accountNames (name:
      let overrides = accountConfig name;
      in {
        isSystemUser = true;
        group = lib.mkDefault name;
        home = lib.mkDefault (if overrides.home == null then "/var/lib/gllmh-users/${name}" else overrides.home);
        createHome = true;
        inherit (overrides) extraGroups;
      }
    ) // {
      gllmh = {
        isSystemUser = true;
        group = "gllmh";
      };
    };

    environment.systemPackages = [ cfg.package ];

    environment.etc = builtins.listToAttrs (map (file: lib.nameValuePair "gllmh/${file.path}" {
      source = "${profileTree}/${file.path}";
      mode = "0640";
      user = "root";
      group = "gllmh";
    }) relativeProfiles);

    systemd.tmpfiles.rules =
      map (dir: "d ${dir} 0750 root gllmh -") configDirs
      # The valet's WorkingDirectory defaults to its store, which must exist before ExecStart.
      ++ lib.mapAttrsToList (_: user:
        let account = config.users.users.${user.system_user};
        in "d ${tmpfilesPath user.user_data_store} 0700 ${account.name} ${account.group} -"
      ) userProfiles;

    systemd.packages = [ renderedUnits ];
    systemd.targets.gllmh.wantedBy = [ "multi-user.target" ];
  };
}
