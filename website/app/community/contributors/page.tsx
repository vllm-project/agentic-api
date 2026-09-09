import Image from 'next/image';
import type { Metadata } from 'next';
import { ArrowUpRight, GitCommitHorizontal } from 'lucide-react';
import community from '@/lib/data/community.json';
import { CommunityNav } from '@/components/site/community-nav';
import { CommunityCTA } from '@/components/site/community-cta';
import {
  Table,
  TableHeader,
  TableBody,
  TableHead,
  TableCell,
  TableRow,
  TableCaption,
} from '@/components/ui/table';
import { REPO } from '@/lib/site';
export const metadata: Metadata = {
  title: 'Contributors',
  description:
    'Meet the contributors building vLLM Agentic API. Explore the GitHub contribution snapshot and find your way to contribute.',
  alternates: { canonical: '/community/contributors' },
};
const total = community.contributors.reduce(
  (sum, p) => sum + p.contributions,
  0,
);
export default function Contributors() {
  return (
    <main id="main" className="container community-page">
      <header className="community-hero">
        <span className="eyebrow">COMMUNITY / CONTRIBUTORS</span>
        <h1>
          Every contribution.
          <br />
          <span>A step forward.</span>
        </h1>
        <p>
          The people turning open-model agents into everyday tools. Thank you
          for building with us.
        </p>
      </header>
      <CommunityNav />
      <section
        className="contributor-stats"
        aria-label="Contributor snapshot statistics"
      >
        <div>
          <span>{community.contributors.length}</span>
          <p>GitHub contributors</p>
        </div>
        <div>
          <span>{total}</span>
          <p>Attributed commits</p>
        </div>
        <div className="snapshot-date">
          <span>Sep 09, 2026</span>
          <p>Snapshot date · All-time GitHub data</p>
        </div>
      </section>
      <section className="contributors-section">
        <div className="contributors-heading">
          <div>
            <span className="eyebrow">THE PEOPLE BEHIND THE COMMITS</span>
            <h2>Contributor spotlight</h2>
          </div>
          <a className="text-link" href={`${REPO}/graphs/contributors`}>
            Explore on GitHub <ArrowUpRight size={15} />
          </a>
        </div>
        <div className="spotlight-grid">
          {community.contributors.slice(0, 3).map((p, i) => (
            <a className="spotlight-card" href={p.url} key={p.login}>
              <div className="spotlight-top">
                <span className="rank">/{String(i + 1).padStart(2, '0')}</span>
                <ArrowUpRight size={18} />
              </div>
              <Image unoptimized src={p.avatar} alt="" width={64} height={64} />
              <h3>{p.name}</h3>
              <span className="person-handle">@{p.login}</span>
              <div className="spotlight-bottom">
                <strong>{p.contributions}</strong>
                <span>attributed commits</span>
                <GitCommitHorizontal size={20} />
              </div>
            </a>
          ))}
        </div>
        <div className="all-contributors-heading">
          <h3>
            All contributors <span>{community.contributors.length}</span>
          </h3>
          <span>BY ATTRIBUTED COMMITS</span>
        </div>
        <div className="contributors-table-wrap">
          <Table className="contributors-table">
            <TableCaption>
              All-time, non-bot accounts returned by the GitHub contributors API
              on September 9, 2026. Counts reflect GitHub’s cached commit
              attribution, not reviews, issues, or total project impact.{' '}
              <a href={community.source}>Data source ↗</a>
            </TableCaption>
            <TableHeader>
              <TableRow>
                <TableHead scope="col">#</TableHead>
                <TableHead scope="col">Contributor</TableHead>
                <TableHead scope="col" className="commit-cell">
                  Commits
                </TableHead>
                <TableHead scope="col" className="share-cell">
                  Share of snapshot
                </TableHead>
                <TableHead scope="col">
                  <span className="sr-only">GitHub profile</span>
                </TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {community.contributors.map((p, i) => (
                <TableRow key={p.login}>
                  <TableCell className="rank-cell">
                    {String(i + 1).padStart(2, '0')}
                  </TableCell>
                  <TableCell>
                    <a
                      className="table-person"
                      href={p.url}
                      aria-label={`${p.name} on GitHub`}
                    >
                      <Image
                        unoptimized
                        src={p.avatar}
                        alt=""
                        loading="lazy"
                        width={38}
                        height={38}
                      />
                      <div>
                        <strong>{p.name}</strong>
                        <span>@{p.login}</span>
                      </div>
                    </a>
                  </TableCell>
                  <TableCell className="commit-cell">
                    {p.contributions}
                  </TableCell>
                  <TableCell className="share-cell">
                    <div className="share-track">
                      <span
                        style={{ width: `${(100 * p.contributions) / total}%` }}
                      />
                    </div>
                    <span>{((100 * p.contributions) / total).toFixed(1)}%</span>
                  </TableCell>
                  <TableCell>
                    <a
                      className="profile-arrow"
                      href={p.url}
                      aria-label={`View ${p.name} on GitHub`}
                    >
                      <ArrowUpRight size={17} />
                    </a>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      </section>
      <CommunityCTA />
    </main>
  );
}
