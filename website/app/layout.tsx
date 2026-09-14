import type { Metadata } from 'next';
import { Geist, Geist_Mono } from 'next/font/google';
import { Header } from '@/components/site/header';
import { Footer } from '@/components/site/footer';
import { SpotlightEffects } from '@/components/site/spotlight-effects';
import { SITE_URL, assetPath } from '@/lib/site';
import { themeInitScript } from '@/lib/theme.mjs';
import './globals.css';
import './effects.css';
const sans = Geist({ variable: '--font-geist-sans', subsets: ['latin'] });
const mono = Geist_Mono({ variable: '--font-geist-mono', subsets: ['latin'] });
export const metadata: Metadata = {
  metadataBase: new URL(SITE_URL),
  title: {
    default: 'vLLM Agentic API — Your agent harness. Now on vLLM.',
    template: '%s | vLLM Agentic API',
  },
  description:
    'Everything you need for agentic inference: run Codex and Claude Code on vLLM with conversation state, built-in tools, and multi-turn execution.',
  openGraph: {
    title: 'vLLM Agentic API — Your agent harness. Now on vLLM.',
    description:
      'Everything you need for agentic inference: run Codex and Claude Code on vLLM with conversation state, built-in tools, and multi-turn execution.',
    type: 'website',
  },
  icons: { icon: assetPath('/icon.svg') },
};
export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en" suppressHydrationWarning>
      <head>
        <link rel="describedby" href={assetPath('/llms.txt')} />
        <script dangerouslySetInnerHTML={{ __html: themeInitScript }} />
      </head>
      <body className={`${sans.variable} ${mono.variable}`}>
        <a className="skip-link" href="#main">
          Skip to content
        </a>
        <Header />
        {children}
        <SpotlightEffects />
        <Footer />
      </body>
    </html>
  );
}
