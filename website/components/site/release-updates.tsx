import { ArrowUpRight } from 'lucide-react';
import { REPO } from '@/lib/site';

const RELEASE = 'v0.10.0';

export function ReleaseUpdates() {
  return (
    <section
      className="container release-updates"
      aria-labelledby="release-heading"
    >
      <div>
        <span className="eyebrow">RELEASE UPDATE</span>
        <h2 id="release-heading">New in {RELEASE}</h2>
        <p>
          Search your documents, keep agents moving, and see how each request
          runs.
        </p>
        <a className="text-link" href={`${REPO}/releases/tag/${RELEASE}`}>
          Read the release notes <ArrowUpRight size={16} />
        </a>
      </div>
      <div>
        <ul className="release-highlights">
          <li>
            <strong>File search with citations.</strong> Files and vector-store
            APIs, sqlite-vec semantic search, and pluggable storage contracts.
          </li>
          <li>
            <strong>More ways to continue.</strong> Asynchronous client tools,
            WebSocket multi-agent execution and steering, and opt-in Messages
            threads.
          </li>
          <li>
            <strong>Operational visibility.</strong> OpenTelemetry metrics, MCP
            trace propagation, and per-caller request rate limits.
          </li>
        </ul>
        <p className="release-upgrade">
          <strong>Upgrading from 0.9?</strong> Back up stored history first.
          Incompatible default SQLite history is reset; incompatible custom
          databases need a reset or a new database.{' '}
          <a href={`${REPO}/blob/${RELEASE}/CHANGELOG.md#upgrade-notes`}>
            Read the upgrade notes
          </a>
          .
        </p>
      </div>
    </section>
  );
}
