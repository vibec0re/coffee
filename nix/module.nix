self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.coffeefs;
  inherit (lib)
    mkOption
    types
    optional
    optionals
    escapeShellArg
    escapeShellArgs
    ;

  linkType = types.submodule {
    options = {
      source = mkOption {
        type = types.str;
        description = "Host file or directory to expose.";
      };
      target = mkOption {
        type = types.str;
        description = "Absolute path inside the coffeefs mount.";
      };
      ro = mkOption {
        type = types.bool;
        default = false;
        description = "Expose read-only.";
      };
    };
  };

  instanceType = types.submodule (
    { name, ... }:
    {
      options = {
        mountpoint = mkOption {
          type = types.str;
          default = "/containers/${name}";
          description = ''
            Where to mount. With `inContainerNamespace` this is a path inside
            the container.
          '';
        };
        container = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "choom1";
          description = ''
            NixOS container (systemd-nspawn machine) this instance belongs to.
            Without `inContainerNamespace` the instance is mounted on the host
            and started before the container, so the container can bind-mount
            the mountpoint. With it, see below.
          '';
        };
        inContainerNamespace = mkOption {
          type = types.bool;
          default = false;
          description = ''
            Mount directly inside the running container's mount namespace
            (at `mountpoint`, a path inside the container) instead of on the
            host. The instance then starts and stops with the container and
            needs no bind mount; daemon restarts re-mount in place.
          '';
        };
        links = mkOption {
          type = types.listOf linkType;
          default = [ ];
          description = ''
            Links set up on every start (more can be added at runtime). One whose
            source doesn't exist yet is retried until it does.
          '';
        };
        dirs = mkOption {
          type = types.listOf types.str;
          default = [ ];
          description = "Empty virtual directories present at startup.";
        };
        persist = mkOption {
          type = types.bool;
          default = true;
          description = ''
            Keep runtime changes (made over the control socket) across restarts
            in /var/lib/coffeefs/NAME.json. `links` and `dirs` are applied on
            every start and never persisted, so removing one here revokes it.
          '';
        };
        threads = mkOption {
          type = types.ints.positive;
          default = 4;
        };
        ttl = mkOption {
          type = types.str;
          default = "1";
          description = "Kernel cache timeout in seconds.";
        };
        extraArgs = mkOption {
          type = types.listOf types.str;
          default = [ ];
        };
      };
    }
  );

  mkService =
    name: i:
    let
      unit = "container@${i.container}.service";
      # passed as a file: no quoting or SRC:DST splitting issues
      configFile = pkgs.writeText "coffeefs-${name}.json" (
        builtins.toJSON (
          map (target: {
            kind = "dir";
            inherit target;
          }) i.dirs
          ++ map (l: {
            kind = "link";
            inherit (l) source target ro;
          }) i.links
        )
      );
      args =
        [
          "serve"
          name
          "--mountpoint"
          i.mountpoint
          "--threads"
          (toString i.threads)
          "--ttl"
          i.ttl
          "--config"
          "${configFile}"
        ]
        ++ optionals i.persist [
          "--state"
          "/var/lib/coffeefs/${name}.json"
        ]
        ++ i.extraArgs;
      bin = "${cfg.package}/bin/coffeefs";
    in
    {
      description = "coffeefs instance ${name}";
      wantedBy =
        if i.inContainerNamespace then [ unit ] else [ "multi-user.target" ] ++ optional (i.container != null) unit;
      before = optional (i.container != null && !i.inContainerNamespace) unit;
      after = optional i.inContainerNamespace unit;
      bindsTo = optional i.inContainerNamespace unit;
      path = [ config.systemd.package ];
      script =
        if i.inContainerNamespace then
          ''
            leader=$(machinectl show --property=Leader --value ${escapeShellArg i.container})
            exec ${bin} ${escapeShellArgs args} --pid "$leader"
          ''
        else
          "exec ${bin} ${escapeShellArgs args}";
      serviceConfig = {
        Type = "notify";
        NotifyAccess = "main";
        Restart = "on-failure";
        RestartSec = 1;
        StateDirectory = "coffeefs";
      };
    };
in
{
  options.services.coffeefs = {
    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
    };
    instances = mkOption {
      type = types.attrsOf instanceType;
      default = { };
      description = "coffeefs instances, one daemon + mount + control socket each.";
    };
  };

  config = lib.mkIf (cfg.instances != { }) {
    assertions = lib.mapAttrsToList (name: i: {
      assertion = i.inContainerNamespace -> i.container != null;
      message = "services.coffeefs.instances.${name}: inContainerNamespace needs `container`";
    }) cfg.instances;

    boot.kernelModules = [ "fuse" ];
    environment.systemPackages = [ cfg.package ];
    systemd.services = lib.mapAttrs' (
      name: i: lib.nameValuePair "coffeefs-${name}" (mkService name i)
    ) cfg.instances;
  };
}
