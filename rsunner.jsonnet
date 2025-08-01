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
}
