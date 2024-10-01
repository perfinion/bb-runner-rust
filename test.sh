#!/bin/bash

die() {
    echo "ERROR: $*" >&2
    exit 1
}

[[ -f ./build.rs ]] || die "Must be run from base of repo"

create_test_files() {
    dir="$1"

    rm -rf ./${dir}/
    mkdir -p ./${dir}/{tmp,logs}

    cat >./${dir}/run.sh <<EOF
#!/bin/bash

echo Test Child in ${dir}
echo Test Child stderr >&2

echo -n "PWD: [\${PWD}] "; pwd
echo "Args: [\$@]"
echo "Hostname: \$(hostname)"
echo -n "nproc: "; nproc

env
sleep \${1:-2}
ls -al /proc/\$\$/ns/* >&2
ps auxf
[[ $dir == *2 ]] && kill \$\$
exit 0
EOF

    chmod +x ./${dir}/run.sh
}


create_test_files test_base1
create_test_files test_base2

run_grpcurl() {
    grpcurl \
        -import-path proto/resourceusage/ \
        -import-path proto/runner/ \
        -proto runner.proto \
        -proto resourceusage.proto \
        -plaintext \
        $@
}

send_run() {
    dir="$1"
    time="$(( $RANDOM % 3 + ${2:-1} ))"

    # proto/resourceusage/resourceusage.proto
    # proto/runner/runner.proto
    # proto/configuration/bb_runner/bb_runner.proto

    echo -e "\nRunning ${dir} with timeout=${time} and input data:"
    run_grpcurl \
        -d @ \
        -unix /tmp/tonic/helloworld \
        buildbarn.runner.Runner/Run <<EOM
{
  "arguments": [
    "/usr/bin/time",
    "-v",
    "bash",
    "./run.sh",
    "${time}",
    "bar"
  ],
  "environment_variables": {
    "AN_ENV_VAR": "hello world",
    "HOME": "${HOME}",
    "TMP": "${TMP:-/tmp}"
  },
  "working_directory": "${dir}/",
  "stdout_path": "stdout.txt",
  "stderr_path": "stderr.txt",
  "input_root_directory": "$(pwd)",
  "temporary_directory": "/tmp",
  "server_logs_directory": "${dir}/logs"
}
EOM

    ls -al ./${dir}/
}

( sleep 1; send_run test_base1 5 ) &
send_run test_base2 5
wait

echo "Both finished!"

echo "=== test_base1/stdout.txt"
grep -H . test_base1/stdout.txt
echo "=== test_base1/stderr.txt"
grep -H . test_base1/stderr.txt

echo
echo "=== test_base2/stdout.txt"
grep -H . test_base2/stdout.txt
echo "=== test_base2/stderr.txt"
grep -H . test_base2/stderr.txt

exit

echo -e "\nCheckReadiness with no path:"
run_grpcurl -unix /tmp/tonic/helloworld buildbarn.runner.Runner/CheckReadiness

echo -e "\nCheckReadiness with existing path:"
run_grpcurl -d @ -unix /tmp/tonic/helloworld buildbarn.runner.Runner/CheckReadiness <<EOM
{
  "path": "build.rs"
}
EOM

echo -e "\nRun no data:"
run_grpcurl -unix /tmp/tonic/helloworld buildbarn.runner.Runner/Run
