#!/bin/bash

rm -rf test/
mkdir -p test/{tmp,logs}

cat >test/run.sh <<EOF
#!/bin/bash

echo Test Child
echo Test Child stderr >&2

echo "PWD: [\${PWD}]"
pwd
echo "Args: [\$@]"

echo Done!
env
sleep \${1:-2}
exit 0
EOF

chmod +x test/run.sh


echo -e "\nRun with testing data:"
grpcurl -d @ -proto proto/runner/runner.proto -plaintext -unix /tmp/tonic/helloworld buildbarn.runner.Runner/Run <<EOM
{
  "arguments": [
    "run.sh",
    "$(( $RANDOM % 5 + 1 ))",
    "bar"
  ],
  "environment_variables": {
    "AN_ENV_VAR": "hello world",
    "HOME": "${HOME}",
    "TMP": "${TMP:-/tmp}"
  },
  "working_directory": "test/",
  "stdout_path": "stdout.txt",
  "stderr_path": "stderr.txt",
  "input_root_directory": "$(pwd)",
  "temporary_directory": "/tmp",
  "server_logs_directory": "test/logs"
}
EOM

ls -al ./test/

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
