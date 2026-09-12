'use client';
import { useEffect, useState } from 'react';
import { Tabs, TabsList, TabsTrigger, TabsContent } from '@/components/ui/tabs';
import { Check, Copy, Terminal, Code2, ArrowUpRight } from 'lucide-react';
import {
  CARGO_COMMAND,
  PYTHON_COMMAND,
  LAUNCH_COMMANDS,
  getLaunchInstructions,
} from '@/lib/quickstart';
import { REPO } from '@/lib/site';
type ModelContext = {
  registerTool: (
    tool: {
      name: string;
      title: string;
      description: string;
      inputSchema: object;
      annotations: object;
      execute: (input: unknown) => unknown;
    },
    options: { signal: AbortSignal },
  ) => void | Promise<void>;
};
function CopyCode({ code, label }: { code: string; label: string }) {
  const [message, setMessage] = useState('');
  useEffect(() => {
    if (!message) return;
    const timer = setTimeout(() => setMessage(''), 2400);
    return () => clearTimeout(timer);
  }, [message]);
  async function copy() {
    try {
      await navigator.clipboard.writeText(code);
      setMessage('Copied');
    } catch {
      setMessage('Select code to copy');
    }
  }
  return (
    <div className="code-snippet">
      <div className="code-label">
        <span>{label}</span>
        <button
          onClick={copy}
          aria-label={`Copy ${label}`}
          title={`Copy ${label}`}
        >
          {message === 'Copied' ? <Check size={15} /> : <Copy size={15} />}
          <span aria-live="polite">{message || 'Copy'}</span>
        </button>
      </div>
      <pre>
        <code>{code}</code>
      </pre>
    </div>
  );
}
export function Quickstart() {
  useEffect(() => {
    const context = (document as Document & { modelContext?: ModelContext })
      .modelContext;
    if (!context?.registerTool) return;
    const controller = new AbortController();
    try {
      void Promise.resolve(
        context.registerTool(
          {
            name: 'get_launch_instructions',
            title: 'Get agent launch instructions',
            description:
              'Read the build and launch instructions displayed on this page for Codex or Claude Code with vLLM. Returns commands; does not run commands or change configuration.',
            inputSchema: {
              type: 'object',
              properties: {
                harness: { type: 'string', enum: ['codex', 'claude'] },
              },
              required: ['harness'],
              additionalProperties: false,
            },
            annotations: { readOnlyHint: true, untrustedContentHint: false },
            execute: getLaunchInstructions,
          },
          { signal: controller.signal },
        ),
      ).catch(() => {});
    } catch {
      /* Progressive enhancement: instructions remain readable without WebMCP. */
    }
    return () => controller.abort();
  }, []);
  return (
    <section className="quickstart-section" id="get-started">
      <div className="container quickstart-grid">
        <div>
          <span className="eyebrow">FROM MODEL TO AGENT</span>
          <h2>
            Same workflow.
            <br />
            Your infrastructure.
          </h2>
          <p className="section-copy">
            Point your coding agent at Agentic API. Keep your familiar workflow,
            with an open model behind it.
          </p>
          <ol className="setup-steps">
            <li>
              <span>01</span>
              <div>
                <strong>Serve your model</strong>
                <p>
                  Run a tool-capable model with vLLM. The example uses an
                  upstream on port 5050.
                </p>
              </div>
            </li>
            <li>
              <span>02</span>
              <div>
                <strong>Install and start Agentic API</strong>
                <p>
                  Install the Python wheel with pip, or install the Rust CLI
                  with Cargo. Start the gateway against your vLLM upstream.
                </p>
              </div>
            </li>
            <li>
              <span>03</span>
              <div>
                <strong>Launch your client</strong>
                <p>
                  For the Cargo CLI, choose Codex or Claude Code below. The
                  client launcher starts its own gateway; use it instead of the
                  standalone serve command.
                </p>
              </div>
            </li>
          </ol>
          <a className="text-link" href={`${REPO}#agentic-api-cli`}>
            Read the full setup guide <ArrowUpRight size={16} />
          </a>
        </div>
        <div className="terminal-card">
          <div className="terminal-top">
            <div aria-hidden="true">
              <i />
              <i />
              <i />
            </div>
            <span>QUICKSTART</span>
            <span>bash</span>
          </div>
          <Tabs defaultValue="python" className="launch-tabs">
            <TabsList
              className="launch-tab-list"
              aria-label="Choose installation method"
            >
              <TabsTrigger value="python">Python / pip</TabsTrigger>
              <TabsTrigger value="cargo">Cargo</TabsTrigger>
            </TabsList>
            <TabsContent value="python">
              <CopyCode
                code={PYTHON_COMMAND}
                label="Install and start with Python"
              />
              <div className="terminal-note">
                <p>
                  Python 3.10+. Download your platform wheel from the{' '}
                  <a href={`${REPO}/actions/workflows/release-python.yml`}>
                    release workflow
                  </a>
                  . PyPI publication is pending; once published, install with{' '}
                  <code>python -m pip install agentic-api</code>.
                </p>
              </div>
            </TabsContent>
            <TabsContent value="cargo">
              <CopyCode
                code={CARGO_COMMAND}
                label="Install and start with Cargo"
              />
            </TabsContent>
          </Tabs>
          <p className="terminal-note">
            Coding clients with the Cargo-installed CLI
          </p>
          <Tabs defaultValue="codex" className="launch-tabs">
            <TabsList
              className="launch-tab-list"
              aria-label="Choose your coding agent"
            >
              <TabsTrigger value="codex">
                <Terminal size={16} /> Codex
              </TabsTrigger>
              <TabsTrigger value="claude">
                <Code2 size={16} /> Claude Code
              </TabsTrigger>
            </TabsList>
            <TabsContent value="codex">
              <CopyCode code={LAUNCH_COMMANDS.codex} label="Launch Codex" />
            </TabsContent>
            <TabsContent value="claude">
              <CopyCode
                code={LAUNCH_COMMANDS.claude}
                label="Launch Claude Code"
              />
            </TabsContent>
          </Tabs>
          <div className="terminal-note">
            <span />
            Use the model ID served by your vLLM instance.
          </div>
        </div>
      </div>
    </section>
  );
}
