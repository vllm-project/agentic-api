'use client';
import { useEffect, useState } from 'react';
import { Tabs, TabsList, TabsTrigger, TabsContent } from '@/components/ui/tabs';
import {
  Check,
  Copy,
  Terminal,
  Monitor,
  Code2,
  ArrowUpRight,
} from 'lucide-react';
import {
  DEFAULT_INSTALL_METHOD,
  INSTALL_METHODS,
  PUBLISHED_VERSION,
  type InstallMethod,
  isInstallMethod,
  getLaunchCommands,
  getServeCommand,
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
  const [installation, setInstallation] = useState<InstallMethod>(
    DEFAULT_INSTALL_METHOD,
  );
  const launchCommands = getLaunchCommands(installation);
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
            description: `Read installation and launch instructions for Codex or Claude Code with vLLM. Version ${PUBLISHED_VERSION} is available on crates.io and PyPI; defaults to crates.io. Returns commands; does not run commands or change configuration.`,
            inputSchema: {
              type: 'object',
              properties: {
                harness: { type: 'string', enum: ['codex', 'claude'] },
                installation: {
                  type: 'string',
                  enum: ['crates', 'pypi', 'source'],
                  default: DEFAULT_INSTALL_METHOD,
                },
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
                <strong>Get Agentic API</strong>
                <p>
                  Run v{PUBLISHED_VERSION} from PyPI with uvx or install from
                  crates.io, with source builds available for development.
                </p>
              </div>
            </li>
            <li>
              <span>03</span>
              <div>
                <strong>Launch your client</strong>
                <p>
                  Install Codex or Claude Code, then choose your client and
                  served model.
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
          <Tabs
            value={installation}
            onValueChange={(value) => {
              if (isInstallMethod(value)) setInstallation(value);
            }}
            className="launch-tabs"
          >
            <TabsList
              className="launch-tab-list install-tab-list"
              aria-label="Choose how to install Agentic API"
            >
              {Object.entries(INSTALL_METHODS).map(([method, details]) => (
                <TabsTrigger key={method} value={method}>
                  {details.label}
                </TabsTrigger>
              ))}
            </TabsList>
            {Object.entries(INSTALL_METHODS).map(([method, details]) => (
              <TabsContent key={method} value={method}>
                <CopyCode
                  code={details.command}
                  label={
                    method === 'source'
                      ? 'Build from source'
                      : method === 'pypi'
                        ? `Check the PyPI CLI (v${PUBLISHED_VERSION})`
                        : `Install from crates.io (v${PUBLISHED_VERSION})`
                  }
                />
                <p className="install-note">{details.note}</p>
              </TabsContent>
            ))}
          </Tabs>
          <CopyCode
            key={installation + '-serve'}
            code={getServeCommand(installation)}
            label="Start the standalone server"
          />
          <p className="install-note">
            The CLI launchers below start the gateway for you.
          </p>
          <Tabs defaultValue="codex" className="launch-tabs">
            <TabsList
              className="launch-tab-list client-tab-list"
              aria-label="Choose your coding agent"
            >
              <TabsTrigger value="codex">
                <Terminal size={16} /> Codex CLI
              </TabsTrigger>
              <TabsTrigger value="codex-desktop">
                <Monitor size={16} /> Codex Desktop
              </TabsTrigger>
              <TabsTrigger value="claude">
                <Code2 size={16} /> Claude Code
              </TabsTrigger>
            </TabsList>
            <TabsContent value="codex">
              <CopyCode
                key={installation + '-codex'}
                code={launchCommands.codex}
                label="Launch Codex"
              />
            </TabsContent>
            <TabsContent value="codex-desktop" className="desktop-guide">
              <p>
                Use the desktop UI with a local vLLM model. The tested Linux
                setup keeps your normal session separate and configures Agentic
                API as the model provider.
              </p>
              <p>
                Shell commands, file writes, and follow-up turns were verified.
                Freeform <code>apply_patch</code> must be disabled; file editing
                uses shell commands.
              </p>
              <p>
                <a
                  className="text-link"
                  href={`${REPO}/blob/main/docs/guides/codex-desktop.md`}
                >
                  Set up Codex Desktop <ArrowUpRight size={16} />
                </a>
              </p>
            </TabsContent>
            <TabsContent value="claude">
              <CopyCode
                key={installation + '-claude'}
                code={launchCommands.claude}
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
