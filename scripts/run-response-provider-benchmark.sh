#!/usr/bin/env bash
set -Eeuo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd -- "${script_dir}/.." && pwd)"

MODEL="${MODEL:-Qwen/Qwen3.5-35B-A3B-FP8}"
WORKLOAD="${WORKLOAD:-history-rehydration}"
REQUESTS_PER_SESSION="${REQUESTS_PER_SESSION:-}"
DEPTHS="${DEPTHS:-}"
SESSIONS="${SESSIONS:-5}"
TRANSPORT_ROUNDS="${TRANSPORT_ROUNDS:-4}"
PROVIDER="${PROVIDER:-both}"
AGENTIC_URL="${AGENTIC_URL:-http://localhost:9000/v1}"
VLLM_URL="${VLLM_URL:-http://localhost:5050/v1}"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-300}"
SEED="${SEED:-20260817}"
RESULTS_ROOT="${RESULTS_ROOT:-${repository_root}/target/response-provider-benchmark}"
DATASET_QUESTIONS="${DATASET_QUESTIONS:-}"
DATASET_ANSWERS="${DATASET_ANSWERS:-}"
DATASET_OFFSET="${DATASET_OFFSET:-0}"
BFCL_ROOT="${BFCL_ROOT:-}"
BFCL_CATEGORY="${BFCL_CATEGORY:-simple_python}"
PRINT_PROMPTS="${PRINT_PROMPTS:-0}"
GATEWAY_PID="${GATEWAY_PID:-}"

