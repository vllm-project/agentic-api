import Image from 'next/image';
import type { Metadata } from 'next';
import { ArrowUpRight } from 'lucide-react';
import community from '@/lib/data/community.json';
import { CommunityNav } from '@/components/site/community-nav';
import { CommunityCTA } from '@/components/site/community-cta';
import { REPO, assetPath } from '@/lib/site';

export const metadata: Metadata = {
  title: 'Contributors',
  description:
    'Meet the people contributing to vLLM Agentic API and find ways to help through thoughtful reviews, clear documentation, useful bug reports, and well-tested code.',
  alternates: { canonical: '/community/contributors' },
};

const refreshedDate = new Intl.DateTimeFormat('en-US', {
  month: 'long',
  day: 'numeric',
  year: 'numeric',
  timeZone: 'UTC',
}).format(new Date(`${community.updatedAt}T00:00:00Z`));

const waysToHelp = [
  {
    title: 'Share a useful bug report',
    description:
      'Explain what happened, what you expected, and how to reproduce it. Clear context helps someone find a fix.',
    href: `${REPO}/issues`,
  },
  {
    title: 'Make the docs clearer',
    description:
      'Improve an explanation, test an example, or document something you learned while getting started.',
    href: `${REPO}/tree/main/docs`,
  },
  {
    title: 'Review a change',
    description:
      'Ask thoughtful questions, try a proposed fix, and help catch edge cases before a change ships.',
    href: `${REPO}/pulls`,
  },
  {
    title: 'Improve the code and tests',
    description:
      'Work with maintainers on a focused change. Explain the problem it solves and test the behavior that matters.',
    href: `${REPO}/blob/main/CONTRIBUTING.md`,
  },
];

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
          Clear bug reports, thoughtful reviews, better documentation, and
          well-tested code all help make Agentic API better.
        </p>
      </header>
      <CommunityNav />
      <section className="contribution-ways" aria-labelledby="ways-to-help">
        <div className="contribution-section-heading">
          <div>
            <span className="eyebrow">START WITH A PROBLEM YOU CARE ABOUT</span>
            <h2 id="ways-to-help">Find a way to help.</h2>
          </div>
          <p>
            Bring your experience, share context, and work with the community
            toward a useful solution.
          </p>
        </div>
        <div className="contribution-ways-grid">
          {waysToHelp.map((way) => (
            <article key={way.title}>
              <h3>
                <a href={way.href}>
                  {way.title}
                  <ArrowUpRight size={17} />
                </a>
              </h3>
              <p>{way.description}</p>
            </article>
          ))}
        </div>
      </section>
      <section
        className="contributor-community"
        aria-labelledby="contributor-directory-heading"
      >
        <div className="contribution-section-heading">
          <div>
            <span className="eyebrow">THE PEOPLE WORKING ON AGENTIC API</span>
            <h2 id="contributor-directory-heading">Meet the contributors.</h2>
          </div>
        </div>
        <ul className="contributor-directory">
          {community.contributors.map((person) => (
            <li key={person.login}>
              <a
                className="contributor-profile"
                href={person.url}
                aria-label={`${person.name} on GitHub`}
              >
                <Image
                  unoptimized
                  src={assetPath(person.avatar)}
                  alt=""
                  width={48}
                  height={48}
                  loading="lazy"
                />
                <div>
                  <h3>{person.name}</h3>
                  <span>@{person.login}</span>
                </div>
                <ArrowUpRight size={16} />
              </a>
            </li>
          ))}
        </ul>
        <p className="source-note">
          Names from the project’s GitHub contributor list, checked{' '}
          {refreshedDate}. This list is one part of a wider community that also
          helps through reviews, issue reports, documentation, and support.
        </p>
      </section>
      <CommunityCTA />
    </main>
  );
}
