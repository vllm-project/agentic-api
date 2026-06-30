#!/bin/bash
# Records stateful multi-turn tool-call cassettes using record_cassette.py
#
# Scenario: SRE debugging failed ETL pipeline job-382
# Tools: get_job_status, get_error_logs, search_runbook, run_analysis,
#        restart_job, web_search
#
# IMPORTANT: These cassettes prove context retention via ambiguous prompts.
# Turns 2+ use pronouns ("that job", "it", "those errors") that can ONLY resolve
# correctly if previous_response_id preserves server-side conversation state.
#
# Prerequisites (vLLM):
#   - SSH tunnel to G6e instance: ssh -L 8100:localhost:8100 ubuntu@<G6e-IP>
#   - gpt-oss container running with VLLM_ENABLE_RESPONSES_API_STORE=1
#
# Prerequisites (OpenAI):
#   - OPENAI_API_KEY env var set (or ~/.openai_api_key file)
#
# Usage:
#   ./record.sh              # Record all (vLLM + OpenAI)
#   ./record.sh vllm         # Record vLLM only
#   ./record.sh openai       # Record OpenAI only

set -euo pipefail

RECORDER="$(dirname "$0")/../../record_cassette.py"
TOOLS="$(dirname "$0")/pipeline_tools.json"
TOOL_OUTPUTS="$(dirname "$0")/pipeline_tool_outputs.json"
OUTPUT_DIR="$(dirname "$0")"
VLLM_URL="http://localhost:8100"
VLLM_MODEL="openai/gpt-oss-20b"
OPENAI_MODEL="gpt-4o"

TARGET="${1:-all}"

# ═══════════════════════════════════════════════════════════════════
# vLLM cassettes (gpt-oss-20b)
# ═══════════════════════════════════════════════════════════════════

if [[ "$TARGET" == "all" || "$TARGET" == "vllm" ]]; then

echo "══════════════════════════════════════════════════════════════"
echo "  Recording vLLM cassettes (gpt-oss-20b)"
echo "══════════════════════════════════════════════════════════════"

echo ""
echo "=== 3-turn non-streaming (context retention: 'that job' resolves to job-382) ==="
printf '%s\n' \
  "You are an SRE assistant. Check the current status of ETL pipeline job-382." \
  "Now pull the error logs for that job. Use severity ERROR and max 10 entries." \
  "Based on those errors, search the runbook for troubleshooting procedures. Max 5 results." \
| python3 "$RECORDER" \
    --turns 3 --mode responses --no-stream \
    --model "$VLLM_MODEL" --vllm "$VLLM_URL" \
    --tools "$TOOLS" --tool-choice auto \
    --tool-outputs "$TOOL_OUTPUTS" \
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
    --model "$VLLM_MODEL" --vllm "$VLLM_URL" \
    --tools "$TOOLS" --tool-choice auto \
    --tool-outputs "$TOOL_OUTPUTS" \
    --output "$OUTPUT_DIR/responses_tool_calls_5turn.yaml"

echo ""
echo "=== 3-turn streaming (context retention in SSE mode: 'that job' resolves) ==="
printf '%s\n' \
  "You are an SRE assistant. Check the status of pipeline job-382." \
  "Get the error logs for that job with severity FATAL and max 5 entries." \
  "Search the web for how to fix that type of error in Spark pipelines." \
| python3 "$RECORDER" \
    --turns 3 --mode responses --stream \
    --model "$VLLM_MODEL" --vllm "$VLLM_URL" \
    --tools "$TOOLS" --tool-choice auto \
    --tool-outputs "$TOOL_OUTPUTS" \
    --output "$OUTPUT_DIR/responses_tool_calls_3turn_streaming.yaml"

echo ""
echo "=== 3-turn branch (turn 3 diverges from turn 1, skipping turn 2's context) ==="
printf '%s\n' \
  "You are an SRE assistant. Check the current status of ETL pipeline job-382." \
  "Get the error logs for that job with severity ERROR and max 10 entries." \
  "Instead of investigating errors, search the runbook for how to increase memory limits for ETL jobs. Max 3 results." \
| python3 "$RECORDER" \
    --turns 3 --mode responses --no-stream \
    --model "$VLLM_MODEL" --vllm "$VLLM_URL" \
    --tools "$TOOLS" --tool-choice auto \
    --tool-outputs "$TOOL_OUTPUTS" \
    --branch-from 1 --branch-turn-number 3 \
    --output "$OUTPUT_DIR/responses_tool_calls_branch.yaml"

echo ""
echo "=== 3-turn parallel (attempts 2 tools in one turn) ==="
printf '%s\n' \
  "You are an SRE assistant. Do TWO things in parallel: 1) check the status of job-382 AND 2) search the web for Spark OOM fixes. Call BOTH tools now." \
  "Based on those results, search the runbook for memory override procedures. Max 3 results." \
  "Now restart that job with 64GB memory and high priority." \
| python3 "$RECORDER" \
    --turns 3 --mode responses --no-stream \
    --model "$VLLM_MODEL" --vllm "$VLLM_URL" \
    --tools "$TOOLS" --tool-choice auto \
    --tool-outputs "$TOOL_OUTPUTS" \
    --output "$OUTPUT_DIR/responses_tool_calls_parallel.yaml"

echo ""
echo "=== 3-turn tool-output-only (turn 2 has no user message, just tool output) ==="
# Turn 2 prompt is empty string → _build_tool_output_input omits user message
printf '%s\n' \
  "You are an SRE assistant. Check the current status of ETL pipeline job-382." \
  "" \
  "Based on what you found, search the runbook for how to fix it. Max 5 results." \
