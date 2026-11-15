# `dirigent host` as your service: a systemd user unit on Linux, a launchd agent on macOS (while
# you're logged in). Sign in once with `dirigent login`; its credentials are the host's.
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
  configFile = "${config.xdg.configHome}/dirigent/host.toml";
  env = {
    PATH = host.path cfg [
      "${config.home.profileDirectory}/bin"
      "/etc/profiles/per-user/${config.home.username}/bin"
      "/run/current-system/sw/bin"
      "/nix/var/nix/profiles/default/bin"
      "/usr/local/bin"
      "/usr/bin"
      "/bin"
    ];
    DIRIGENT_HOST_CONFIG = configFile;
    DIRIGENT_CREDENTIALS = "${config.xdg.configHome}/dirigent/credentials.json";
  };
  exe = lib.getExe cfg.package;
in
{
  options.services.dirigent-host = host.options;

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        home.packages = [ cfg.package ];
        xdg.configFile."dirigent/host.toml".source = host.config cfg;
      }
      (lib.mkIf pkgs.stdenv.hostPlatform.isLinux {
        systemd.user.services.dirigent-host = {
          Unit = {
            Description = "dirigent host: runs players for your sessions";
            X-Restart-Triggers = [ (host.config cfg) ];
          };
          Service = {
            ExecStart = "${exe} host run";
            Environment = lib.mapAttrsToList (k: v: "${k}=${v}") env;
            Restart = "always";
            RestartSec = 5;
            # players outlive the daemon: they hold their sessions' leases
            KillMode = "process";
          };
          Install.WantedBy = [ "default.target" ];
        };
      })
      (lib.mkIf pkgs.stdenv.hostPlatform.isDarwin {
        launchd.agents.dirigent-host = {
          enable = true;
          config = {
            ProgramArguments = [
              exe
              "host"
              "run"
            ];
            EnvironmentVariables = env;
            RunAtLoad = true;
            KeepAlive = true;
            # players outlive the daemon: they hold their sessions' leases
            AbandonProcessGroup = true;
            StandardOutPath = "${config.home.homeDirectory}/Library/Logs/dirigent-host.log";
            StandardErrorPath = "${config.home.homeDirectory}/Library/Logs/dirigent-host.log";
          };
        };
      })
    ]
  );
}
