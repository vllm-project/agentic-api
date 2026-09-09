import Image from 'next/image';
import {
  Terminal,
  Code2,
  Braces,
  Database,
  Wrench,
  Workflow,
  ArrowDown,
  Cpu,
  Radio,
} from 'lucide-react';
export function Architecture() {
  return (
    <figure
      className="architecture"
      aria-label="Codex, Claude Code, and SDK clients connect to Agentic API over HTTP, SSE, or WebSockets. Agentic API handles state, tool execution, and continuation, then calls open models served by vLLM."
    >
      <div className="diagram-heading">
        <span>THE AGENTIC STACK</span>
        <span>01 — 03</span>
      </div>
      <div className="clients">
        <div>
          <Terminal size={19} />
          <span>Codex</span>
        </div>
        <div>
          <Code2 size={19} />
          <span>Claude Code</span>
        </div>
        <div>
          <Braces size={19} />
          <span>Your app</span>
        </div>
      </div>
      <div className="connector">
        <span />
        <span className="transport">HTTP · SSE · WebSockets</span>
        <ArrowDown size={16} />
      </div>
      <div className="gateway">
        <div className="gateway-heading">
          <Image
            unoptimized
            className="gateway-mark"
            src="/brand/agentic-api-mark.svg"
            alt=""
            width={902}
            height={943}
          />
          <div>
            <strong>Agentic API</strong>
            <span>THE APPLICATION LAYER</span>
          </div>
          <span className="rust-label">RUST</span>
        </div>
        <div className="gateway-functions">
          <span>
            <Database size={16} /> State
          </span>
          <span>
            <Wrench size={16} /> Tools
          </span>
          <span>
            <Workflow size={16} /> Execution
          </span>
        </div>
        <div className="loop-line">
          <Radio size={14} />
          <span>Reason → call tools → continue</span>
          <span className="loop-symbol">↻</span>
        </div>
      </div>
      <div className="connector second">
        <span />
        <span className="transport">MODEL INFERENCE</span>
        <ArrowDown size={16} />
      </div>
      <div className="model-layer">
        <div>
          <Cpu size={23} />
          <strong>vLLM</strong>
          <span>INFERENCE ENGINE</span>
        </div>
        <p>Open models. Your GPUs.</p>
      </div>
      <figcaption>
        <span className="diagram-dot" /> You own every layer.
      </figcaption>
    </figure>
  );
}
