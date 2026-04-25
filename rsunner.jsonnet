local nproc = std.parseInt(std.extVar('NPROC'));

{
  buildDirectoryPath: std.extVar('PWD') + '/worker/build',
  grpcListenPath: std.extVar('PWD') + '/worker/runner',
  // One slot per logical CPU; for multi-CPU slots use e.g.:
  // cpus: ["%d-%d" % [i * 4, i * 4 + 3] for i in std.range(0, nproc / 4 - 1)],
  cpus: ["%d" % i for i in std.range(0, nproc - 1)],
  memoryMax: 2147483648,
  rwPaths: [
    "/dev",
    "/proc",
    "/tmp",
  ],
  hiddenPaths: [
    "/home",
  ],
  netInterfaces: {
    dummyeth0: {
      addr: "172.16.0.110/24",
      multicast: true,
    },
  },
  cgroup: {
    delegation: true,
  },
  envOverrides: {
    PATH: { prepend: '/run/bb/bin:' },
  },
}
