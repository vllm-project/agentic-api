import Link from 'next/link';
import { ArrowLeft, ArrowUpRight, FileText, GitBranch } from 'lucide-react';
import { LinkedMarkdown } from './linked-markdown';
import { docsVersions } from '@/lib/docs';
import { type SourceDoc, sourceDocUrl } from '@/lib/source-docs';
import { REPO, assetPath } from '@/lib/site';

export function SourceDocPage({
  document,
  markdown,
}: {
  document: SourceDoc;
  markdown: string;
}) {
  const markdownPath = assetPath(`/docs/latest/${document.slug}.md`);
  const sourcePath = document.sourcePath.startsWith('docs/')
    ? document.sourcePath
    : `docs/${document.sourcePath}`;
  const latestRelease = docsVersions.find(
    (version) => version.channel === 'release',
  );

  return (
    <main id="main" className="container roadmap-page source-doc-page">
      <link rel="alternate" type="text/markdown" href={markdownPath} />
      <header className="community-hero">
        <span className="eyebrow">BUILD / {document.group.toUpperCase()}</span>
        <h1>{document.title}</h1>
        <p>{document.description}</p>
      </header>
      <aside className="docs-version-notice">
        <strong>Development documentation</strong>
        <p>
          This page tracks the main branch and may describe features beyond your
          installed release.
          {latestRelease && (
            <>
              {' '}
              <Link href={`/docs/${latestRelease.id}`}>
                Browse {latestRelease.label} documentation
              </Link>{' '}
              for the latest tagged source snapshot.
            </>
          )}
        </p>
      </aside>
      <div className="roadmap-source">
        <span>
          <GitBranch size={16} aria-hidden="true" /> Source documentation
        </span>
        <div>
          <a href={markdownPath}>
            <FileText size={16} aria-hidden="true" /> Read as Markdown
          </a>
          <a href={`${REPO}/blob/main/${sourcePath}`}>
            View source <ArrowUpRight size={16} aria-hidden="true" />
          </a>
        </div>
      </div>
      <article className="roadmap-body" aria-label={document.title}>
        <LinkedMarkdown gfm urlTransform={(url) => sourceDocUrl(document, url)}>
          {markdown}
        </LinkedMarkdown>
      </article>
      <div className="docs-all-link">
        <Link href="/docs/latest">
          <ArrowLeft size={16} aria-hidden="true" /> All development
          documentation
        </Link>
      </div>
    </main>
  );
}