| python3 "$RECORDER" \
    --turns 3 --mode responses --no-stream \
    --model "$VLLM_MODEL" --vllm "$VLLM_URL" \
    --tools "$TOOLS" --tool-choice auto \
    --tool-outputs "$TOOL_OUTPUTS" \
    --output "$OUTPUT_DIR/responses_tool_calls_tool_output_only.yaml"

echo ""
echo "=== vLLM cassettes done ==="
ls -la "$OUTPUT_DIR"/responses_*.yaml

fi

# ═══════════════════════════════════════════════════════════════════
# OpenAI cassettes (gpt-4o)
# ═══════════════════════════════════════════════════════════════════

if [[ "$TARGET" == "all" || "$TARGET" == "openai" ]]; then

echo ""
echo "══════════════════════════════════════════════════════════════"
echo "  Recording OpenAI cassettes (gpt-4o)"
echo "══════════════════════════════════════════════════════════════"

echo ""
echo "=== 3-turn non-streaming (context retention: 'that job' resolves to job-382) ==="
printf '%s\n' \
  "You are an SRE assistant. Check the current status of ETL pipeline job-382." \
  "Now pull the error logs for that job. Use severity ERROR and max 10 entries." \
  "Based on those errors, search the runbook for troubleshooting procedures. Max 5 results." \
| python3 "$RECORDER" \
    --turns 3 --mode responses --no-stream \
    --model "$OPENAI_MODEL" \
    --tools "$TOOLS" --tool-choice auto \
    --tool-outputs "$TOOL_OUTPUTS" \
    --output "$OUTPUT_DIR/openai_responses_tool_calls_3turn.yaml"

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
    --model "$OPENAI_MODEL" \
    --tools "$TOOLS" --tool-choice auto \
    --tool-outputs "$TOOL_OUTPUTS" \
    --output "$OUTPUT_DIR/openai_responses_tool_calls_5turn.yaml"

echo ""
echo "=== 3-turn streaming (context retention in SSE mode) ==="
printf '%s\n' \
  "You are an SRE assistant. Check the status of pipeline job-382." \
  "Get the error logs for that job with severity FATAL and max 5 entries." \
  "Search the web for how to fix that type of error in Spark pipelines." \
| python3 "$RECORDER" \
    --turns 3 --mode responses --stream \
    --model "$OPENAI_MODEL" \
    --tools "$TOOLS" --tool-choice auto \
    --tool-outputs "$TOOL_OUTPUTS" \
    --output "$OUTPUT_DIR/openai_responses_tool_calls_3turn_streaming.yaml"

echo ""
echo "=== 3-turn branch (turn 3 diverges from turn 1) ==="
printf '%s\n' \
  "You are an SRE assistant. Check the current status of ETL pipeline job-382." \
  "Get the error logs for that job with severity ERROR and max 10 entries." \
  "Instead of investigating errors, search the runbook for how to increase memory limits for ETL jobs. Max 3 results." \
| python3 "$RECORDER" \
    --turns 3 --mode responses --no-stream \
    --model "$OPENAI_MODEL" \
    --tools "$TOOLS" --tool-choice auto \
    --tool-outputs "$TOOL_OUTPUTS" \
    --branch-from 1 --branch-turn-number 3 \
    --output "$OUTPUT_DIR/openai_responses_tool_calls_branch.yaml"

echo ""
echo "=== 3-turn parallel (2 tools in one turn — gpt-4o reliably does this) ==="
printf '%s\n' \
  "You are an SRE assistant. Do TWO things in parallel: 1) check the status of job-382 AND 2) search the web for Spark OOM fixes. Call BOTH tools now." \
  "Based on those results, search the runbook for memory override procedures. Max 3 results." \
  "Now restart that job with 64GB memory and high priority." \
| python3 "$RECORDER" \
    --turns 3 --mode responses --no-stream \
    --model "$OPENAI_MODEL" \
    --tools "$TOOLS" --tool-choice auto \
    --tool-outputs "$TOOL_OUTPUTS" \
    --output "$OUTPUT_DIR/openai_responses_tool_calls_parallel.yaml"

echo ""
echo "=== 3-turn tool-output-only (turn 2 has no user message) ==="
# Turn 2 prompt is empty string → _build_tool_output_input omits user message
printf '%s\n' \
  "You are an SRE assistant. Check the current status of ETL pipeline job-382." \
  "" \
  "Based on what you found, search the runbook for how to fix it. Max 5 results." \
| python3 "$RECORDER" \
    --turns 3 --mode responses --no-stream \
    --model "$OPENAI_MODEL" \
    --tools "$TOOLS" --tool-choice auto \
    --tool-outputs "$TOOL_OUTPUTS" \
    --output "$OUTPUT_DIR/openai_responses_tool_calls_tool_output_only.yaml"

echo ""
echo "=== OpenAI cassettes done ==="
ls -la "$OUTPUT_DIR"/openai_*.yaml

fi

echo ""
echo "══════════════════════════════════════════════════════════════"
CASSETTE_COUNT=$(ls "$OUTPUT_DIR"/*.yaml 2>/dev/null | wc -l | tr -d ' ')
echo "  All done. ${CASSETTE_COUNT} cassettes in ${OUTPUT_DIR}."
echo "══════════════════════════════════════════════════════════════"
