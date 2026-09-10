'use client';
import Link from 'next/link';
import { usePathname } from 'next/navigation';
import { ArrowUpRight, Users, HeartHandshake } from 'lucide-react';
import { REPO } from '@/lib/site';
export function CommunityNav() {
  const path = usePathname();
  return (
    <nav className="community-nav" aria-label="Community navigation">
      <div>
        <Link
          href="/community/team"
          aria-current={
            path.replace(/\/$/, '') === '/community/team' ? 'page' : undefined
          }
        >
          <Users size={16} /> Project team
        </Link>
        <Link
          href="/community/contributors"
          aria-current={
            path.replace(/\/$/, '') === '/community/contributors'
              ? 'page'
              : undefined
          }
        >
          <HeartHandshake size={17} /> Contributors
        </Link>
      </div>
      <a href={`${REPO}/blob/main/CONTRIBUTING.md`}>
        Contributing guide <ArrowUpRight size={14} />
      </a>
    </nav>
  );
}
