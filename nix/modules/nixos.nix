# `dirigent host` on NixOS: a systemd system unit, started at boot, running as `user` (never
# root), its sessions under that user's ~/dirigent (or `roots`). It signs in with the user's own
# credentials: `dirigent login` as them (~/.config/dirigent/credentials.json), renewed in place.
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
      type = lib.types.str;
      example = "adam";
      description = "The user it runs as, and its players: whose `dirigent login` it signs in with.";
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
        PATH = lib.mkForce (
          host.path cfg [
            "/etc/profiles/per-user/${cfg.user}/bin"
            "/run/current-system/sw/bin"
          ]
        );
        DIRIGENT_HOST_CONFIG = "/etc/dirigent/host.toml";
      };
      serviceConfig = {
        # its HOME is the user's: their credentials, and ~/dirigent
        User = cfg.user;
        ExecStart = "${lib.getExe cfg.package} host run";
        Restart = "always";
        RestartSec = 5;
        # players outlive the daemon: they hold their sessions' leases
        KillMode = "process";
      };
    };
  };
}
