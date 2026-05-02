# bb-runner-rust

A Rust implementation of Buildbarn's `bb_runner`, the small per-host service
that [`bb_worker`](https://github.com/buildbarn/bb-remote-execution) calls into
to actually execute build actions.

`bb_worker` does not fork build commands itself; instead it delegates over a
UNIX-domain gRPC socket to a runner that implements the
[`buildbarn.runner.Runner`](proto/runner/runner.proto) service. This project is
a drop-in replacement for the upstream Go runner with a focus on much stricter
Linux sandboxing.

The upstream bb-runner is generic for any UNIX system and usually relies on
kubernetes/docker for isolation between jobs. This bb_runner is much more
focused on the embedded use-case where jobs are run and need to rely on
specifics of the BSP and not much else (ie not a full generic linux server).
Additionally this runner makes no attempt at supporting non-Linux platforms. It
makes heavy use of Linux sandboxing primitives.

This project started because I came across [this github issue](https://github.com/buildbarn/bb-remote-execution/issues/68#issuecomment-681818465)
some time ago and had wanted some excuse to learn Rust. It was also around the
time of BazelCon so I started this mostly as a way to learn and have fun. The
sandboxing is focused on making sure bazel tests and actions are as isolated as
possible to prevent noisy-neighbour problems where tests could interfere with
each other (eg over IPC/Shared Memory). Additionally it can enforce strict
CPU-core and Memory usage limits per action with cgroups. There is no threat
model for protecting against malicious actions, this is entirely focused on
accidental build problems (eg a bazel test consumes huge amounts of RAM and
the dev does not notice).

## Features

- Implements the `buildbarn.runner.Runner` gRPC service (`CheckReadiness`,
  `Run`) over a UNIX socket, with reflection enabled for easy debugging via
  `grpcurl`.
- Per-action sandboxing using Linux namespaces (user, mount, network, UTS, PID)
  with optional fixed UID/GID mapping.
- Mount sandboxing via per-action `rwPaths` (writable bind mounts) and
  `hiddenPaths` (masked with empty `tmpfs`).
- Optional virtual network interfaces (`netInterfaces`) per action — useful
  for tests that want multicast traffic without interfering across actions
  or the host network.
- cgroup v1 and v2 support (v2 uses systemd cgroup delegation): each concurrent
  slot is pinned to separate CPU cores and optional max memory limit.
- Returns POSIX rusage (`getrusage`) data back to `bb_worker` via the
  `buildbarn.resourceusage.PosixResourceUsage` message.
- Configurable environment overrides (`envOverrides`) for prepending /
  appending to specific variables (e.g. `PATH`) for dealing with strange
  embedded systems.
- Acts as a proper PID 1 reaper when run inside a container so orphaned
  grandchildren are cleaned up.
- Configuration in Jsonnet, with all process
  environment variables exposed as `extVar`s plus the runner's pwd and
  number of cpus.
- Static `musl` binaries for `linux/amd64` and `linux/arm64`, packaged as
  scratch images suitable for use as a Kubernetes init container.

## Building

### From source

Requires a recent stable Rust toolchain and `protoc`.

```sh
cargo build --release
```

The binary lands at `target/release/bb_runner`.

### Container images

The [`Dockerfile`](Dockerfile) cross-compiles fully static `musl` binaries for
amd64 & aarch64. Produces plain and installer containers like official
bb_runner.

```sh
# Standalone runner on `scratch`:
docker buildx build --target runner \
    --platform linux/amd64,linux/arm64 \
    -t bb_runner .

# Installer image (default target) — copies the binary into a mounted
# volume. Use as a Kubernetes init container with an emptyDir at /bb/.
docker buildx build \
    --platform linux/amd64,linux/arm64 \
    -t bb_runner_installer .
```

CI publishes images to `ghcr.io/<owner>/bb-runner-rust`

## Running

```sh
SIZECLASS=2 bb_runner path/to/config.jsonnet
```

Logging is controlled by the standard `RUST_LOG` env var (default: `debug`).

A minimal config is in [`rsunner.jsonnet`](rsunner.jsonnet); a fuller example
illustrating every supported field:

```jsonnet
local nproc = std.parseInt(std.extVar('NPROC'));
local sizeClass = std.max(1, std.parseInt(std.extVar('SIZECLASS')));

{
  buildDirectoryPath: std.extVar('PWD') + '/worker/build',
  grpcListenPath:     std.extVar('PWD') + '/worker/runner',

  // One slot per logical CPU. For multi-CPU slots use cpuset ranges, e.g.
  // cpus: ["%d-%d" % [i * 4, i * 4 + 3] for i in std.range(0, nproc / 4 - 1)],
  // cpus: ["%d" % i for i in std.range(0, nproc - 1)],

  // Size class 1 gets 1 core for each action, class 2 gets 2 cores each ...
  cpus: [
    "%d-%d" % [i * sizeClass, i * sizeClass + (sizeClass - 1)]
    for i in std.range(0, std.floor(nproc / sizeClass) - 1)
  ],

  // Optional per-action memory.max in bytes. 3 GiB * sizeClass per action
  memoryMax: (sizeClass * 3) * 1024 * 1024 * 1024,

  // Bind-mounted into each action read-write. /tmp is writable, but since
  // hidden as well, will be separate from the outer /tmp
  rwPaths: ["/dev", "/proc", "/tmp"],

  // Masked with an empty tmpfs so actions can't see them.
  hiddenPaths: ["/home", "/tmp"],

  // Virtual interfaces created inside the per-action net namespace.
  // This interface goes nowhere, only to help multicast testing which
  // lo does not allow.
  netInterfaces: {
    dummyeth0: { addr: "172.16.0.110/24", multicast: true },
  },

  // Use cgroup delegated to bb_runner by systemd to run each child in separate
  // cgroup. requires that the parent cgroup has the necessary controllers
  // delegated. (eg systemd `Delegate=`)
  cgroup: { delegation: true },

  // Modify env vars handed to the child (in addition to those provided
  // by bb_worker on the RunRequest). If $PATH is not set, this will not
  // prepend/append. Use bb_worker env overrides to set a default.
  envOverrides: {
    PATH: { prepend: '/run/bb/bin:', append: '/run/bb/otherbin' },
  },
}
```

### Configuration reference

| Field | Type | Description |
| --- | --- | --- |
| `buildDirectoryPath` | path | Root containing per-action input directories. Required. |
| `grpcListenPath` | path | UNIX socket path the runner listens on. Required. |
| `cpus` | list of cpuset strings | One entry per concurrency slot. Defaults to one slot per logical CPU. |
| `memoryMax` | bytes | Optional cgroup v2 `memory.max` per action. |
| `rwPaths` | list of paths | Host paths bind-mounted read-write into the sandbox. |
| `hiddenPaths` | list of paths | Host paths masked with an empty tmpfs. |
| `netInterfaces` | map | Virtual interfaces (with `addr`, `multicast`) created in the action net namespace. |
| `userNamespace` | bool | Run actions in a user namespace. |
| `runAsUser` / `runAsGroup` | uint | UID/GID to map the action to inside the user namespace. |
| `cgroup.delegation` | bool | Place per-action cgroups inside the cgroup bb_runner was handed by its parent cgroup manager (the systemd `Delegate=` model). Recommended on cgroup v2. |
| `cgroup.path` | string | Full path of the parent cgroup, relative to `/sys/fs/cgroup` (or to each controller hierarchy on v1) — bb_runner manages this cgroup directly. Default `bb_runner`. Ignored on v2 when `delegation: true`. |
| `envOverrides` | map | `{ VAR: { prepend, append } }` modifications to env vars from the `RunRequest`. |

On a cgroup v2 system, you should set `cgroup: { delegation: true }` and leave
`cgroup.path` alone — systemd is the single owner of the v2 hierarchy and
bb_runner just operates inside whatever slice it was handed (e.g. via a unit
with `Delegate=yes`; see [systemd's Cgroup Delegation
docs](https://systemd.io/CGROUP_DELEGATION/) for the full rules). systemd does
not support delegation on cgroup v1 (it considers v1 unsafe to delegate), so
on v1 you set `cgroup.path` to a path bb_runner will manage directly.
Configuring a separate systemd slice and pointing `cgroup.path` at it is
technically possible with delegation: false on v2, but delegation is much better so just use that.

All process environment variables are also exposed to the Jsonnet evaluator
as `std.extVar(NAME)`, plus the synthesised `PWD` (current working directory)
and `NPROC` (logical CPU count).

## Testing

[`test.sh`](test.sh) drives the runner end-to-end with `grpcurl`. Start the
runner in another terminal first:

```sh
mkdir -p /tmp/tonic
RUST_LOG=debug ./target/debug/bb_runner ./rsunner.jsonnet
./test.sh
```

The script issues two concurrent `Run` RPCs and prints the captured stdout /
stderr from each action.

## License

Copyright [Jason Zaman](https://github.com/perfinion) 2024-2026.

Apache License 2.0. See [LICENSE](LICENSE).
