import Link from 'next/link';
import { ArrowUpRight, BookOpen, GitBranch } from 'lucide-react';
import {
  docsHome,
  docsSections,
  docsVersions,
  type DocsVersion,
} from '@/lib/docs';
import { PUBLISHED_VERSION } from '@/lib/quickstart';
import { DocsVersionSelect } from './docs-version-select';

export function DocsPage({ version }: { version: DocsVersion }) {
  const development = version.channel === 'development';
  const latestTagged = docsVersions.find((item) => item.channel === 'release')!;
  const currentTag = version.id === latestTagged.id;
  return (
    <main id="main" className="container docs-page">
      <header className="community-hero">
        <span className="eyebrow">BUILD / DOCUMENTATION</span>
        <h1>
          Your version.
          <br />
          <span>Your docs.</span>
        </h1>
        <p>
          Start building with vLLM Agentic API. Choose the documentation that
          matches the version you run.
        </p>
      </header>
      <section className="docs-version-bar" aria-label="Version selection">
        <DocsVersionSelect versionId={version.id} />
        <span className="docs-channel">
          <GitBranch size={14} />
          {development
            ? 'MAIN BRANCH'
            : currentTag
              ? 'LATEST TAGGED VERSION'
              : 'EARLIER VERSION'}
        </span>
      </section>
      <aside className="docs-version-notice">
        <strong>
          {development
            ? 'Development documentation'
            : `${version.label} documentation`}
        </strong>
        <p>
          {development
            ? 'Tracks main and may describe features that are not in a tagged release.'
            : 'A documentation snapshot from this version’s source code.'}
          {!development && !currentTag && (
            <>
              {' '}
              Looking for the latest tagged version?{' '}
              <Link href={`/docs/${latestTagged.id}`}>
                Open {latestTagged.id}.
              </Link>
            </>
          )}
          {development
            ? ' Current guides are available on this site; design notes and remaining pages open on GitHub.'
            : ' Release guides open at a version-pinned source snapshot on GitHub.'}
        </p>
      </aside>
      {currentTag && (
        <Link
          href="/docs/latest/guides/quickstart"
          className="docs-section-link"
        >
          <BookOpen size={22} aria-hidden="true" />
          <div>
            <h2>Quickstart with the published {PUBLISHED_VERSION} release</h2>
            <p>
              Install the gateway, connect a running vLLM endpoint, and send a
              first Responses API request.
            </p>
          </div>
          <ArrowUpRight size={20} aria-hidden="true" />
        </Link>
      )}
      <Link href="/docs/latest/rust-cli" className="docs-section-link">
        <BookOpen size={22} aria-hidden="true" />
        <div>
          <h2>Rust command-line reference</h2>
          <p>
            Latest / development · The <code>agentic</code> CLI: run harnesses,
            serve, and validate.
          </p>
        </div>
        <ArrowUpRight size={20} aria-hidden="true" />
      </Link>
      <Link href="/docs/latest/python-cli" className="docs-section-link">
        <BookOpen size={22} aria-hidden="true" />
        <div>
          <h2>Python command-line reference</h2>
          <p>
            Latest / development · The <code>agentic-api</code> Python launcher:
            commands, options, and defaults.
          </p>
        </div>
        <ArrowUpRight size={20} aria-hidden="true" />
      </Link>
      <section
        className="docs-section-list"
        aria-label={`${version.label} guides`}
      >
        {docsSections(version).map((section, index) => (
          <a href={section.href} className="docs-section-link" key={section.id}>
            <span className="docs-section-number">
              {String(index + 1).padStart(2, '0')}
            </span>
            <div>
              <h2>{section.title}</h2>
              <p>{section.description}</p>
            </div>
            <ArrowUpRight size={20} />
          </a>
        ))}
      </section>
      <div className="docs-all-link">
        <a href={docsHome(version)}>
          <BookOpen size={18} /> Browse all {version.label} source documentation{' '}
          <ArrowUpRight size={16} />
        </a>
      </div>
      <nav className="docs-archive" aria-label="All documentation versions">
        <span>All versions</span>
        <div>
          {docsVersions.map((item) => (
            <Link
              key={item.id}
              href={`/docs/${item.id}`}
              aria-current={item.id === version.id ? 'page' : undefined}
            >
              {item.label}
            </Link>
          ))}
        </div>
      </nav>
    </main>
  );
}
