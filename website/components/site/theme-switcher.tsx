'use client';

import { useSyncExternalStore } from 'react';
import { Monitor, Moon, Sun } from 'lucide-react';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
} from '@/components/ui/select';
import { readTheme, saveTheme, subscribeTheme } from '@/lib/theme.mjs';

const choices = [
  { value: 'light', label: 'Light', icon: Sun },
  { value: 'dark', label: 'Dark', icon: Moon },
  { value: 'system', label: 'System', icon: Monitor },
] as const;

export function ThemeSwitcher() {
  const theme = useSyncExternalStore(subscribeTheme, readTheme, () => 'system');
  const selected = choices.find((choice) => choice.value === theme)!;
  const Icon = selected.icon;

  return (
    <Select
      value={theme}
      onValueChange={(value) => {
        if (value !== 'light' && value !== 'dark' && value !== 'system') return;
        saveTheme(value);
      }}
    >
      <SelectTrigger
        className="theme-trigger"
        aria-label={`Color theme: ${selected.label}`}
        title={`Color theme: ${selected.label}`}
      >
        <Icon aria-hidden="true" />
        <span className="theme-label">{selected.label}</span>
      </SelectTrigger>
      <SelectContent align="end" alignItemWithTrigger={false} className="p-1">
        {choices.map(({ value, label, icon: ChoiceIcon }) => (
          <SelectItem key={value} value={value} className="py-2">
            <ChoiceIcon aria-hidden="true" /> {label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
