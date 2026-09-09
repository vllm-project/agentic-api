import Link from 'next/link';
import { ArrowUpRight } from 'lucide-react';
import { Brand } from './header';
import { REPO, DOCS } from '@/lib/site';
export function Footer() {
  return (
    <footer className="site-footer">
      <div className="container">
        <div className="footer-top">
          <div>
            <Brand />
            <p>
              Open models. Stateful agents.
              <br />
              Built together.
            </p>
          </div>
          <div className="footer-links">
            <div>
              <span>PROJECT</span>
              <Link href={DOCS}>Documentation</Link>
              <a href={REPO}>
                GitHub <ArrowUpRight size={13} />
              </a>
              <a href={`${REPO}/blob/main/ROADMAP.md`}>
                Roadmap <ArrowUpRight size={13} />
              </a>
            </div>
            <div>
              <span>COMMUNITY</span>
              <Link href="/community/team">Team</Link>
              <Link href="/community/contributors">Contributors</Link>
              <a href={`${REPO}/blob/main/CONTRIBUTING.md`}>
                Contributing <ArrowUpRight size={13} />
              </a>
            </div>
          </div>
        </div>
        <div className="footer-bottom">
          <span>© 2026 vLLM Agentic API contributors</span>
          <a href={`${REPO}/blob/main/LICENSE`}>
            Apache 2.0 license <ArrowUpRight size={12} />
          </a>
          <span className="footer-built">
            <i /> Open source, from the ground up
          </span>
        </div>
      </div>
    </footer>
  );
}
