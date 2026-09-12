import { REPO } from './site';
export const BUILD_COMMAND = 'cargo install agentic-server --locked';
export const PYTHON_COMMAND = `python -m venv .venv
source .venv/bin/activate
# Download the wheel for your platform first.
python -m pip install /absolute/path/to/agentic_api-PLATFORM.whl
python -m agentic_api serve --vllm-base-url http://127.0.0.1:5050`;
export const CARGO_COMMAND = `${BUILD_COMMAND}
agentic serve --upstream http://127.0.0.1:5050`;
export const LAUNCH_COMMANDS = {
  codex:
    'agentic run codex \\\n  --upstream http://127.0.0.1:5050 \\\n  --model Qwen/Qwen3-30B-A3B-FP8',
  claude:
    'agentic run claude \\\n  --upstream http://127.0.0.1:5050 \\\n  --model Qwen/Qwen3-30B-A3B-FP8',
};
export function getLaunchInstructions(input: unknown) {
  if (
    !input ||
    typeof input !== 'object' ||
    !('harness' in input) ||
    Object.keys(input).length !== 1 ||
    (input.harness !== 'codex' && input.harness !== 'claude')
  )
    throw new Error('Choose harness "codex" or "claude".');
  return {
    harness: input.harness,
    prerequisites:
      'Install agentic-server with Cargo and install the selected client. Serve a tool-capable model with vLLM at http://127.0.0.1:5050. Replace the example model with the one you serve.',
    build: BUILD_COMMAND,
    launch: LAUNCH_COMMANDS[input.harness],
    guide: `${REPO}#agentic-api-cli`,
  };
}
