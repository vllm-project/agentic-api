import Image from 'next/image';
import type { Metadata } from 'next';
import { ArrowUpRight, GitBranch } from 'lucide-react';
import Link from 'next/link';
import community from '@/lib/data/community.json';
import { CommunityNav } from '@/components/site/community-nav';
import { CommunityCTA } from '@/components/site/community-cta';
import { REPO } from '@/lib/site';
export const metadata: Metadata = {
  title: 'Project team',
  description:
    'Meet the maintainers of vLLM Agentic API, the application layer for open-model agents.',
  alternates: { canonical: '/community/team' },
};
export default function Team() {
  return (
    <main id="main" className="container community-page">
      <header className="community-hero">
        <span className="eyebrow">COMMUNITY / PEOPLE</span>
        <h1>
          The people
          <br />
          behind the <span>agent loop.</span>
        </h1>
        <p>
          Open infrastructure takes a community. Meet the maintainers guiding
          vLLM Agentic API forward.
        </p>
      </header>
      <CommunityNav />
      <section className="roster-section">
        <div className="roster-heading">
          <div>
            <span className="eyebrow">01 / PROJECT STEWARDSHIP</span>
            <h2>
              Maintainers{' '}
              <span>
                {community.maintainers.length.toString().padStart(2, '0')}
              </span>
            </h2>
          </div>
          <p>
            The project’s maintainers are listed in{' '}
            <a href={community.ownersSource}>
              CODEOWNERS <ArrowUpRight size={13} />
            </a>
            . They help review changes and keep the project moving.
          </p>
        </div>
        <div className="team-grid">
          {community.maintainers.map((p, i) => (
            <a className="person-card" href={p.url} key={p.login}>
              <div className="person-top">
                <Image
                  unoptimized
                  src={p.avatar}
                  alt=""
                  width={72}
                  height={72}
                />
                <span className="person-number">
                  {String(i + 1).padStart(2, '0')}
                </span>
                <ArrowUpRight size={18} />
              </div>
              <h3>{p.name}</h3>
              <span className="person-handle">@{p.login}</span>
              <div className="person-bottom">
                <span>MAINTAINER</span>
                <GitBranch size={16} />
              </div>
            </a>
          ))}
        </div>
        <p className="source-note">
          Roster and public GitHub profiles checked September 9, 2026.{' '}
          <a href={community.ownersSource}>
            View source <ArrowUpRight size={12} />
          </a>
        </p>
      </section>
      <section className="community-principles">
        <div>
          <span className="eyebrow">02 / WORKING TOGETHER</span>
          <h2>Built with everyone.</h2>
        </div>
        <div>
          <p>
            Maintainers are one part of the project. Every contribution, from an
            issue report to a new integration, helps make open-model agents more
            useful.
          </p>
          <div className="inline-links">
            <Link href="/community/contributors">
              Meet the contributors <ArrowUpRight size={15} />
            </Link>
            <a href={`${REPO}/blob/main/CODE_OF_CONDUCT.md`}>
              Code of conduct <ArrowUpRight size={15} />
            </a>
          </div>
        </div>
      </section>
      <CommunityCTA />
    </main>
  );
}