usage() {
  cat <<'EOF'
Run optimized Responses capability benchmarks from the agentic-server bench target.

Usage:
  ./scripts/run-response-provider-benchmark.sh

Core environment variables:
  MODEL                  Model served by all selected endpoints.
  WORKLOAD               transport, tool-call, or history-rehydration.
  SESSIONS               Parallel sessions per provider (default: 5). A comma-separated list, e.g. 1,5,10,25,50,
                         runs one independent batch per level, in order, each under its own sessions-NNNNN/
                         directory of the run, and writes sessions_summary.md beside them.
  REQUESTS_PER_SESSION   Sequential turns per session; unset uses the workload default.
  DEPTHS                 Comma-separated fixed turn depths (history-rehydration only), e.g.
                         1,5,10,25,50,100. Each depth runs as its own independent batch of SESSIONS
                         sessions instead of bucketing one long run after the fact, so sample counts
                         stay even across depths. Overrides REQUESTS_PER_SESSION when set. Produces
                         depth_summary.md/.json under RESULTS_ROOT with request/response bytes and
                         accuracy versus turn depth.
  PROVIDER               all, both, agentic-api, agentic-api-http, agentic-api-json,
                         vllm, or vllm-json.
  AGENTIC_URL            Agentic API base URL (default: http://localhost:9000/v1).
  VLLM_URL               Direct vLLM base URL (default: http://localhost:5050/v1).
  TRANSPORT_ROUNDS       Sequential model/tool rounds in each transport turn (default: 4).
  SEED                   Deterministic prompt/case seed shared across separate runs.
  PRINT_PROMPTS          Set to 1 to validate and print generated prompt JSONL without running.

BFCL tool-call workload:
  BFCL_ROOT              Checkout of https://github.com/EnlightenedAI/BFCL.
  BFCL_CATEGORY          v4 filename suffix, such as simple_python or multiple.
  DATASET_QUESTIONS      Explicit BFCL question JSONL; overrides BFCL_ROOT derivation.
  DATASET_ANSWERS        Explicit BFCL possible-answer JSONL.
  DATASET_OFFSET         First deterministic case index (default: 0).

Other controls:
  TIMEOUT_SECONDS RESULTS_ROOT
  GATEWAY_PID            Process ID of a local Agentic API server (Linux). Its CPU time and resident memory are read
                         from /proc once a second during each batch (gateway-usage.csv in the batch's directory) and
                         summarized per level in sessions_summary.md.

Examples:
  WORKLOAD=transport PROVIDER=all TRANSPORT_ROUNDS=8 ./scripts/run-response-provider-benchmark.sh

  WORKLOAD=transport PROVIDER=agentic-api-json ./scripts/run-response-provider-benchmark.sh

  WORKLOAD=transport PROVIDER=all SESSIONS=1,5,10,25,50 GATEWAY_PID="$(pgrep -nx agentic-server)" \
    ./scripts/run-response-provider-benchmark.sh

  WORKLOAD=tool-call PROVIDER=agentic-api BFCL_ROOT=/path/to/BFCL \
    REQUESTS_PER_SESSION=1 ./scripts/run-response-provider-benchmark.sh

EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi
if (( $# != 0 )); then
  echo "error: unexpected argument: $1" >&2
  usage >&2
  exit 2
fi

for command_name in cargo tee; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "error: ${command_name} is required" >&2
    exit 2
  fi
done

for numeric_value in TRANSPORT_ROUNDS TIMEOUT_SECONDS; do
  if [[ ! "${!numeric_value}" =~ ^[1-9][0-9]*$ ]]; then
    echo "error: ${numeric_value} must be a positive integer" >&2
    exit 2
  fi
done
if [[ ! "$SESSIONS" =~ ^[1-9][0-9]*(,[1-9][0-9]*)*$ ]]; then
  echo "error: SESSIONS must be a positive integer or a comma-separated list of them" >&2
  exit 2
fi
if [[ -n "$GATEWAY_PID" && ! -r "/proc/${GATEWAY_PID}/stat" ]]; then
  echo "error: GATEWAY_PID ${GATEWAY_PID} is not a running process with a /proc entry (sampling needs Linux)" >&2
  exit 2
fi
if [[ -n "$REQUESTS_PER_SESSION" && ! "$REQUESTS_PER_SESSION" =~ ^[1-9][0-9]*$ ]]; then
  echo "error: REQUESTS_PER_SESSION must be a positive integer when set" >&2
  exit 2
fi
if [[ -n "$DEPTHS" ]]; then
  if [[ ! "$DEPTHS" =~ ^[1-9][0-9]*(,[1-9][0-9]*)*$ ]]; then
    echo "error: DEPTHS must be a comma-separated list of positive integers" >&2
    exit 2
  fi
  if [[ "$WORKLOAD" != "history-rehydration" ]]; then
    echo "error: DEPTHS is only supported for WORKLOAD=history-rehydration" >&2
    exit 2
  fi
fi
if [[ ! "$DATASET_OFFSET" =~ ^[0-9]+$ || ! "$SEED" =~ ^[0-9]+$ ]]; then
  echo "error: DATASET_OFFSET and SEED must be non-negative integers" >&2
  exit 2
fi
case "$WORKLOAD" in
  transport | tool-call | history-rehydration) ;;
  *)
    echo "error: unsupported WORKLOAD: ${WORKLOAD}" >&2
    exit 2
    ;;
esac
case "$PROVIDER" in
  all | both | agentic-api | agentic-api-http | agentic-api-json | vllm | vllm-json) ;;
  *)
    echo "error: unsupported PROVIDER: ${PROVIDER}" >&2
    exit 2
    ;;
esac

if [[ "$WORKLOAD" == "tool-call" && -z "$DATASET_QUESTIONS" && -n "$BFCL_ROOT" ]]; then
  bfcl_data_dir="${BFCL_ROOT%/}/berkeley-function-call-leaderboard/bfcl_eval/data"
  DATASET_QUESTIONS="${bfcl_data_dir}/BFCL_v4_${BFCL_CATEGORY}.json"
  DATASET_ANSWERS="${bfcl_data_dir}/possible_answer/BFCL_v4_${BFCL_CATEGORY}.json"
fi
if [[ "$WORKLOAD" == "tool-call" && ( -z "$DATASET_QUESTIONS" || -z "$DATASET_ANSWERS" ) ]]; then
  echo "error: tool-call requires BFCL_ROOT or both DATASET_QUESTIONS and DATASET_ANSWERS" >&2
  exit 2
fi
for dataset_path in "$DATASET_QUESTIONS" "$DATASET_ANSWERS"; do
  if [[ -n "$dataset_path" && ! -f "$dataset_path" ]]; then
    echo "error: dataset file not found: ${dataset_path}" >&2
    exit 2
  fi
done
run_stamp="$(date -u +%Y%m%dT%H%M%SZ)-$$"
run_dir="${RESULTS_ROOT%/}/${run_stamp}"
mkdir -p "$run_dir"

benchmark_command=(
  cargo bench --package agentic-server --bench response_provider --
  --model "$MODEL"
  --workload "$WORKLOAD"
  --transport-rounds "$TRANSPORT_ROUNDS"
  --provider "$PROVIDER"
  --agentic-url "$AGENTIC_URL"
  --vllm-url "$VLLM_URL"
  --timeout-seconds "$TIMEOUT_SECONDS"
  --seed "$SEED"
  --dataset-offset "$DATASET_OFFSET"
  --live-jsonl
)
if [[ -n "$DEPTHS" ]]; then
  benchmark_command+=(--depths "$DEPTHS")
elif [[ -n "$REQUESTS_PER_SESSION" ]]; then
  benchmark_command+=(--requests-per-session "$REQUESTS_PER_SESSION")
fi
if [[ -n "$DATASET_QUESTIONS" ]]; then
  benchmark_command+=(--dataset-questions "$DATASET_QUESTIONS" --dataset-answers "$DATASET_ANSWERS")
fi
if [[ "$PRINT_PROMPTS" == "1" ]]; then
  benchmark_command+=(--print-prompts)
fi

# One sample of the gateway: the time in Unix milliseconds, its CPU time (utime + stime) in seconds and its resident
# memory in KiB. Fails once the process is gone.
clock_ticks="$(getconf CLK_TCK)"
gateway_sample() {
  awk -v hz="$clock_ticks" -v now="$(date +%s%3N)" '
    FNR == NR { sub(/^.*\) /, ""); cpu = ($12 + $13) / hz; next }
    /^VmRSS:/ { printf "%s,%.2f,%s\n", now, cpu, $2 }
  ' "/proc/${GATEWAY_PID}/stat" "/proc/${GATEWAY_PID}/status" 2>/dev/null
}

# run_batch SESSIONS DIR: one benchmark batch at SESSIONS sessions per provider, with its results in DIR.
run_batch() {
  local sessions="$1" batch_dir="$2" sampler="" status=0
  local command=("${benchmark_command[@]}" --sessions "$sessions" --output-dir "$batch_dir")
  mkdir -p "$batch_dir"
  {
    printf 'cd %q\n' "$repository_root"
    printf '%q ' "${command[@]}"
    printf '\n'
  } >"${batch_dir}/command.sh"
  echo "Benchmark results:  ${batch_dir} (${sessions} sessions per provider)" >&2
  echo "Live JSONL:        ${batch_dir}/live-events.jsonl" >&2
  echo "Diagnostic log:    ${batch_dir}/benchmark.log" >&2
  if [[ -n "$GATEWAY_PID" ]]; then
    echo "unix_ms,cpu_seconds,rss_kib" >"${batch_dir}/gateway-usage.csv"
    while gateway_sample >>"${batch_dir}/gateway-usage.csv"; do sleep 1; done &
    sampler=$!
  fi
  "${command[@]}" \
    > >(tee "${batch_dir}/live-events.jsonl") \
    2> >(tee "${batch_dir}/benchmark.log" >&2) || status=$?
  if [[ -n "$sampler" ]]; then
    kill "$sampler" 2>/dev/null || true
    wait "$sampler" 2>/dev/null || true
    gateway_sample >>"${batch_dir}/gateway-usage.csv" || true
  fi
  echo "Benchmark exit status: ${status}" >&2
  echo "Summary: ${batch_dir}/summary.md" >&2
  return "$status"
}

# A one-level run keeps its results in the run directory itself; a sweep gives each level a directory of its own.
IFS=, read -r -a session_levels <<<"$SESSIONS"
batch_dirs=() batch_statuses=()
for sessions in "${session_levels[@]}"; do
  if (( ${#session_levels[@]} == 1 )); then
    batch_dirs+=("$run_dir")
  else
    batch_dirs+=("$(printf '%s/sessions-%05d' "$run_dir" "$sessions")")
  fi
done

echo "Benchmark workload: ${WORKLOAD}" >&2
cd "$repository_root"
benchmark_status=0
for i in "${!session_levels[@]}"; do
  status=0
  run_batch "${session_levels[$i]}" "${batch_dirs[$i]}" || status=$?
  batch_statuses+=("$status")
  if (( benchmark_status == 0 )); then
    benchmark_status="$status"
  fi
done

if (( ${#session_levels[@]} > 1 )) || [[ -n "$GATEWAY_PID" ]]; then
  {
    echo "# Responses provider benchmark: ${WORKLOAD} by sessions per provider"
    echo
    echo "Each level ran as its own batch, in this order. Its summary.md has the latency, throughput and success of every"
    echo "provider at that level."
    if [[ -n "$GATEWAY_PID" ]]; then
      echo "Gateway columns are process ${GATEWAY_PID}'s CPU time and resident memory over the batch, sampled once a second."
    fi
    echo
    echo "| Sessions per provider | Exit status | Gateway CPU (s) | Gateway cores, mean | Gateway peak RSS (MiB) | Summary |"
    echo "| ---: | ---: | ---: | ---: | ---: | --- |"
    for i in "${!session_levels[@]}"; do
      usage="| n/a | n/a | n/a"
      if [[ -s "${batch_dirs[$i]}/gateway-usage.csv" ]]; then
        usage="$(awk -F, '
          NR == 2 { t0 = $1; c0 = $2 }
          NR > 1 { t1 = $1; c1 = $2; if ($3 > rss) rss = $3 }
          END {
            if (NR < 3) { print "| n/a | n/a | n/a"; exit }
            printf "| %.2f | %.2f | %.1f", c1 - c0, (c1 - c0) / ((t1 - t0) / 1000), rss / 1024
          }
        ' "${batch_dirs[$i]}/gateway-usage.csv")"
      fi
      summary="summary.md"
      if [[ "${batch_dirs[$i]}" != "$run_dir" ]]; then
        summary="${batch_dirs[$i]##*/}/summary.md"
      fi
      echo "| ${session_levels[$i]} | ${batch_statuses[$i]} ${usage} | [${summary}](${summary}) |"
    done
  } >"${run_dir}/sessions_summary.md"
  echo "Sessions summary: ${run_dir}/sessions_summary.md" >&2
fi
exit "$benchmark_status"
