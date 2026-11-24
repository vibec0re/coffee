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
        containerPath = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "/work";
          description = ''
            Host-mode instances with a `container`: bind-mount the mountpoint
            into the container at this path (read-write; what is writable is
            decided per link).
          '';
        };
        passthrough = mkOption {
          type = types.bool;
          default = true;
          description = ''
            Use kernel FUSE passthrough (Linux 6.9+): reads, writes and mmaps of
            open files go straight to the host file instead of through the
            daemon. Falls back to daemon I/O where unavailable.
          '';
        };
        extraArgs = mkOption {
          type = types.listOf types.str;
          default = [ ];
        };
      };
    }
  );

  # Links and dirs, in the state file's format. Installed at a stable path
  # (/etc/coffeefs/NAME.json) so that changing them only reloads the daemon.
  configFile =
    name: i:
    pkgs.writeText "coffeefs-${name}.json" (
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

  mkService =
    name: i:
    let
      unit = "container@${i.container}.service";
      args = [
        "serve"
        name
        "--mountpoint"
        i.mountpoint
        "--threads"
        (toString i.threads)
        "--ttl"
        i.ttl
        "--config"
        "/etc/coffeefs/${name}.json"
      ]
      ++ optionals i.persist [
        "--state"
        "/var/lib/coffeefs/${name}.json"
      ]
      ++ optional (!i.passthrough) "--no-passthrough"
      ++ i.extraArgs;
      bin = "${cfg.package}/bin/coffeefs";
    in
    {
      description = "coffeefs instance ${name}";
      wantedBy =
        if i.inContainerNamespace then
          [ unit ]
        else
          [ "multi-user.target" ] ++ optional (i.container != null) unit;
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

      # Changing links/dirs only reloads (SIGHUP): the mount, open files and
      # runtime links stay. A host-mode mount is bind-mounted into containers,
      # which a restart would leave with a dead mount, so any other change
      # (e.g. a package upgrade) reloads too; restart it by hand to upgrade.
      # In-namespace instances remount in place, so they restart.
      reloadTriggers = [ (configFile name i) ];
      reloadIfChanged = !i.inContainerNamespace;

      serviceConfig = {
        Type = "notify-reload";
        NotifyAccess = "main";
        Restart = "on-failure";
        RestartSec = 1;
        StateDirectory = "coffeefs";
        # one O_PATH fd per inode the kernel holds
        LimitNOFILE = 1048576;

        # Hardening. Nothing that gives the service its own mount namespace
        # (ProtectSystem, PrivateTmp, ...): the mount has to land on the host,
        # and passthrough needs the host's writable view of linked files.
        CapabilityBoundingSet = [
          "CAP_SYS_ADMIN" # mount, setns, passthrough backing files
          "CAP_SYS_CHROOT" # setns into a mount namespace
          "CAP_SYS_PTRACE" # open /proc/LEADER/ns/mnt of a container
          "CAP_DAC_OVERRIDE"
          "CAP_DAC_READ_SEARCH"
          "CAP_FOWNER"
          "CAP_FSETID"
          "CAP_CHOWN"
          "CAP_SETUID" # per-thread fsuid of the caller
          "CAP_SETGID"
        ];
        NoNewPrivileges = true;
        RestrictAddressFamilies = [ "AF_UNIX" ];
        RestrictNamespaces = "mnt";
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        SystemCallArchitectures = "native";
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
    assertions = lib.concatLists (
      lib.mapAttrsToList (name: i: [
        {
          assertion = i.inContainerNamespace -> i.container != null;
          message = "services.coffeefs.instances.${name}: inContainerNamespace needs `container`";
        }
        {
          assertion = i.containerPath != null -> (i.container != null && !i.inContainerNamespace);
          message = "services.coffeefs.instances.${name}: containerPath needs `container` and a host-mode instance";
        }
      ]) cfg.instances
    );

    boot.kernelModules = [ "fuse" ];
    environment.systemPackages = [ cfg.package ];
    environment.etc = lib.mapAttrs' (
      name: i: lib.nameValuePair "coffeefs/${name}.json" { source = configFile name i; }
    ) cfg.instances;
    systemd.services = lib.mapAttrs' (
      name: i: lib.nameValuePair "coffeefs-${name}" (mkService name i)
    ) cfg.instances;
    containers = lib.mkMerge (
      lib.mapAttrsToList (
        _: i:
        lib.optionalAttrs (i.containerPath != null) {
          ${i.container}.bindMounts.${i.containerPath} = {
            hostPath = i.mountpoint;
            isReadOnly = false;
          };
        }
      ) cfg.instances
    );
  };
}
