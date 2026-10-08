import manifest from './data/docs-versions.json';
import siteDocs from './data/site-docs.json';
import { REPO, assetPath } from './site';

export type DocsVersion = Omit<
  (typeof manifest.versions)[number],
  'hostedBaseUrl'
> & { hostedBaseUrl: string | null };
export const docsVersions: DocsVersion[] = manifest.versions;
export const defaultDocsVersion = manifest.defaultVersion;

type DocsSection = {
  id: string;
  title: string;
  description: string;
  path: string;
  slug?: string;
  group?: string;
};

const legacySections: DocsSection[] = [
  {
    id: 'overview',
    title: 'Start here',
    description: 'An introduction to the project and its documentation.',
    path: 'index.md',
  },
  {
    id: 'api',
    title: 'API reference',
    description: 'Explore the API surface for your selected version.',
    path: 'api/index.md',
  },
  {
    id: 'codex-desktop',
    title: 'Codex Desktop',
    description:
      'Use the desktop UI with local models: tested Linux setup and tool limitations.',
    path: 'guides/codex-desktop.md',
  },
  {
    id: 'developing',
    title: 'Developing',
    description: 'Set up a development environment and work on the project.',
    path: 'developing/getting-started.md',
  },
  {
    id: 'community',
    title: 'Contributing',
    description: 'Find ways to participate and contribute to the project.',
    path: 'community/index.md',
  },
  {
    id: 'sglang',
    title: 'SGLang upstream',
    description: 'Connect SGLang and review the tested configuration.',
    path: 'guides/sglang-upstream.md',
  },
  {
    id: 'dynamo',
    title: 'NVIDIA Dynamo upstream',
    description: 'Connect a Dynamo deployment with a vLLM worker.',
    path: 'guides/dynamo-upstream.md',
  },
  {
    id: 'llmd',
    title: 'agentic-llm-d',
    description:
      'Explore the v1alpha response hydration and persistence design.',
    path: 'design/agentic-llm-d.md',
  },
  {
    id: 'kubernetes',
    title: 'Kubernetes deployment',
    description: 'Configure the gateway and its dependencies on Kubernetes.',
    path: 'deploying/kubernetes.md',
  },
  {
    id: 'kind',
    title: 'Kubernetes with kind',
    description: 'Run a local Kubernetes deployment with kind.',
    path: 'deploying/README.md',
  },
  {
    id: 'github-oidc',
    title: 'GitHub authentication with Dex',
    description: 'Set up an authenticated gateway deployment with Dex.',
    path: 'deploying/github-oidc.md',
  },
  {
    id: 'releases',
    title: 'Release guide',
    description: 'Prepare, validate, and publish project releases.',
    path: 'developing/releases.md',
  },
];

const curatedDocs: DocsSection[] = siteDocs.documents.map((document) => ({
  id: document.id,
  title: document.title,
  description: document.description,
  path: document.sourcePath,
  slug: document.slug,
  group: document.group,
}));

export function findDocsVersion(id: string) {
  return docsVersions.find((version) => version.id === id);
}

export function docsHome(version: DocsVersion) {
  return version.hostedBaseUrl || `${REPO}/tree/${version.sourceRef}/docs`;
}

function sectionHref(version: DocsVersion, section: DocsSection): string {
  if (version.id === 'latest') {
    if (section.slug) return assetPath(`/docs/latest/${section.slug}`);
    if (section.id === 'community') return assetPath('/community/team');
  }
  if (version.hostedBaseUrl) {
    const path = section.path
      .replace(/(^|\/)index\.md$/, '$1')
      .replace(/\.md$/, '/');
    return `${version.hostedBaseUrl.replace(/\/$/, '')}/${path}`;
  }
  return `${REPO}/blob/${version.sourceRef}/docs/${section.path}`;
}

export function docsSections(version: DocsVersion) {
  const available: DocsSection[] =
    version.id === 'latest'
      ? [
          ...curatedDocs,
          ...legacySections.filter((section) =>
            [
              'community',
              'llmd',
              'kubernetes',
              'kind',
              'github-oidc',
              'releases',
            ].includes(section.id),
          ),
        ]
      : legacySections;
  return available
    .filter((section) => version.sections.includes(section.id))
    .map((section) => ({
      ...section,
      href: sectionHref(version, section),
    }));
}
