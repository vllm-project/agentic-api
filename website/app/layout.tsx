import type { Metadata } from 'next';
import { Geist, Geist_Mono } from 'next/font/google';
import { Header } from '@/components/site/header';
import { Footer } from '@/components/site/footer';
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
    'vLLM Agentic API is the agentic server that lets you run Codex and Claude Code on top of vLLM.',
  openGraph: {
    title: 'vLLM Agentic API — Your agent harness. Now on vLLM.',
    description:
      'vLLM Agentic API is the agentic server that lets you run Codex and Claude Code on top of vLLM.',
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
        <script dangerouslySetInnerHTML={{ __html: themeInitScript }} />
      </head>
      <body className={`${sans.variable} ${mono.variable}`}>
        <a className="skip-link" href="#main">
          Skip to content
        </a>
        <Header />
        {children}
        <Footer />
      </body>
    </html>
  );
}
