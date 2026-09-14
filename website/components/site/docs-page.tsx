import Link from 'next/link';
import { ArrowUpRight, BookOpen, GitBranch } from 'lucide-react';
import {
  defaultDocsVersion,
  docsHome,
  docsSections,
  docsVersions,
  type DocsVersion,
} from '@/lib/docs';
import { DocsVersionSelect } from './docs-version-select';

export function DocsPage({ version }: { version: DocsVersion }) {
  const development = version.channel === 'development';
  const current = version.id === defaultDocsVersion;
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
            : current
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
          {!development && !current && (
            <>
              {' '}
              Looking for the latest tagged version?{' '}
              <Link href={`/docs/${defaultDocsVersion}`}>
                Open {defaultDocsVersion}.
              </Link>
            </>
          )}
          {!version.hostedBaseUrl && ' Guides currently open on GitHub.'}
        </p>
      </aside>
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
          <BookOpen size={18} /> Browse all {version.label} documentation{' '}
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
