#!/usr/bin/env bash
set -euo pipefail

# Run only inside a transient, delegated cgroup scope. The caller enters a
# gateway leaf before any server or test runtime starts, so worker cgroups can
# be created as siblings under the delegated parent.
if (( $# == 0 )); then
  echo 'usage: with-code-interpreter-cgroup.sh COMMAND [ARG ...]' >&2
  exit 2
fi

scope="$(awk -F: '$1 == "0" { print $3 }' /proc/self/cgroup)"
if [[ -z "$scope" || "$scope" == / ]]; then
  echo 'error: a delegated cgroup v2 scope is required' >&2
  exit 1
fi

scope_root="/sys/fs/cgroup${scope}"
if [[ ! -w "$scope_root/cgroup.procs" || ! -w "$scope_root/cgroup.subtree_control" ]]; then
  echo 'error: the current cgroup scope is not delegated to this user' >&2
  exit 1
fi

controllers="$(<"$scope_root/cgroup.controllers")"
if [[ " $controllers " != *' memory '* || " $controllers " != *' pids '* ]]; then
  echo 'error: the delegated scope needs memory and pids controllers' >&2
  exit 1
fi

gateway_leaf="$scope_root/gateway-$$"
mkdir "$gateway_leaf"
printf '%s\n' "$$" > "$gateway_leaf/cgroup.procs"
if [[ -s "$scope_root/cgroup.procs" ]]; then
  echo 'error: other processes occupy the delegated scope parent' >&2
  exit 1
fi
printf '+memory +pids\n' > "$scope_root/cgroup.subtree_control"

exec "$@"
