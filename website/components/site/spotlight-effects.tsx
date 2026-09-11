'use client';

import { useEffect } from 'react';
import { usePathname } from 'next/navigation';
import { installSpotlights } from '@/lib/spotlights.mjs';

export function SpotlightEffects() {
  const pathname = usePathname();
  useEffect(() => installSpotlights(), [pathname]);
  return null;
}
