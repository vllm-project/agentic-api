import { safeLink } from './protocol.mjs';

// A small Markdown renderer using DOM nodes only. Raw HTML is always displayed as text.
// Supports headings, paragraphs, lists, quotes, tables, fenced code, emphasis, and links.
function element(tag, text) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  return node;
}

function inline(parent, text, depth = 0) {
  if (depth > 8) { parent.append(document.createTextNode(text)); return; }
  const tokens = /(`+)([^`]*?)\1|\[([^\]]+)\]\(([^\s)]+)\)|\*\*(.+?)\*\*|__(.+?)__|\*([^*\n]+)\*/g;
  let offset = 0;
  for (const match of text.matchAll(tokens)) {
    parent.append(document.createTextNode(text.slice(offset, match.index)));
    let child;
    if (match[1]) child = element('code', match[2]);
    else if (match[3]) {
      const url = safeLink(match[4]);
      child = element(url ? 'a' : 'span', match[3]);
      if (url) { child.href = url; child.target = '_blank'; child.rel = 'noopener noreferrer'; }
    } else { child = element(match[5] || match[6] ? 'strong' : 'em'); inline(child, match[5] || match[6] || match[7], depth + 1); }
    parent.append(child); offset = match.index + match[0].length;
  }
  parent.append(document.createTextNode(text.slice(offset)));
}

const cells = line => line.trim().replace(/^\|/, '').replace(/\|$/, '').split(/(?<!\\)\|/).map(cell => cell.trim().replaceAll('\\|', '|'));
const divider = line => line?.includes('|') && cells(line).every(cell => /^:?-{3,}:?$/.test(cell));
const blockStart = line => /^( {0,3})(#{1,6}\s|`{3,}|~{3,}|>\s?|[-*+]\s|\d+[.)]\s|(?:---+|\*\*\*+)\s*$)/.test(line);

export function renderMarkdown(text, depth = 0) {
  const root = element('div'); root.className = 'markdown';
  if (depth >= 12) { root.append(element('p', text)); return root; }
  const lines = String(text).replaceAll('\r\n', '\n').split('\n');
  for (let i = 0; i < lines.length;) {
    const line = lines[i];
    if (!line.trim()) { i++; continue; }
    const fence = /^\s{0,3}(`{3,}|~{3,})(.*)$/.exec(line);
    if (fence) {
      const code = [], marker = fence[1][0], length = fence[1].length;
      i++;
      while (i < lines.length && !(lines[i].trim().startsWith(marker.repeat(length)) && new RegExp(`^${marker}+\\s*$`).test(lines[i].trim()))) code.push(lines[i++]);
      if (i < lines.length) i++;
      const wrapper = element('div'); wrapper.className = 'code-block';
      const bar = element('div'); bar.className = 'code-toolbar';
      bar.append(element('span', fence[2].trim() || 'Code'));
      const copy = element('button', 'Copy code'); copy.type = 'button';
      copy.addEventListener('click', async () => {
        try { await navigator.clipboard.writeText(code.join('\n')); copy.textContent = 'Copied'; }
        catch { copy.textContent = 'Select code to copy'; }
      });
      bar.append(copy);
      const pre = element('pre'); pre.append(element('code', code.join('\n')));
      wrapper.append(bar, pre); root.append(wrapper); continue;
    }
    const heading = /^\s{0,3}(#{1,6})\s+(.+?)(?:\s+#+\s*)?$/.exec(line);
    if (heading) { const h = element(`h${heading[1].length}`); inline(h, heading[2]); root.append(h); i++; continue; }
    if (/^\s*(---+|\*\*\*+|___+)\s*$/.test(line)) { root.append(element('hr')); i++; continue; }
    if (line.includes('|') && divider(lines[i + 1])) {
      const table = element('table'), head = element('thead'), body = element('tbody');
      const row = (values, tag) => { const tr = element('tr'); for (const value of values) { const cell = element(tag); inline(cell, value); tr.append(cell); } return tr; };
      head.append(row(cells(line), 'th')); i += 2;
      while (i < lines.length && lines[i].trim() && lines[i].includes('|')) body.append(row(cells(lines[i++]), 'td'));
      table.append(head, body); const wrapper = element('div'); wrapper.className = 'table-scroll'; wrapper.append(table); root.append(wrapper); continue;
    }
    if (/^\s*>/.test(line)) {
      const quote = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) quote.push(lines[i++].replace(/^\s*>\s?/, ''));
      const block = element('blockquote'); block.append(renderMarkdown(quote.join('\n'), depth + 1)); root.append(block); continue;
    }
    const list = /^\s*([-*+]|\d+[.)])\s+/.exec(line);
    if (list) {
      const ordered = /^\d/.test(list[1]), ul = element(ordered ? 'ol' : 'ul');
      if (ordered) ul.start = parseInt(list[1], 10);
      while (i < lines.length) {
        const entry = /^\s*([-*+]|\d+[.)])\s+(.*)$/.exec(lines[i]);
        if (!entry || /^\d/.test(entry[1]) !== ordered) break;
        const li = element('li'); inline(li, entry[2]); ul.append(li); i++;
      }
      root.append(ul); continue;
    }
    const paragraph = [line]; i++;
    while (i < lines.length && lines[i].trim() && !blockStart(lines[i]) && !divider(lines[i + 1])) paragraph.push(lines[i++]);
    const p = element('p'); inline(p, paragraph.join('\n')); root.append(p);
  }
  return root;
}

export function downloadAnswer(text, extension) {
  const url = URL.createObjectURL(new Blob([text], { type: extension === 'md' ? 'text/markdown;charset=utf-8' : 'text/plain;charset=utf-8' }));
  const link = element('a'); link.href = url; link.download = `agentic-answer.${extension}`; link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
