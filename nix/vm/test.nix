# The stock players' VM, booted (it needs KVM): a session's commands find the defaults' tools
# first, and `nixpkgs#…` is the image's own nixpkgs, there without the network.
{
  pkgs,
  nixpkgs,
  dirigent,
}:
pkgs.testers.runNixOSTest {
  name = "player-vm";
  node.specialArgs.dirigent.defaults = ./defaults.nix;
  nodes.machine =
    { lib, ... }:
    {
      imports = [
        (import ./base.nix { inherit dirigent nixpkgs; })
        ./defaults.nix
      ];
      # no KubeVirt, so no session: the player's unit runs what its commands would find
      systemd.services.player = {
        unitConfig.SuccessAction = lib.mkForce "none";
        script = lib.mkForce ''
          for tool in curl jq python3 gh git rg gcc make; do command -v "$tool"; done > /tmp/found
          python3 -c 'print("python runs")' >> /tmp/found
          nix eval --raw nixpkgs#hello.version > /tmp/eval 2>&1 || echo failed >> /tmp/eval
          mv /tmp/eval /tmp/hello
          sleep infinity
        '';
      };
    };
  testScript = ''
    machine.wait_for_file("/tmp/hello")
    found = machine.succeed("cat /tmp/found")
    print(found)
    for tool in ["curl", "jq", "python3", "gh", "git", "rg", "gcc", "make"]:
        assert f"/run/current-system/sw/bin/{tool}\n" in found, f"{tool} isn't the system's"
    assert "python runs" in found
    hello = machine.succeed("cat /tmp/hello")
    assert hello[0].isdigit() and "failed" not in hello, hello
  '';
}
