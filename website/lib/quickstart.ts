import { REPO } from './site';
export const BUILD_COMMAND = `git clone https://github.com/vllm-project/agentic-api.git
cd agentic-api
cargo build -p agentic-server --bins`;
export const LAUNCH_COMMANDS = {
  codex:
    './target/debug/agentic run codex \\\n  --upstream http://127.0.0.1:5050 \\\n  --model Qwen/Qwen3-30B-A3B-FP8',
  claude:
    './target/debug/agentic run claude \\\n  --upstream http://127.0.0.1:5050 \\\n  --model Qwen/Qwen3-30B-A3B-FP8',
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
      'Install Rust and the selected client. Serve a tool-capable model with vLLM at http://127.0.0.1:5050. Replace the example model with the one you serve.',
    build: BUILD_COMMAND,
    launch: LAUNCH_COMMANDS[input.harness],
    guide: `${REPO}#agentic-api-cli`,
  };
}
