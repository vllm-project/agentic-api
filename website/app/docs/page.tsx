import type { Metadata } from 'next';
import { DocsPage } from '@/components/site/docs-page';
import { defaultDocsVersion, findDocsVersion } from '@/lib/docs';

export const metadata: Metadata = {
  title: 'Documentation',
  description:
    'Browse vLLM Agentic API documentation by version, from tagged releases to the latest development guides.',
  alternates: { canonical: '/docs' },
};

export default function Documentation() {
  return <DocsPage version={findDocsVersion(defaultDocsVersion)!} />;
}
