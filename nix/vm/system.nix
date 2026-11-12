# A players' VM's NixOS system: what dirigent needs (base.nix), and a configuration: defaults.nix,
# or the cluster's own (`player.nixos.files`), whose modules get `dirigent.defaults` to import.
# `checked` builds the cluster's: its system, unless the configuration took away what dirigent
# needs (mkForce can't keep a module from overriding anything, so that's checked from outside).
{
  nixpkgs,
  dirigent,
  system,
}:
let
  lib = import "${nixpkgs}/lib";
  eval =
    configuration:
    import "${nixpkgs}/nixos/lib/eval-config.nix" {
      system = null;
      specialArgs.dirigent.defaults = ./defaults.nix;
      modules = [
        { nixpkgs.hostPlatform = system; }
        (import ./base.nix { inherit dirigent nixpkgs; })
        configuration
      ];
    };
in
{
  inherit eval;

  checked =
    configuration:
    let
      built = eval configuration;
      stock = eval ./defaults.nix;
      unit = s: s.config.systemd.services.player;
      mount = s: path: {
        inherit (s.config.fileSystems.${path} or { }) device fsType;
      };
      paths = s: map toString (unit s).path;
      kept = {
        "the player's service is enabled" = (unit built).enable;
        "the player's service starts with the system" = builtins.elem "multi-user.target" (unit built)
        .wantedBy;
        "the player's service runs the player" = (unit built).script == (unit stock).script;
        "the player's service has the player's tools" = paths built == paths stock;
        "the session's environment is mounted" =
          mount built "/mnt/dirigent/config" == mount stock "/mnt/dirigent/config";
        "the workspace's disk is mounted" =
          mount built "/var/lib/dirigent" == mount stock "/var/lib/dirigent";
        "dirigent's cache is trusted" =
          lib.hasInfix "!include /run/dirigent/nix.conf" built.config.nix.extraOptions;
      };
      lost = builtins.filter (n: !kept.${n}) (builtins.attrNames kept);
    in
    if lost == [ ] then
      built.config.system.build.toplevel
    else
      throw "the configuration changes what dirigent needs: ${builtins.concatStringsSep "; " lost}";
}
