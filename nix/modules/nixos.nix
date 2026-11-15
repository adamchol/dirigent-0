# `dirigent host` as the machine's service (`host install --system`'s, declared): a systemd
# system unit, running as root or `user`, its sessions under /var/lib/dirigent. It signs in with
# `credentialsFile`, a copy of a user's (`dirigent login`, then
# `sudo install -m 600 ~/.config/dirigent/credentials.json /etc/dirigent/credentials.json`).
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.dirigent-host;
  host = import ./host.nix { inherit lib pkgs self; };
in
{
  options.services.dirigent-host = host.options // {
    user = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "The user it runs as (its players too); root when null.";
    };
    credentialsFile = lib.mkOption {
      type = lib.types.str;
      default = "/etc/dirigent/credentials.json";
      description = "The CLI's credentials it signs in with, readable by its user.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.etc."dirigent/host.toml".source = host.config cfg;
    systemd.services.dirigent-host = {
      description = "dirigent host: runs players for your sessions";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      restartTriggers = [ (host.config cfg) ];
      environment = {
        PATH = lib.mkForce (host.path cfg [ "/run/current-system/sw/bin" ]);
        DIRIGENT_HOST_CONFIG = "/etc/dirigent/host.toml";
        DIRIGENT_CREDENTIALS = cfg.credentialsFile;
      };
      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} host run --system";
        Restart = "always";
        RestartSec = 5;
        # players outlive the daemon: they hold their sessions' leases
        KillMode = "process";
        StateDirectory = "dirigent";
      }
      // lib.optionalAttrs (cfg.user != null) { User = cfg.user; };
    };
  };
}
