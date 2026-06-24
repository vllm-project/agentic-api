#!/bin/bash
# Records stateful multi-turn tool-call cassettes using record_cassette.py
# Backend: vLLM gpt-oss-20b with VLLM_ENABLE_RESPONSES_API_STORE=1
# Scenario: SRE debugging failed ETL pipeline job-382
#
# IMPORTANT: These cassettes prove context retention via ambiguous prompts.
# Turns 2+ use pronouns ("that job", "it", "those errors") that can ONLY resolve
# correctly if previous_response_id preserves server-side conversation state.
#
# Prerequisites:
#   - SSH tunnel to G6e instance: ssh -L 8100:localhost:8100 ubuntu@<G6e-IP>
#   - gpt-oss container running with VLLM_ENABLE_RESPONSES_API_STORE=1
#   - Tools file at /tmp/pipeline_tools.json (6 tools: get_job_status,
#     get_error_logs, search_runbook, run_analysis, restart_job, web_search)

set -euo pipefail

RECORDER="$(dirname "$0")/../../cassettes/record_cassette.py"
TOOLS="/tmp/pipeline_tools.json"
OUTPUT_DIR="$(dirname "$0")"
VLLM_URL="http://localhost:8100"
MODEL="openai/gpt-oss-20b"

echo "=== 3-turn non-streaming (context retention: 'that job' resolves to job-382) ==="
printf '%s\n' \
  "You are an SRE assistant. Check the current status of ETL pipeline job-382." \
  "Now pull the error logs for that job. Use severity ERROR and max 10 entries." \
  "Based on those errors, search the runbook for troubleshooting procedures. Max 5 results." \
| python3 "$RECORDER" \
    --turns 3 --mode responses --no-stream \
    --model "$MODEL" --vllm "$VLLM_URL" \
    --tools "$TOOLS" --tool-choice auto \
    --output "$OUTPUT_DIR/responses_tool_calls_3turn.yaml"

echo ""
echo "=== 5-turn non-streaming (context retention: 'restart it' resolves to job-382) ==="
printf '%s\n' \
  "You are an SRE assistant. ETL pipeline job-382 failed overnight. What is its current status?" \
  "Pull the error logs for that failed job. Use severity ERROR and max 20 entries." \
  "Search the runbook for how to fix the issue found in those logs. Max 5 results." \
  "Run this analysis code to summarize: import json; print(json.dumps({'job': 'job-382', 'error': 'OOM', 'stage': 'transform', 'recommendation': 'increase memory to 64GB'}))" \
  "Great. Now restart it with 64 GB memory, skip completed stages, and high priority." \
| python3 "$RECORDER" \
    --turns 5 --mode responses --no-stream \
    --model "$MODEL" --vllm "$VLLM_URL" \
    --tools "$TOOLS" --tool-choice auto \
    --output "$OUTPUT_DIR/responses_tool_calls_5turn.yaml"

echo ""
echo "=== 3-turn streaming (context retention in SSE mode: 'that job' resolves) ==="
printf '%s\n' \
  "You are an SRE assistant. Check the status of pipeline job-382." \
  "Get the error logs for that job with severity FATAL and max 5 entries." \
  "Search the web for how to fix that type of error in Spark pipelines." \
| python3 "$RECORDER" \
    --turns 3 --mode responses --stream \
    --model "$MODEL" --vllm "$VLLM_URL" \
    --tools "$TOOLS" --tool-choice auto \
    --output "$OUTPUT_DIR/responses_tool_calls_3turn_streaming.yaml"

echo ""
echo "=== 3-turn branch (turn 3 diverges from turn 1, skipping turn 2's context) ==="
printf '%s\n' \
  "You are an SRE assistant. Check the current status of ETL pipeline job-382." \
  "Get the error logs for that job with severity ERROR and max 10 entries." \
  "Instead of investigating errors, search the runbook for how to increase memory limits for ETL jobs. Max 3 results." \
| python3 "$RECORDER" \
    --turns 3 --mode responses --no-stream \
    --model "$MODEL" --vllm "$VLLM_URL" \
    --tools "$TOOLS" --tool-choice auto \
    --branch-from 1 --branch-turn-number 3 \
    --output "$OUTPUT_DIR/responses_tool_calls_branch.yaml"

echo ""
echo "=== All cassettes recorded ==="
ls -la "$OUTPUT_DIR"/*.yaml
