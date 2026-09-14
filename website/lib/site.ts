export const REPO = 'https://github.com/vllm-project/agentic-api';
export const DOCS = '/docs';
export const SLACK = 'https://slack.vllm.ai/';
export const SLACK_CHANNEL = '#sig-agentic-api';
export const SITE_URL =
  process.env.NEXT_PUBLIC_SITE_URL ||
  'https://vllm-agentic-api.franciscojavierarceo.chatgpt.site';

export function assetPath(path: string) {
  return `${process.env.NEXT_PUBLIC_BASE_PATH || ''}${path}`;
}
