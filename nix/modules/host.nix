# What the host's modules share (`dirigent host`: home-manager's for your user, NixOS's for a
# user of the machine): their options, the config they write (host.toml, as the CLI
# reads it), and what the daemon runs.
{
  lib,
  pkgs,
  self,
}:
let
  inherit (lib) mkOption types;
  nullOr =
    t:
    mkOption {
      type = types.nullOr t;
      default = null;
    };
in
{
  options = {
    enable = lib.mkEnableOption "dirigent host: this machine runs players for your sessions placed on it";
    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.cli;
      defaultText = lib.literalExpression "dirigent's cli";
      description = "dirigent's CLI, which runs the host and its players.";
    };
    url = nullOr types.str // {
      description = "dirigent's address, the web's: its API at /api, chlive at /live, and control's players' service at the address itself. By default, the signed-in API's.";
    };
    api = nullOr types.str // {
      description = "Control's API, where it isn't at `url`.";
    };
    chlive = nullOr types.str // {
      description = "chlive's WebSocket, where it isn't at `url`.";
    };
    players = nullOr types.str // {
      description = "Control's players' service (gRPC), where it isn't at `url` (e.g. on a tailnet).";
    };
    name = nullOr types.str // {
      description = "The host's name, how sessions are placed on it. By default, the machine's.";
    };
    roots = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Where sessions' directories go (`~` is your home). By default ~/dirigent.";
    };
    slots = nullOr types.ints.positive // {
      description = "How many players it runs at once (2 by default).";
    };
    labels = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Words for what it has (e.g. xcode), shown with it.";
    };
    environment = nullOr types.lines // {
      description = "What the players' system prompt says of this machine, besides its system and user.";
    };
    playerArgs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "More of the players' arguments (`dirigent __player --help`).";
    };
    path = mkOption {
      type = types.listOf types.package;
      default = with pkgs; [
        git
        bash
        coreutils
      ];
      defaultText = lib.literalExpression "with pkgs; [ git bash coreutils ]";
      description = "On the players' PATH, before the profiles': the tools their sessions use.";
    };
  };

  # host.toml, of the options set
  config =
    cfg:
    (pkgs.formats.toml { }).generate "host.toml" (
      lib.filterAttrs (_: v: v != null && v != [ ]) {
        inherit (cfg)
          url
          api
          chlive
          players
          name
          roots
          slots
          labels
          environment
          ;
        player_args = cfg.playerArgs;
      }
    );

  # the players' PATH: the tools asked for, then `profiles` (bin directories)
  path = cfg: profiles: lib.concatStringsSep ":" ([ (lib.makeBinPath cfg.path) ] ++ profiles);
}
