import { ArrowUpRight } from 'lucide-react';
import { REPO, SLACK, SLACK_CHANNEL } from '@/lib/site';
export function CommunityCTA() {
  return (
    <section className="community-cta">
      <div>
        <span className="eyebrow">THERE ARE MANY WAYS TO HELP</span>
        <h2>Help shape what comes next.</h2>
        <p>
          Improve the docs, report a bug, review a change, or build the next
          capability.
        </p>
        <p className="slack-note">
          Join us in <a href={SLACK}>{SLACK_CHANNEL} on the vLLM Slack</a>.
        </p>
      </div>
      <a className="button primary" href={`${REPO}/blob/main/CONTRIBUTING.md`}>
        Start contributing <ArrowUpRight size={17} />
      </a>
    </section>
  );
}
