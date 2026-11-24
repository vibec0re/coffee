# End-to-end test against a real kernel: three instances, one NixOS
# container (systemd-nspawn) using two of them.
#
#   nix build .#checks.x86_64-linux.vm -L
self:
{ ... }:
{
  name = "coffeefs";

  nodes.machine =
    { ... }:
    {
      imports = [ self.nixosModules.default ];
      virtualisation.memorySize = 2048;

      users.users.alice = {
        isNormalUser = true;
        uid = 1000;
        extraGroups = [ "team" ];
      };
      users.groups.team = { };

      systemd.tmpfiles.rules = [
        "d /srv/shared 0755 root root -"
        "f /srv/shared/hello.txt 0644 root root - hello"
        "d /srv/rw 1777 root root -"
        # writable for alice only via a supplementary group
        "d /srv/rw/team 0770 root team -"
        # setgid: new entries inherit the group (and dirs the bit)
        "d /srv/rw/sg 2777 root team -"
      ];

      services.coffeefs.instances = {
        # plain host mount, poked at directly from the host
        plain.mountpoint = "/mnt/plain";

        # host mount at /containers/choom1, bind-mounted into the container
        choom1 = {
          container = "choom1";
          links = [
            {
              source = "/srv/shared";
              target = "/shared";
              ro = true;
            }
          ];
          dirs = [ "/empty" ];
        };

        # mounted straight into the running container's mount namespace
        choom1ns = {
          container = "choom1";
          inContainerNamespace = true;
          mountpoint = "/coffee";
        };
      };

      containers.choom1 = {
        autoStart = true;
        bindMounts."/work" = {
          hostPath = "/containers/choom1";
          isReadOnly = false;
        };
        config = {
          users.users.alice = {
            isNormalUser = true;
            uid = 1000;
          };
          system.stateVersion = "26.05";
        };
      };
    };

  testScript = ''
    import shlex

    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("container@choom1.service")
    for unit in ["plain", "choom1", "choom1ns"]:
        machine.wait_for_unit(f"coffeefs-{unit}.service")

    def inside(cmd):
        return machine.succeed(f"nixos-container run choom1 -- sh -c {shlex.quote(cmd)}")

    def inside_fails(cmd):
        machine.fail(f"nixos-container run choom1 -- sh -c {shlex.quote(cmd)}")

    with subtest("ro and rw links, read-only skeleton"):
        machine.succeed("coffeefs add plain /srv/shared /s --ro")
        assert machine.succeed("cat /mnt/plain/s/hello.txt").strip() == "hello"
        machine.fail("touch /mnt/plain/s/x")
        machine.fail("touch /mnt/plain/top")
        machine.fail("mkdir /mnt/plain/newdir")
        machine.succeed("coffeefs add plain /srv/rw /rw")
        machine.succeed("echo data > /mnt/plain/rw/f && grep -qx data /srv/rw/f")
        machine.succeed("mkdir /mnt/plain/rw/d && ln -s ../f /mnt/plain/rw/d/l && grep -qx data /mnt/plain/rw/d/l")
        mnt = machine.succeed("grep ' /mnt/plain ' /proc/mounts")
        print(mnt)
        assert all(w in mnt for w in ["fuse.coffeefs", "nosuid", "nodev", "default_permissions", "allow_other"]), mnt

    with subtest("created files belong to the caller, host permissions apply"):
        machine.succeed("su alice -c 'echo hi > /mnt/plain/rw/alice.txt'")
        assert machine.succeed("stat -c %U:%a /srv/rw/alice.txt").strip() == "alice:644"
        machine.succeed("su alice -c 'mkdir /mnt/plain/rw/alicedir'")
        assert machine.succeed("stat -c %U /srv/rw/alicedir").strip() == "alice"
        machine.succeed("echo secret > /srv/rw/rootonly && chmod 600 /srv/rw/rootonly")
        machine.fail("su alice -c 'cat /mnt/plain/rw/rootonly'")
        machine.fail("su alice -c 'rm -f /mnt/plain/rw/rootonly'")  # sticky dir
        machine.succeed("su alice -c 'rm /mnt/plain/rw/alice.txt'")

    with subtest("objects are created as the caller, not chowned afterwards"):
        def owner(p):
            return machine.succeed(f"stat -c %U:%G:%a {p}").strip()
        # the daemon's own umask (022) must not leak into the caller's files
        machine.succeed("su alice -c 'umask 002; echo x > /mnt/plain/rw/u.txt; mkdir /mnt/plain/rw/udir'")
        assert owner("/srv/rw/u.txt") == "alice:users:664", owner("/srv/rw/u.txt")
        assert owner("/srv/rw/udir") == "alice:users:775", owner("/srv/rw/udir")
        machine.succeed("su alice -c 'mkfifo /mnt/plain/rw/fifo; ln -s nowhere /mnt/plain/rw/sym'")
        assert owner("/srv/rw/fifo").startswith("alice:users:"), owner("/srv/rw/fifo")
        assert owner("/srv/rw/sym").startswith("alice:users:"), owner("/srv/rw/sym")
        # access through a supplementary group the daemon thread doesn't have
        machine.succeed("su alice -c 'echo t > /mnt/plain/rw/team/t.txt && mkdir /mnt/plain/rw/team/d'")
        assert owner("/srv/rw/team/t.txt") == "alice:users:644", owner("/srv/rw/team/t.txt")
        # setgid directory: group and bit are inherited by the kernel
        machine.succeed("su alice -c 'echo s > /mnt/plain/rw/sg/f && mkdir /mnt/plain/rw/sg/d'")
        assert owner("/srv/rw/sg/f") == "alice:team:644", owner("/srv/rw/sg/f")
        assert owner("/srv/rw/sg/d") == "alice:team:2755", owner("/srv/rw/sg/d")
        # root stays root
        machine.succeed("echo r > /mnt/plain/rw/root.txt")
        assert owner("/srv/rw/root.txt") == "root:root:644", owner("/srv/rw/root.txt")

    with subtest("daemon's own files are not world-writable"):
        assert machine.succeed("stat -c %a /containers").strip() == "755"
        assert machine.succeed("stat -c %a /var/lib/coffeefs/plain.json").strip() == "600"
        # a tampered (group/world-writable) state file is refused, not replayed
        machine.succeed("cp -a /var/lib/coffeefs/plain.json /tmp/plain.json.bak")
        machine.succeed("chmod 666 /var/lib/coffeefs/plain.json")
        machine.fail("systemctl restart coffeefs-plain")
        machine.succeed("journalctl -u coffeefs-plain | grep -q 'refusing a state file'")
        machine.succeed("cp -a /tmp/plain.json.bak /var/lib/coffeefs/plain.json")
        machine.succeed("systemctl reset-failed coffeefs-plain; systemctl restart coffeefs-plain")
        machine.wait_for_unit("coffeefs-plain.service")
        machine.succeed("grep -qx data /mnt/plain/rw/f")

    with subtest("removal invalidates kernel caches, open files survive"):
        machine.succeed("ls /mnt/plain/s >/dev/null")
        machine.succeed("coffeefs rm plain /s")
        machine.fail("ls /mnt/plain/s")
        machine.succeed("coffeefs add plain /srv/shared /s2")
        machine.succeed("exec 3</mnt/plain/s2/hello.txt; coffeefs rm plain /s2; grep -qx hello <&3")
        machine.fail("test -e /mnt/plain/s2")

    with subtest("single-file links and ls"):
        machine.succeed("coffeefs add plain /srv/shared/hello.txt /etc/greeting --ro")
        machine.succeed("grep -qx hello /mnt/plain/etc/greeting")
        out = machine.succeed("coffeefs ls plain")
        assert "/etc/greeting -> /srv/shared/hello.txt (ro)" in out, out

    with subtest("host-side changes become visible"):
        machine.succeed("cat /mnt/plain/rw/f")
        machine.succeed("echo changed > /srv/rw/f")
        machine.wait_until_succeeds("grep -qx changed /mnt/plain/rw/f", timeout=10)

    with subtest("rename across links is EXDEV, mv falls back to copying"):
        machine.succeed("mkdir -p /srv/rw2 && coffeefs add plain /srv/rw2 /rw2")
        machine.succeed("echo m > /mnt/plain/rw/m && mv /mnt/plain/rw/m /mnt/plain/rw2/m")
        machine.succeed("grep -qx m /srv/rw2/m && test ! -e /srv/rw/m")

    with subtest("control socket is root-only"):
        assert machine.succeed("stat -c %a /run/coffeefs/plain.sock").strip() == "600"
        machine.fail("su alice -c 'coffeefs ls /run/coffeefs/plain.sock'")

    with subtest("links survive restart and crash"):
        machine.succeed("systemctl restart coffeefs-plain")
        machine.wait_for_unit("coffeefs-plain.service")
        machine.succeed("grep -qx hello /mnt/plain/etc/greeting")
        machine.succeed("systemctl kill -s KILL coffeefs-plain")
        machine.wait_until_succeeds("grep -qx hello /mnt/plain/etc/greeting", timeout=30)

    with subtest("container sees the host mount via its bind mount"):
        assert inside("cat /work/shared/hello.txt").strip() == "hello"
        inside("test -d /work/empty")
        inside_fails("touch /work/shared/x")
        # linked while the container is running
        machine.succeed("coffeefs add choom1 /srv/rw /proj")
        inside("echo from-container > /work/proj/c.txt")
        machine.succeed("grep -qx from-container /srv/rw/c.txt")
        inside("su alice -c 'echo a > /work/proj/by-alice'")
        assert machine.succeed("stat -c %U /srv/rw/by-alice").strip() == "alice"
        # control sockets are not reachable from inside
        inside_fails("test -e /run/coffeefs/choom1.sock")
        # instances are independent
        assert "/proj" not in machine.succeed("coffeefs ls plain")

    with subtest("symlinks resolve in the container's view"):
        machine.succeed("mkdir -p /srv/outside && echo topsecret > /srv/outside/secret")
        machine.succeed("ln -s /srv/outside /srv/rw/escape")
        machine.succeed("grep -qx topsecret /mnt/plain/rw/escape/secret")  # host view: fine
        assert inside("readlink /work/proj/escape").strip() == "/srv/outside"
        inside_fails("cat /work/proj/escape/secret")  # no /srv/outside in there

    with subtest("instance mounted inside the container's mount namespace"):
        machine.succeed("coffeefs add choom1ns /srv/shared /data --ro")
        assert inside("cat /coffee/data/hello.txt").strip() == "hello"
        inside("mountpoint -q /coffee")
        machine.fail("mountpoint -q /coffee")
        inside_fails("touch /coffee/data/x")
        machine.succeed("systemctl restart coffeefs-choom1ns")
        machine.wait_for_unit("coffeefs-choom1ns.service")
        machine.wait_until_succeeds("nixos-container run choom1 -- cat /coffee/data/hello.txt", timeout=30)
        assert inside("grep -c ' /coffee ' /proc/mounts").strip() == "1"
  '';
}
