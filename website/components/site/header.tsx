'use client';
import Link from 'next/link';
import Image from 'next/image';
import { usePathname } from 'next/navigation';
import { ArrowUpRight, GitBranch, Menu } from 'lucide-react';
import {
  Sheet,
  SheetTrigger,
  SheetContent,
  SheetTitle,
  SheetDescription,
} from '@/components/ui/sheet';
import { REPO, DOCS } from '@/lib/site';
import { useState } from 'react';
export function Brand() {
  return (
    <Link href="/" className="brand" aria-label="vLLM Agentic API home">
      <Image
        unoptimized
        className="brand-logo"
        src="/brand/agentic-api-logo.svg"
        alt="Agentic API"
        width={1739}
        height={796}
        priority
      />
    </Link>
  );
}
export function Header() {
  const path = usePathname();
  const [open, setOpen] = useState(false);
  const links = (
    <>
      <Link href="/#capabilities" onClick={() => setOpen(false)}>
        Capabilities
      </Link>
      <Link
        href={DOCS}
        onClick={() => setOpen(false)}
        aria-current={path.startsWith('/docs') ? 'page' : undefined}
      >
        Documentation
      </Link>
      <Link
        href="/community/team"
        onClick={() => setOpen(false)}
        aria-current={path.startsWith('/community') ? 'page' : undefined}
      >
        Community
      </Link>
    </>
  );
  return (
    <header className="site-header">
      <div className="container header-inner">
        <Brand />
        <nav className="desktop-nav" aria-label="Main navigation">
          {links}
        </nav>
        <a className="github-nav" href={REPO}>
          <GitBranch size={17} />
          <span>GitHub</span>
          <ArrowUpRight size={14} />
        </a>
        <Sheet open={open} onOpenChange={setOpen}>
          <SheetTrigger className="mobile-toggle" aria-label="Open navigation">
            <Menu size={23} />
          </SheetTrigger>
          <SheetContent className="mobile-sheet">
            <SheetTitle>vLLM Agentic API</SheetTitle>
            <SheetDescription>Explore the project</SheetDescription>
            <nav aria-label="Mobile navigation">
              {links}
              <Link
                href="/community/contributors"
                onClick={() => setOpen(false)}
              >
                Contributors
              </Link>
              <a href={REPO}>
                GitHub <ArrowUpRight size={16} />
              </a>
            </nav>
          </SheetContent>
        </Sheet>
      </div>
    </header>
  );
}
