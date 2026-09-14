import Link from 'next/link';
import { ArrowRight } from 'lucide-react';
export default function NotFound() {
  return (
    <main id="main" className="container not-found">
      <span className="eyebrow">404 / ROUTE NOT FOUND</span>
      <h1>A small detour.</h1>
      <p>This page doesn’t exist. Let’s get you back to the project.</p>
      <Link href="/" className="button primary">
        Back to home <ArrowRight size={17} />
      </Link>
    </main>
  );
}
