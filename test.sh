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

echo "PWD: [\${PWD}]"
pwd
echo "Args: [\$@]"

echo Done!
env
sleep \${1:-2}
ls -al
exit 0
EOF

    chmod +x ./${dir}/run.sh
}


create_test_files test_base1
create_test_files test_base2


send_run() {
    dir="$1"
    time="$(( $RANDOM % 3 + ${2:-1} ))"

    echo -e "\nRunning ${dir} with timeout=${time} and input data:"
    time grpcurl -d @ -proto proto/runner/runner.proto -plaintext -unix /tmp/tonic/helloworld buildbarn.runner.Runner/Run <<EOM
{
  "arguments": [
    "run.sh",
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
grpcurl -proto proto/runner/runner.proto -plaintext -unix /tmp/tonic/helloworld buildbarn.runner.Runner/CheckReadiness

echo -e "\nCheckReadiness with existing path:"
grpcurl -d @ -proto proto/runner/runner.proto -plaintext -unix /tmp/tonic/helloworld buildbarn.runner.Runner/CheckReadiness <<EOM
{
  "path": "build.rs"
}
EOM

echo -e "\nRun no data:"
grpcurl -proto proto/runner/runner.proto -plaintext -unix /tmp/tonic/helloworld buildbarn.runner.Runner/Run
