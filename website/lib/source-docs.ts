import { readFileSync } from 'node:fs';
import { posix, resolve, sep } from 'node:path';
import { defaultUrlTransform } from 'react-markdown';
import manifest from './data/site-docs.json';
import { REPO, assetPath } from './site';

export type SourceDoc = (typeof manifest.documents)[number];
export const sourceDocs: SourceDoc[] = manifest.documents;

const bySlug = new Map(sourceDocs.map((document) => [document.slug, document]));
const bySourcePath = new Map(
  sourceDocs.map((document) => [docsPath(document.sourcePath), document]),
);

function docsPath(sourcePath: string): string {
  return posix.normalize(`docs/${sourcePath.replace(/^docs\//, '')}`);
}

export function findSourceDoc(slug: string): SourceDoc | undefined {
  return bySlug.get(slug);
}

export function readSourceDoc(document: SourceDoc): string {
  const contentRoot = resolve(process.cwd(), 'content/docs');
  const sourceFile = resolve(contentRoot, document.sourcePath);
  if (!sourceFile.startsWith(`${contentRoot}${sep}`)) {
    throw new Error(
      `Invalid documentation source path: ${document.sourcePath}`,
    );
  }

  const markdown = readFileSync(sourceFile, 'utf8').replace(/^\uFEFF/, '');
  const title = /^# [^\r\n]+\r?\n/.exec(markdown);
  if (!title) {
    throw new Error(`Expected a first-level heading in ${document.sourcePath}`);
  }
  return markdown.slice(title[0].length).replace(/^\r?\n/, '');
}

/** Resolve a link relative to its source Markdown file, keeping published routes under the site base path. */
export function sourceDocUrl(document: SourceDoc, url: string): string {
  const safe = defaultUrlTransform(url);
  if (!safe || safe.startsWith('#') || safe.startsWith('//')) return safe;
  if (/^[a-z][a-z\d+.-]*:/i.test(safe)) return safe;
  if (safe.startsWith('/')) {
    const basePath = process.env.NEXT_PUBLIC_BASE_PATH || '';
    return basePath && (safe === basePath || safe.startsWith(`${basePath}/`))
      ? safe
      : assetPath(safe);
  }

  const suffixAt = safe.search(/[?#]/);
  const linkPath = suffixAt < 0 ? safe : safe.slice(0, suffixAt);
  const suffix = suffixAt < 0 ? '' : safe.slice(suffixAt);
  if (!linkPath) return safe;

  const sourceDirectory = posix.dirname(docsPath(document.sourcePath));
  const targetPath = posix.normalize(posix.join(sourceDirectory, linkPath));
  if (targetPath === '..' || targetPath.startsWith('../')) return safe;

  if (targetPath === 'docs/reference/rust-cli.md') {
    return `${assetPath('/docs/latest/rust-cli')}${suffix}`;
  }
  if (targetPath === 'docs/reference/python-cli.md') {
    return `${assetPath('/docs/latest/python-cli')}${suffix}`;
  }

  const migrated = bySourcePath.get(targetPath);
  if (migrated) {
    return `${assetPath(`/docs/latest/${migrated.slug}`)}${suffix}`;
  }

  if (/\.(?:avif|gif|jpe?g|png|svg|webp)$/i.test(targetPath)) {
    return `https://raw.githubusercontent.com/vllm-project/agentic-api/main/${targetPath}${suffix}`;
  }
  return `${REPO}/blob/main/${targetPath}${suffix}`;
}
