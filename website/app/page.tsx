import Image from 'next/image';
import { ArrowRight, ArrowUpRight, GitBranch } from 'lucide-react';
import { Architecture } from '@/components/site/architecture';
import Link from 'next/link';
import { Features } from '@/components/site/features';
import { Quickstart } from '@/components/site/quickstart';
import { Ecosystem } from '@/components/site/ecosystem';
import community from '@/lib/data/community.json';
import { REPO, assetPath } from '@/lib/site';
export default function Home() {
  return (
    <main id="main">
      <section className="hero container">
        <div className="hero-copy">
          <a className="eyebrow hero-eyebrow" href={REPO}>
            <span className="status-dot" /> OPEN SOURCE. OPEN POSSIBILITIES.{' '}
            <ArrowUpRight size={13} />
          </a>
          <h1>
            Your agent harness.
            <br />
            Now on <span>vLLM.</span>
          </h1>
          <p className="hero-description">
            vLLM Agentic API is the agentic server that lets you run{' '}
            <strong>Codex</strong> and <strong>Claude Code</strong> on top of
            vLLM.
          </p>
          <p className="hero-detail">
            Bring the tools you love to the open models you choose. We handle
            the state, tools, and execution in between.
          </p>
          <div className="hero-actions">
            <a className="button primary" href="#get-started">
              Get started <ArrowRight size={17} />
            </a>
            <a className="button secondary" href={REPO}>
              <GitBranch size={17} /> View on GitHub <ArrowUpRight size={14} />
            </a>
          </div>
          <div className="hero-meta">
            <span>APACHE 2.0</span>
            <span>BUILT IN RUST</span>
            <span>POWERED BY vLLM</span>
          </div>
        </div>
        <Architecture />
      </section>
      <div className="capability-strip">
        <div className="container">
          <span>One application layer.</span>
          <span>Conversations</span>
          <b>+</b>
          <span>Built-in tools</span>
          <b>+</b>
          <span>Multi-turn execution</span>
          <b>+</b>
          <span>WebSockets</span>
        </div>
      </div>
      <section id="capabilities" className="container section">
        <span className="eyebrow">BUILT FOR THE AGENT LOOP</span>
        <div className="section-heading">
          <h2>
            Inference is the start.
            <br />
            Keep the agent going.
          </h2>
          <p>
            vLLM serves the model. Agentic API carries the conversation forward,
            executes built-in tools on the gateway, and connects every turn.
          </p>
        </div>
        <Features />
      </section>
      <Ecosystem />
      <Quickstart />
      <section className="container community-callout">
        <div className="community-intro">
          <span className="eyebrow">BUILT IN THE OPEN</span>
          <h2>
            Better agents.
            <br />
            Built together.
          </h2>
          <p>
            Meet the people building the application layer for open-model
            agents. There’s room for your ideas, your fixes, and your next
            contribution.
          </p>
          <div className="hero-actions">
            <Link className="button secondary" href="/community/team">
              Meet the team <ArrowRight size={16} />
            </Link>
            <Link className="text-link" href="/community/contributors">
              See contributors <ArrowUpRight size={16} />
            </Link>
          </div>
        </div>
        <div className="community-visual">
          <div className="avatar-mosaic">
            {community.contributors.map((p) => (
              <a href={p.url} key={p.login} aria-label={`${p.name} on GitHub`}>
                <Image
                  unoptimized
                  src={assetPath(p.avatar)}
                  width={64}
                  height={64}
                  loading="lazy"
                  alt=""
                />
              </a>
            ))}
          </div>
          <p>People helping make Agentic API better.</p>
          <a className="text-link" href={`${REPO}/blob/main/CONTRIBUTING.md`}>
            Make your first contribution <ArrowUpRight size={15} />
          </a>
        </div>
      </section>
    </main>
  );
}
