import { REPO } from './site';

export const INSTALL_METHODS = {
  crates: {
    label: 'crates.io',
    command: 'cargo install agentic-server --locked',
    note: 'Install the released gateway and agentic CLI. Requires Rust and Cargo.',
  },
  pypi: {
    label: 'PyPI',
    command: 'python -m pip install "agentic-api>=0.6.0"',
    note: 'Coming soon. Use this command after the PyPI release, in a Python 3.10+ environment. The wheel includes the gateway and agentic CLI; vLLM is installed separately.',
  },
  source: {
    label: 'Build from source',
    command: [
      'git clone https://github.com/vllm-project/agentic-api.git',
      'cd agentic-api',
      'cargo build -p agentic-server --bins',
    ].join('\n'),
    note: 'Build the development version with Rust and Cargo. Run the launch command from the repository directory.',
  },
};

export type InstallMethod = keyof typeof INSTALL_METHODS;
export const DEFAULT_INSTALL_METHOD: InstallMethod = 'crates';

export function isInstallMethod(value: unknown): value is InstallMethod {
  return value === 'crates' || value === 'pypi' || value === 'source';
}

export function getLaunchCommands(installation: InstallMethod) {
  const executable =
    installation === 'source' ? './target/debug/agentic' : 'agentic';
  const options =
    ' \\\n  --upstream http://127.0.0.1:5050 \\\n  --model Qwen/Qwen3-30B-A3B-FP8';
  return {
    codex: executable + ' run codex' + options,
    claude: executable + ' run claude' + options,
  };
}

export function getLaunchInstructions(input: unknown) {
  if (
    !input ||
    typeof input !== 'object' ||
    !('harness' in input) ||
    Object.keys(input).some(
      (key) => key !== 'harness' && key !== 'installation',
    ) ||
    (input.harness !== 'codex' && input.harness !== 'claude')
  )
    throw new Error('Choose harness "codex" or "claude".');
  const installation =
    'installation' in input ? input.installation : DEFAULT_INSTALL_METHOD;
  if (!isInstallMethod(installation))
    throw new Error('Choose installation "crates", "pypi", or "source".');
  return {
    harness: input.harness,
    installation,
    prerequisites:
      'Install the selected client. Serve a tool-capable model with vLLM at http://127.0.0.1:5050. Replace the example model with the one you serve.',
    note: INSTALL_METHODS[installation].note,
    install: INSTALL_METHODS[installation].command,
    launch: getLaunchCommands(installation)[input.harness],
    guide: REPO + '#agentic-api-cli',
  };
}
