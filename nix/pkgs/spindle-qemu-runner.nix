{lib}: {
  # runner args for the arch and machine the guest was built for
  mkQemuRunner = {
    arch ? "x86_64",
    machine ? "microvm",
    pcie ? false,
    virtioTransport ? null,
  }: let
    isX86 = arch == "x86_64";
    machineOpts = {
      microvm = "acpi=on,mem-merge=on,pcie=${
        if pcie
        then "on"
        else "off"
      },pic=off,pit=off,rtc=on,usb=off";
      virt = "gic-version=max,its=off,msi=off,mem-merge=on";
    };
  in
    {
      # qemu < 10.0 crashes when a microvm guest reads the sgx cpuid leaf
      # https://gitlab.com/qemu-project/qemu/-/issues/2142
      cpu =
        if isX86
        then "host,+x2apic,-sgx"
        else "host";
      machine = "${machine},accel=kvm:tcg,${machineOpts.${machine}}";
      console = "hvc0";
      # i8042 only exists on x86, it is the kernel's reset path there
      extraArgs = lib.optionals isX86 ["-device" "i8042"];
    }
    // lib.optionalAttrs (virtioTransport != null) {inherit virtioTransport;};
}
