import { ArrowRight, ArrowUpRight, GitBranch, Layers } from 'lucide-react';
import { REPO } from '@/lib/site';

export function Ecosystem() {
  return (
    <section className="container ecosystem-section" id="ecosystem">
      <span className="eyebrow">ACROSS THE OPEN INFERENCE STACK</span>
      <div className="section-heading">
        <h2>Bring your serving stack.</h2>
        <p>
          Built for vLLM, with recorded integration tests for SGLang and NVIDIA
          Dynamo covering streaming, multi-turn state, and client-executed
          function calls.
        </p>
      </div>
      <div className="upstream-grid">
        <a
          className="upstream-card"
          href={`${REPO}/blob/main/docs/guides/sglang-upstream.md`}
        >
          <div className="upstream-heading">
            <h3>SGLang</h3>
            <ArrowUpRight size={21} />
          </div>
          <p>
            Connect Agentic API to SGLang’s Responses endpoint. The gateway
            stores response history and carries it into the next turn.
          </p>
          <div className="upstream-config">
            <span>TESTED CONFIGURATION</span>
            <strong>SGLang 0.5.18 · Qwen3-8B</strong>
          </div>
          <span className="text-link">
            Setup and test coverage <ArrowUpRight size={15} />
          </span>
        </a>
        <a
          className="upstream-card"
          href={`${REPO}/blob/main/docs/guides/dynamo-upstream.md`}
        >
          <div className="upstream-heading">
            <h3>NVIDIA Dynamo</h3>
            <ArrowUpRight size={21} />
          </div>
          <p>
            Point Agentic API at the Dynamo frontend to add conversation state
            and continuation to your distributed inference stack.
          </p>
          <div className="upstream-config">
            <span>TESTED CONFIGURATION</span>
            <strong>Dynamo 1.4.1 · vLLM worker · GPT-OSS 20B</strong>
          </div>
          <span className="text-link">
            Setup and test coverage <ArrowUpRight size={15} />
          </span>
        </a>
      </div>
      <aside className="llmd-callout" aria-labelledby="llmd-title">
        <div className="llmd-copy">
          <span className="eyebrow">
            <Layers size={16} /> llm-d INTEGRATION · v1alpha
          </span>
          <h3 id="llmd-title">State services for llm-d.</h3>
          <p>
            <code>agentic-llm-d</code> exposes separate hydrate and persist
            steps. The llm-d coordinator routes inference; Agentic API restores
            item history and stores the response.
          </p>
          <a
            className="text-link"
            href={`${REPO}/blob/main/docs/design/agentic-llm-d.md`}
          >
            Explore the llm-d integration <ArrowUpRight size={16} />
          </a>
        </div>
        <div className="llmd-flow">
          <div
            className="split-flow"
            aria-label="Hydrate history, route inference through llm-d, then persist the response"
          >
            <span>Hydrate</span>
            <ArrowRight size={16} />
            <span>
              <GitBranch size={17} /> llm-d
            </span>
            <ArrowRight size={16} />
            <span>Persist</span>
          </div>
          <p>
            This path handles response state. Gateway-executed built-in tools
            use the full Agentic API gateway.
          </p>
        </div>
      </aside>
    </section>
  );
}
