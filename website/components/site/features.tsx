import {
  MessagesSquare,
  Wrench,
  Workflow,
  Radio,
  Cpu,
  ArrowUpRight,
} from 'lucide-react';
import { REPO } from '@/lib/site';
export function Features() {
  return (
    <div className="feature-grid">
      <article className="feature-card feature-wide">
        <div className="feature-top">
          <MessagesSquare size={22} />
          <span>01 / CONTEXT</span>
        </div>
        <h3>A conversation that carries forward.</h3>
        <p>
          Keep item history and response state on the server. Continue with{' '}
          <code>previous_response_id</code>, without rebuilding the transcript
          on every call.
        </p>
        <div
          className="state-demo"
          aria-label="Response 01 continues into response 02 and response 03"
        >
          <span>response_01</span>
          <b>→</b>
          <span>response_02</span>
          <b>→</b>
          <span className="highlighted">response_03</span>
        </div>
      </article>
      <article className="feature-card feature-wide">
        <div className="feature-top">
          <Wrench size={22} />
          <span>02 / ACTION</span>
        </div>
        <h3>Tools, with the server in the loop.</h3>
        <p>
          Execute built-in web search and tools from Model Context Protocol
          (MCP) servers on the gateway. Tool call outputs flow back to the model
          so it can take the next step.
        </p>
        <div className="tool-demo">
          <span>
            <span className="small-dot" /> web_search
          </span>
          <span>
            <span className="small-dot" /> MCP tools
          </span>
          <span className="tool-label">GATEWAY-EXECUTED</span>
        </div>
      </article>
      <article className="feature-card">
        <div className="feature-top">
          <Workflow size={22} />
          <span>03 / EXECUTION</span>
        </div>
        <h3>
          One request.
          <br />
          Multiple inference rounds.
        </h3>
        <p>
          Let the gateway coordinate model calls and built-in tools until the
          response is ready or the client needs to act.
        </p>
        <span className="feature-footnote">REASON → ACT → CONTINUE</span>
      </article>
      <article className="feature-card">
        <div className="feature-top">
          <Radio size={22} />
          <span>04 / TRANSPORT</span>
        </div>
        <h3>
          Stay connected.
          <br />
          Keep streaming.
        </h3>
        <p>
          Use HTTP for requests, server-sent events (SSE) for streamed
          responses, and WebSockets for interactive Responses API clients.
        </p>
        <div className="protocol-labels">
          <span>HTTP</span>
          <span>SSE</span>
          <span>WS</span>
        </div>
      </article>
      <article className="feature-card">
        <div className="feature-top">
          <Cpu size={22} />
          <span>05 / INFERENCE</span>
        </div>
        <h3>
          Open models.
          <br />
          Your choice.
        </h3>
        <p>
          Connect to compatible open models served by vLLM. Choose the model and
          tool-calling configuration for your workload.
        </p>
        <a className="feature-footnote" href={`${REPO}#quickstart`}>
          EXPLORE SETUP <ArrowUpRight size={13} />
        </a>
      </article>
      <p className="ownership-note">
        <span>Clear execution boundaries.</span> Codex and Claude Code still
        execute their own shell and editor tools. Agentic API runs the tools
        assigned to the gateway.
      </p>
    </div>
  );
}
