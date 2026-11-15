# The stock players' VM, booted (it needs KVM): a session's commands find the defaults' tools
# first, and `nixpkgs#…` is the image's own nixpkgs, there without the network.
{
  pkgs,
  nixpkgs,
}:
pkgs.testers.runNixOSTest {
  name = "player-vm";
  node.specialArgs.dirigent.defaults = ./defaults.nix;
  nodes.machine =
    { lib, ... }:
    {
      imports = [
        (import ./base.nix { inherit nixpkgs; })
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
      # what a configuration hangs on the session's target, as the player starts and stops it
      systemd.services.session-probe = {
        wantedBy = [ "dirigent-session.target" ];
        partOf = [ "dirigent-session.target" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          EnvironmentFile = "/run/dirigent/session/env";
        };
        script = ''echo "$DIRIGENT_SESSION $(cat /run/dirigent/session/secrets/TS_AUTHKEY)" > /tmp/probe'';
        preStop = "echo stopped > /tmp/probe-stopped";
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

    # the session's target: not at boot; started (as the player does, its files written), its
    # units run with the session's env and secrets; stopped, they stop
    machine.fail("systemctl is-active dirigent-session.target")
    machine.succeed(
        "mkdir -p /run/dirigent/session/secrets"
        " && echo \"DIRIGENT_SESSION='s1'\" > /run/dirigent/session/env"
        " && printf tskey > /run/dirigent/session/secrets/TS_AUTHKEY"
    )
    machine.succeed("systemctl start dirigent-session.target")
    machine.wait_for_file("/tmp/probe")
    assert machine.succeed("cat /tmp/probe") == "s1 tskey\n"
    machine.succeed("systemctl stop dirigent-session.target")
    machine.wait_for_file("/tmp/probe-stopped")
  '';
}
