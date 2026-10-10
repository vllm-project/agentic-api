import Markdown, { type Options } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeSlug from 'rehype-slug';
import rehypeAutolinkHeadings, {
  type Options as HeadingLinkOptions,
} from 'rehype-autolink-headings';

export function LinkedMarkdown({
  children,
  urlTransform,
  commandHeadings = false,
  gfm = false,
}: Pick<Options, 'children' | 'urlTransform'> & {
  commandHeadings?: boolean;
  gfm?: boolean;
}) {
  const headingLinks: HeadingLinkOptions = {
    behavior: 'wrap',
    properties: { className: ['heading-anchor'] },
    headingProperties: (heading) =>
      commandHeadings && heading.tagName === 'h2'
        ? { className: ['command-heading'] }
        : {},
    content: (heading) =>
      commandHeadings && heading.tagName === 'h2'
        ? [
            {
              type: 'element',
              tagName: 'span',
              properties: { className: ['command-prompt'], ariaHidden: 'true' },
              children: [{ type: 'text', value: '$' }],
            },
            ...heading.children,
          ]
        : heading.children,
  };

  return (
    <Markdown
      skipHtml
      urlTransform={urlTransform}
      remarkPlugins={gfm ? [remarkGfm] : []}
      rehypePlugins={[rehypeSlug, [rehypeAutolinkHeadings, headingLinks]]}
    >
      {children}
    </Markdown>
  );
}
