{
  buildDirectoryPath: std.extVar('PWD') + '/worker/build',
  grpcListenPath: std.extVar('PWD') + '/worker/runner',
  numCpus: 0, // Autodetect
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
