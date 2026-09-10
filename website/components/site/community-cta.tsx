import { ArrowUpRight } from 'lucide-react';
import { REPO } from '@/lib/site';
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
      </div>
      <a className="button primary" href={`${REPO}/blob/main/CONTRIBUTING.md`}>
        Start contributing <ArrowUpRight size={17} />
      </a>
    </section>
  );
}
