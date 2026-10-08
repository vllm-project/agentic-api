import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { SourceDocPage } from '@/components/site/source-doc-page';
import { findSourceDoc, readSourceDoc, sourceDocs } from '@/lib/source-docs';

export const dynamicParams = false;

export function generateStaticParams() {
  return sourceDocs.map((document) => ({ slug: document.slug.split('/') }));
}

type Props = { params: Promise<{ slug: string[] }> };

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const document = findSourceDoc((await params).slug.join('/'));
  if (!document) notFound();
  return {
    title: document.title,
    description: document.description,
    alternates: { canonical: `/docs/latest/${document.slug}` },
  };
}

export default async function SourceDocumentation({ params }: Props) {
  const document = findSourceDoc((await params).slug.join('/'));
  if (!document) notFound();
  return (
    <SourceDocPage document={document} markdown={readSourceDoc(document)} />
  );
}
