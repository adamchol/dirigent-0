# The players' VMs' configuration, unless the chart has one of its own (`player.nixos.files`),
# which replaces it; `imports = [ dirigent.defaults ];` there keeps it. What a session's commands
# find installed; anything else is a `nix shell nixpkgs#…` away.
{ pkgs, ... }:
{
  environment.systemPackages = with pkgs; [
    # the basics
    coreutils
    findutils
    gnugrep
    gnused
    gawk
    diffutils
    patch
    file
    less
    tree
    which
    procps
    # archives
    gnutar
    gzip
    unzip
    zip
    xz
    # the network
    curl
    wget
    openssh
    # searching, and data
    ripgrep
    fd
    jq
    # building
    git
    gh
    gcc
    gnumake
    pkg-config
    python3
  ];
}
