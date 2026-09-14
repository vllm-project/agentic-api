'use client';

import { useRouter } from 'next/navigation';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { docsVersions } from '@/lib/docs';

export function DocsVersionSelect({ versionId }: { versionId: string }) {
  const router = useRouter();
  return (
    <div className="docs-version-control">
      <span id="docs-version-label">Documentation version</span>
      <Select
        value={versionId}
        items={docsVersions.map((version) => ({
          value: version.id,
          label: version.label,
        }))}
        onValueChange={(value) => {
          if (value && docsVersions.some((version) => version.id === value)) {
            router.push(`/docs/${value}`);
          }
        }}
      >
        <SelectTrigger
          aria-labelledby="docs-version-label"
          className="docs-version-trigger"
        >
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {docsVersions.map((version) => (
            <SelectItem key={version.id} value={version.id}>
              {version.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </div>
  );
}
