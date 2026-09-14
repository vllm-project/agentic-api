import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { DocsPage } from '@/components/site/docs-page';
import { docsVersions, findDocsVersion } from '@/lib/docs';

export const dynamicParams = false;

export function generateStaticParams() {
  return docsVersions.map(({ id }) => ({ version: id }));
}

type Props = { params: Promise<{ version: string }> };

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const version = findDocsVersion((await params).version);
  if (!version) notFound();
  return {
    title: `${version.label} documentation`,
    description: `Documentation and guides for vLLM Agentic API ${version.label}.`,
    alternates: { canonical: `/docs/${version.id}` },
  };
}

export default async function VersionedDocumentation({ params }: Props) {
  const version = findDocsVersion((await params).version);
  if (!version) notFound();
  return <DocsPage version={version} />;
}
