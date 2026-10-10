import { refreshCliDocs } from './refresh-cli-docs.mjs';
import { refreshRoadmap } from './refresh-roadmap.mjs';
import { refreshSiteDocs } from './refresh-site-docs.mjs';

await refreshRoadmap();
await refreshCliDocs();
await refreshSiteDocs();
console.log(
  'Refreshed the roadmap, site docs, Python and Rust CLI references, and Markdown exports.',
);
