import manifest from './data/docs-versions.json';
import { REPO } from './site';

export type DocsVersion = Omit<
  (typeof manifest.versions)[number],
  'hostedBaseUrl'
> & { hostedBaseUrl: string | null };
export const docsVersions: DocsVersion[] = manifest.versions;
export const defaultDocsVersion = manifest.defaultVersion;

const sections = [
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
];

export function findDocsVersion(id: string) {
  return docsVersions.find((version) => version.id === id);
}

export function docsHome(version: DocsVersion) {
  return version.hostedBaseUrl || `${REPO}/tree/${version.sourceRef}/docs`;
}

export function docsSections(version: DocsVersion) {
  return sections
    .filter((section) => version.sections.includes(section.id))
    .map((section) => ({
      ...section,
      href: version.hostedBaseUrl
        ? `${version.hostedBaseUrl.replace(/\/$/, '')}/${section.path.replace(/(^|\/)index\.md$/, '$1').replace(/\.md$/, '/')}`
        : `${REPO}/blob/${version.sourceRef}/docs/${section.path}`,
    }));
}
