{...}: {
  microvm = {
    hypervisor = "qemu";
    qemu.machine = "microvm";

    optimize.enable = true;

    vcpu = 2;
    # don't set to 2048, https://github.com/microvm-nix/microvm.nix/issues/171
    mem = 2049;

    interfaces = [
      {
        type = "user";
        id = "net0";
        mac = "02:00:00:00:10:01";
      }
    ];
    vsock.cid = 3;

    socket = "control.socket";
  };

  boot.kernelModules = ["vsock_loopback"];
}
