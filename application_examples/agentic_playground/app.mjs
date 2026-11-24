import { apiBase, buildRequest, createRun, applyEvent, itemText, safeLink, comparisonPrompt, teamPrompt } from './protocol.mjs';
import { createSettingsUI, redact } from './settings.mjs';
import { executeHandler } from './sandbox.mjs';
import { clientKinds, pendingCalls, executeClientCall, mapLimited, sumUsage, withContinuation, compactionRequest, clientQueue } from './client-tools.mjs';
import { renderMarkdown, downloadAnswer } from './markdown.mjs';
import { createResponsesTransport, jsonRequest, authHeaders as headers, checkResponse, uploadFile } from './transport.mjs';


const $ = id => document.getElementById(id);
const generalPrompt = 'Implement a Python function that merges overlapping intervals. Execute examples and edge-case tests, then explain correctness and time/space complexity.';
const sampleTasks = [
  'Implement two_sum(nums, target) in Python. Run examples covering duplicates, negatives, and no solution. Explain correctness and complexity.',
  'Implement merge_intervals(intervals) in Python without mutating the input. Run examples covering unsorted input, nested intervals, touching endpoints, and an empty list.',
];
let serial = 0;
let controller = null;
let run = createRun();
let lastRequest = null;
let sessionConfig = null;
let previousResponseId = null;
let history = [];
let chatTurns = [];
let activeChatTurn = null;
const chatViews = new WeakMap();
let started = 0;
let timer = null;
let renderTimer = null;
let renderedRun = null;
const agentViews = new Map();
let treeView = null;
let renderedAnswer = null;
let view = 'activity';
let activeApi = 'responses';
let conversationId = null;
let conversationBase = null;
let checkpointStored = true;
let itemCursor = null;
const sessions = new Map();
const transports = { responses: createResponsesTransport(), conversations: createResponsesTransport() };
let currentResponses = [];

function node(tag, text, className) {
  const element = document.createElement(tag);
  if (text !== undefined) element.textContent = text;
  if (className) element.className = className;
  return element;
}

function notice(message) {
  $('notice').textContent = message;
  $('notice').hidden = !message;
}

const missionControls = node('div', undefined, 'mission-controls');
const modeLabel = node('label', 'Workflow');
const mode = node('select'); mode.id = 'mode';
mode.append(new Option('General prompt', 'prompt'), new Option('Two-agent coding comparison', 'mission'), new Option('Four-agent tool demo', 'team'), new Option('Separate tasks for each agent', 'tasks'));
mode.value = 'team'; modeLabel.append(mode);
const missionLabel = node('label', 'What should the agents do?');
missionLabel.htmlFor = 'mission';
const mission = node('textarea');
mission.id = 'mission';
mission.rows = 12;
mission.value = teamPrompt;
mission.addEventListener('input', preview);
missionControls.append(modeLabel, missionLabel, mission);
$('tasks').before(missionControls);
const roster = node('p', 'Progress updates appear as the gateway reports them.', 'progress-summary');
roster.setAttribute('role', 'status');
roster.setAttribute('aria-live', 'polite');
document.querySelector('.view-bar').before(roster);

function setMode() {
  const separate = mode.value === 'tasks';
  mission.hidden = missionLabel.hidden = separate;
  $('tasks').hidden = $('add').hidden = !separate;
  document.querySelector('.section-description').textContent = separate
    ? 'Give each agent a focused task.' : 'Describe the goal once. Your coordinator takes it from here.';
  settingsUI.setMultiAgent(mode.value !== 'prompt');
  if (mode.value === 'team') loadTeamExample();
  else {
    $('concurrency').value = mode.value === 'prompt' ? '1' : '2';
    if (mode.value === 'prompt') mission.value = generalPrompt;
    if (mode.value === 'mission') mission.value = comparisonPrompt;
  }
  preview();
}
mode.addEventListener('change', setMode);

function loadTeamExample() {
  mission.value = teamPrompt;
  settingsUI.setMultiAgent(true);
  $('concurrency').value = '4';
  $('search').checked = $('code').checked = $('store').checked = true;
  settingsUI.enableTeamTools();
  if (!settingsUI.read().vectorStoreIds.trim()) void settingsUI.loadFileSearchDemo();
}
const settingsContainer = node('div');
$('run').before(settingsContainer);
const settingsUI = createSettingsUI(settingsContainer, preview, executeHandler, notice, prepareFileSearchDemo);

const usagePanel = node('div', undefined, 'usage-panel');
usagePanel.id = 'usage-panel';
usagePanel.setAttribute('aria-live', 'polite');
document.querySelector('.metrics').after(usagePanel);
const turnHistory = node('details', undefined, 'turn-history');
turnHistory.append(node('summary', 'Usage by turn'));
const usageRows = node('div'); turnHistory.append(usageRows);
$('followup-form').after(turnHistory);
const compactButton = node('button', 'Compact conversation', 'text-button');
compactButton.type = 'button'; compactButton.disabled = true;
document.querySelector('.result-actions').append(compactButton);
const compactionNotice = node('div', undefined, 'compaction-notice'); compactionNotice.hidden = true;
$('followup-form').before(compactionNotice);

function renderUsage() {
  const usage = sumUsage(currentResponses);
  const count = value => value == null ? '—' : value.toLocaleString();
  usagePanel.replaceChildren(...[['Input tokens', usage.input_tokens], ['Output tokens', usage.output_tokens], ['Total tokens', usage.total_tokens]].map(([label, value]) => {
    const cell = node('div'); cell.append(node('span', label), node('strong', count(value))); return cell;
  }));
  usagePanel.title = `${usage.reportedResponses} of ${usage.responseCount} responses reported usage. A dash means unavailable.`;
  let turnNumber = 0;
  usageRows.replaceChildren(...history.map(turn => {
    const totals = turn.usage || sumUsage(turn.responses || [turn.response]);
    return node('p', `${turn.kind === 'compaction' ? 'Compaction' : `Turn ${++turnNumber}`} · input ${count(totals.input_tokens)} · output ${count(totals.output_tokens)} · total ${count(totals.total_tokens)}`, 'hint');
  }));
}

function addTask(problem = '') {
  if ($('tasks').children.length >= 8) return;
  const id = `task-${++serial}`;
  const card = node('div', undefined, 'task');
  const heading = node('div', undefined, 'task-header');
  heading.append(node('span', '', 'avatar'));
  const label = node('label'); label.htmlFor = id;
  const remove = node('button', '×', 'remove'); remove.type = 'button';
  remove.addEventListener('click', () => { card.remove(); numberTasks(); preview(); });
  heading.append(label, remove);
  const textarea = node('textarea'); textarea.id = id; textarea.rows = 7; textarea.value = problem;
  textarea.placeholder = 'Describe the problem and what a correct solution should do…';
  textarea.addEventListener('input', preview);
  card.append(heading, textarea); $('tasks').append(card); numberTasks();
}
function numberTasks() {
  [...$('tasks').children].forEach((card, i) => {
    card.querySelector('.avatar').textContent = String(i + 1).padStart(2, '0');
    card.querySelector('label').textContent = `Agent ${i + 1}`;
    const remove = card.querySelector('button');
    remove.setAttribute('aria-label', `Remove agent ${i + 1}`);
    remove.disabled = $('tasks').children.length === 1 || Boolean(controller);
  });
  $('add').disabled = $('tasks').children.length >= 8 || Boolean(controller);
}

function config() {
  const base = apiBase($('base').value.trim());
  return {
    base, token: $('token').value.trim(), model: $('model').value,
    search: $('search').checked, code: $('code').checked, concurrency: Number($('concurrency').value),
    mode: mode.value, mission: mission.value,
    tasks: [...$('tasks').querySelectorAll('textarea')].map(input => ({ problem: input.value })),
    ...settingsUI.read(base),
    apiMode: activeApi, transport: activeApi === 'responses' ? $('transport').value : 'http',
    store: activeApi === 'responses' ? $('store').checked : true,
  };
}

function preview() {
  if (controller) return;
  try { $('request-preview').textContent = JSON.stringify(redact(withContinuation(buildRequest(config()), continuation())), null, 2); }
  catch (error) { $('request-preview').textContent = error.message; }
}

async function prepareFileSearchDemo() {
  if (controller) throw new Error('Wait for the current operation to finish before loading the sample document.');
  const settings = config();
  controller = new AbortController();
  const signal = AbortSignal.any([controller.signal, AbortSignal.timeout(60000)]);
  let uploadedId;
  busy(true);
  notice('Loading the sample vector store…');
  try {
    let after = '';
    do {
      const stores = await jsonRequest(settings, `/v1/vector_stores?limit=100${after ? `&after=${encodeURIComponent(after)}` : ''}`, 'GET', undefined, signal);
      const existing = stores.data.find(store => store.metadata?.playground_sample === 'archive-requirements.md' && store.status === 'completed' && store.file_counts?.completed > 0);
      if (existing) return { id: existing.id, base: settings.base };
      if (!stores.has_more) break;
      if (!stores.last_id || stores.last_id === after) throw new Error('The gateway returned an invalid vector store page.');
      after = stores.last_id;
    } while (true);
    notice('Uploading and indexing the sample document…');
    const sample = await checkResponse(await fetch(new URL('./archive-requirements.md', import.meta.url), { signal }));
    uploadedId = await uploadFile(settings, await sample.blob(), 'archive-requirements.md', signal);
    const store = await jsonRequest(settings, '/v1/vector_stores', 'POST', { name: 'playground-archive', file_ids: [uploadedId], metadata: { playground_sample: 'archive-requirements.md' } }, signal);
    if (!store.id) throw new Error('The vector store response did not contain an ID.');
    return { id: store.id, base: settings.base };
  } catch (error) {
    if (uploadedId) throw new Error(`Sample file ${uploadedId} was uploaded, but vector store setup failed: ${error.message}`);
    throw error;
  } finally { controller = null; busy(false); }
}

$('connect').addEventListener('click', async () => {
  $('connect').disabled = true;
  notice('');
  try {
    const settings = config();
    const response = await checkResponse(await fetch(`${settings.base}/v1/models`, {
      headers: headers(settings), signal: AbortSignal.timeout(15000),
    }));
    const models = (await response.json()).data;
    if (!Array.isArray(models) || !models.length) throw new Error('The gateway returned no served models.');
    $('models').replaceChildren(...models.map(model => { const option = node('option'); option.value = model.id; return option; }));
    if (!$('model').value) $('model').value = models[0].id;
    $('connection-dot').classList.add('connected');
    $('connection-label').textContent = `${models.length} model${models.length === 1 ? '' : 's'} available`;
    preview();
    notice('Model listing succeeded. Tool availability is checked by the gateway when a run starts.');
  } catch (error) {
    $('connection-dot').classList.remove('connected');
    $('connection-label').textContent = 'Connection failed';
    notice(`${error.message} Check the URL, authentication, and gateway CORS settings.`);
  } finally { $('connect').disabled = Boolean(controller); }
});

function busy(value) {
  for (const input of document.querySelectorAll('.brief input, .brief textarea, .brief select, .brief button, .connection input, .connection button, .api-session button, .api-session input, .api-session select')) input.disabled = value;
  $('disconnect').disabled = !value;
  $('download').disabled = value || !history.length;
  $('followup').disabled = value || !(previousResponseId || conversationId);
  $('send').disabled = value || !(previousResponseId || conversationId);
  $('send').textContent = value ? 'Working…' : 'Send ↑';
  $('followup').placeholder = value ? 'Waiting for the final response…' : 'Message the agents…';
  compactButton.disabled = value || !(previousResponseId || conversationId) || (activeApi === 'responses' && !checkpointStored && sessionConfig?.transport !== 'websocket');
  numberTasks();
  if (!value) settingsUI.sync();
  document.querySelector('.results').dataset.running = String(value);
  $('run').firstChild.textContent = value ? 'Working… ' : 'Send prompt ';
  updateSessionHint();
}

function scheduleRender() {
  // Progress needs a few updates per second, not a complete redraw for every token.
  if (renderTimer === null) renderTimer = setTimeout(() => { renderTimer = null; render(); }, 100);
}

function setText(element, text) {
  if (element.textContent !== String(text)) element.textContent = text;
}

function linksFor(item, container) {
  const sources = [...(item.action?.sources || []), ...(item.content || []).flatMap(part => part?.annotations || [])];
  for (const source of sources) {
    if (source.type === 'file_citation') {
      container.append(node('p', `Source file: ${source.filename || source.file_id} · ${source.file_id}`, 'hint'));
      continue;
    }
    const href = safeLink(source.url);
    if (!href) continue;
    const link = node('a', source.title || source.url);
    link.href = href;
    link.target = '_blank';
    link.rel = 'noopener noreferrer';
    container.append(link);
  }
}

function renderFileSearch(item, container) {
  container.append(node('h4', 'File search'), node('p', (item.queries || []).join(', ') || 'Searching files…', 'hint'));
  for (const result of item.results || []) {
    const details = [result.file_id, result.score == null ? '' : `Score: ${result.score}`].filter(Boolean).join(' · ');
    container.append(node('strong', result.filename || result.file_id || 'Retrieved passage'), node('p', details, 'hint'), node('pre', result.text || ''));
  }
  if (item.status === 'failed' || item.status === 'incomplete') container.append(node('p', `File search ${item.status}.`, 'hint'));
  else if (item.status === 'completed' && !item.results?.length) container.append(node('p', item.results == null ? 'Retrieved passages were not included in the response.' : 'No matching passages.', 'hint'));
}

function renderItem(item) {
  const card = node('article', undefined, 'activity-item');
  const heading = node('div', undefined, 'item-heading');
  heading.append(node('strong', item.agent?.agent_name || 'Unattributed'), node('span', item.status || ''), node('span', item.type.replaceAll('_', ' '), 'kind'));
  card.append(heading);
  if (item.type === 'message') {
    const text = itemText(item);
    const actions = node('div', undefined, 'answer-actions');
    for (const [label, extension] of [['Download Markdown', 'md'], ['Download text', 'txt']]) {
      const button = node('button', label, 'text-button'); button.type = 'button';
      button.addEventListener('click', () => downloadAnswer(text, extension)); actions.append(button);
    }
    card.append(actions, renderMarkdown(text || 'Writing…'));
  } else if (item.type === 'code_interpreter_call') {
    card.append(node('pre', item.code || 'Preparing Python…'));
    for (const output of item.outputs || []) if (output.type === 'logs') card.append(node('pre', output.logs, 'logs'));
  } else if (item.type === 'web_search_call') {
    card.append(node('p', item.action?.query || (item.action?.queries || []).join(', ') || item.action?.url || 'Searching the web…', 'prose'));
  } else if (item.type === 'file_search_call') {
    renderFileSearch(item, card);
  } else if (item.type === 'multi_agent_call') {
    card.append(node('p', item.action?.replaceAll('_', ' ') || 'Collaboration', 'prose'));
    const details = node('details');
    details.append(node('summary', 'Call arguments'), node('pre', item.arguments || ''));
    card.append(details);
  } else if (item.type === 'multi_agent_call_output') {
    const details = node('details');
    details.append(node('summary', `${item.action?.replaceAll('_', ' ') || 'Collaboration'} output`), node('pre', (item.output || []).map(part => part.text || '').join('\n')));
    card.append(details);
  } else if (item.type === 'agent_message') {
    card.append(node('p', `${item.author} → ${item.recipient}. Message content is opaque in the public transcript.`, 'prose'));
  } else return null;
  linksFor(item, card);
  return card;
}

function agentLabel(agent) {
  if (agent === '/root') return sessionConfig?.multiAgent ? 'Coordinator' : 'Assistant';
  const name = agent.split('/').at(-1);
  return ({ solver_a: 'Agent A', solver_b: 'Agent B' })[name] || name.replaceAll('_', ' ');
}

function refreshEvidence(view) {
  if (!view.details.open) return;
  // Item replacements and the two streamed/client mutations invalidate visible evidence.
  const snapshot = view.evidence.flatMap(item => [item, item.status, item.code, item.clientOutput]);
  if (view.evidenceSnapshot?.length === snapshot.length && snapshot.every((value, i) => value === view.evidenceSnapshot[i])) return;
  view.evidenceSnapshot = snapshot;
  const blocks = view.evidence.map(item => {
    const block = node('div', undefined, 'evidence-item');
    if (item.type === 'code_interpreter_call') {
      block.append(node('h4', 'Python execution'), node('pre', item.code || 'Preparing code…'));
      for (const output of item.outputs || []) if (output.type === 'logs') block.append(node('pre', output.logs, 'logs'));
    } else if (item.type === 'web_search_call') block.append(node('h4', 'Web search'), node('p', item.action?.query || (item.action?.queries || []).join(', ') || item.action?.url || 'Searching…', 'hint'));
    else if (item.type === 'file_search_call') renderFileSearch(item, block);
    else if (item.type === 'mcp_call') block.append(node('h4', `${item.server_label} · ${item.name}`), node('pre', item.output || (item.error ? JSON.stringify(item.error) : 'Waiting for tool output…')));
    else if (item.type === 'mcp_list_tools') block.append(node('h4', `Tools from ${item.server_label}`), node('p', (item.tools || []).map(tool => tool.name).join(', ') || 'Discovering…', 'hint'));
    else block.append(node('h4', item.name || item.type.replaceAll('_', ' ')), node('pre', item.clientOutput || item.arguments || item.input || JSON.stringify(item.action || {})));
    linksFor(item, block);
    return block;
  });
  view.evidenceBody.replaceChildren(...blocks);
}

function refreshEventTypes(details, body, types) {
  if (!details.open) return;
  const key = JSON.stringify([...(types || [])]);
  if (body.dataset.counts === key) return;
  body.dataset.counts = key;
  body.replaceChildren(...[...(types || [])].map(([type, count]) => {
    const line = node('div', undefined, 'sse-type'); line.append(node('span', type), node('span', `×${count}`)); return line;
  }));
}

function renderAgent(agent) {
  let view = agentViews.get(agent);
  if (!view) {
    const card = node('article', undefined, 'agent-progress');
    card.dataset.agent = agent === '/root' ? 'coordinator' : agent.endsWith('solver_b') ? 'b' : 'a';
    const header = node('div', undefined, 'agent-heading');
    const avatar = node('span', agent === '/root' ? '◎' : agentLabel(agent).replace('Agent ', '').slice(0, 2).toUpperCase(), 'agent-avatar');
    const title = node('div'); title.append(node('h3', agentLabel(agent)), node('small', agent));
    const badge = node('span', '', 'agent-badge'), bar = node('progress'); bar.max = 1;
    header.append(avatar, title, badge);
    const list = node('ol', undefined, 'progress-steps'), hint = node('p', '', 'agent-hint'); hint.hidden = true;
    card.append(header, bar, list, hint);
    const details = node('details', undefined, 'agent-evidence'); details.dataset.detailKey = `evidence:${agent}`;
    const summary = node('summary'), evidenceBody = node('div'); details.append(summary, evidenceBody);
    const typesDetails = node('details', undefined, 'sse-types'); typesDetails.dataset.detailKey = `types:${agent}`;
    const typesSummary = node('summary'), typesBody = node('div'); typesDetails.append(typesSummary, typesBody);
    view = { card, badge, bar, list, hint, details, summary, evidenceBody, typesDetails, typesSummary, typesBody, evidence: [] };
    details.addEventListener('toggle', () => {
      if (details.open) refreshEvidence(view);
      else { evidenceBody.replaceChildren(); view.evidenceSnapshot = null; }
    });
    typesDetails.addEventListener('toggle', () => refreshEventTypes(typesDetails, typesBody, run.eventTypes.get(agent)));
    agentViews.set(agent, view);
  }
  const steps = [...(run.progress.get(agent)?.values() || [])];
  const latest = steps.at(-1);
  const finished = latest?.label === 'Reporting the solution' && latest.status === 'done' && !['failed', 'incomplete', 'error', 'disconnected'].includes(run.status);
  const live = Boolean(controller) && !finished;
  const state = finished ? 'Finished' : live ? latest?.label || 'Starting' : run.status === 'completed' ? 'Turn completed' : 'Stopped';
  setText(view.badge, state);
  if (view.badge.dataset.live !== String(live)) view.badge.dataset.live = String(live);
  if (live) { if (view.bar.hasAttribute('value')) view.bar.removeAttribute('value'); }
  else if (!view.bar.hasAttribute('value') || view.bar.value !== Number(finished || run.status === 'completed')) view.bar.value = Number(finished || run.status === 'completed');
  const label = `${agentLabel(agent)}: ${state}`;
  if (view.bar.getAttribute('aria-label') !== label) view.bar.setAttribute('aria-label', label);
  const recent = steps.slice(-6), stepsKey = JSON.stringify([recent, live]);
  if (view.stepsKey !== stepsKey) {
    view.stepsKey = stepsKey;
    view.list.replaceChildren(...recent.map(step => {
      const row = node('li'); row.dataset.state = step.status;
      const icon = step.status === 'done' ? '✓' : step.status === 'failed' ? '!' : live ? '◌' : '–';
      const content = node('div'); content.append(node('span', step.label));
      if (step.detail) content.append(node('small', step.detail));
      if (step.eventType) content.append(node('small', step.eventType, 'sse-hint'));
      row.append(node('span', icon, 'step-icon'), content, node('span', step.status === 'done' ? 'Done' : step.status === 'failed' ? 'Failed' : live ? 'In progress' : 'Stopped', 'step-state'));
      return row;
    }));
    if (!recent.length) view.list.append(node('li', 'Waiting for the next agent event…', 'hint'));
  }
  const items = [...run.items.values()].filter(item => item.agent?.agent_name === agent);
  const message = items.filter(item => item.type === 'message').at(-1);
  const text = message ? itemText(message) : '';
  view.hint.hidden = !text;
  setText(view.hint, text.slice(0, 240) + (text.length > 240 ? '…' : ''));
  view.evidence = items.filter(item => ['code_interpreter_call', 'web_search_call', 'file_search_call', 'mcp_call', 'mcp_list_tools', ...clientKinds].includes(item.type));
  if (view.evidence.length) {
    if (!view.card.contains(view.details)) view.card.append(view.details);
    setText(view.summary, `View code, outputs & sources (${view.evidence.length})`);
    refreshEvidence(view);
  }
  const types = run.eventTypes.get(agent);
  if (types?.size) {
    if (!view.card.contains(view.typesDetails)) view.card.append(view.typesDetails);
    setText(view.typesSummary, `Received SSE types (${types.size})`);
    refreshEventTypes(view.typesDetails, view.typesBody, types);
  }
  return view.card;
}

function renderAgentTree() {
  const agents = new Set(['/root', ...run.agents]);
  const key = [...agents].sort().join('\n');
  if (treeView?.key !== key) {
    const diagram = node('section', undefined, 'agent-tree'); diagram.setAttribute('aria-label', 'Observed agent delegation tree');
    diagram.append(node('h3', 'Live agent tree'), node('p', 'Branches appear when the API reports subagents. Select an agent to inspect its progress.', 'hint'));
    const children = new Map(), buttons = new Map();
    for (const agent of agents) {
      if (agent === '/root') continue;
      let parent = agent.slice(0, agent.lastIndexOf('/'));
      while (parent !== '/root' && !agents.has(parent)) parent = parent.slice(0, parent.lastIndexOf('/')) || '/root';
      if (!children.has(parent)) children.set(parent, []);
      children.get(parent).push(agent);
    }
    function branch(agent) {
      const li = node('li'), button = node('button', undefined, 'tree-agent'); button.type = 'button';
      const state = node('small');
      button.append(node('span', agent === '/root' ? '◎' : agentLabel(agent).replace('Agent ', '').slice(0, 2).toUpperCase(), 'tree-avatar'), node('strong', agentLabel(agent)), node('span', agent, 'tree-path'), state);
      button.addEventListener('click', () => { $('agent-filter').value = agent; render(); });
      buttons.set(agent, { button, state }); li.append(button);
      if (children.has(agent)) { const list = node('ul'); list.append(...children.get(agent).sort().map(branch)); li.append(list); }
      return li;
    }
    const tree = node('ul', undefined, 'tree-root'); tree.append(branch('/root')); diagram.append(tree);
    treeView = { key, diagram, buttons };
  }
  for (const [agent, { button, state }] of treeView.buttons) {
    const latest = [...(run.progress.get(agent)?.values() || [])].at(-1);
    setText(state, latest?.status === 'active' && !controller ? 'Turn ended' : latest?.label || (run.agents.has(agent) ? 'Agent observed' : 'Waiting for agent events'));
    if (button.dataset.state !== (latest?.status || 'active')) button.dataset.state = latest?.status || 'active';
    const selected = String($('agent-filter').value === agent);
    if (button.getAttribute('aria-pressed') !== selected) button.setAttribute('aria-pressed', selected);
  }
  return treeView.diagram;
}

function render() {
  clearTimeout(renderTimer); renderTimer = null;
  if (renderedRun !== run) {
    const hadRun = renderedRun !== null;
    renderedRun = run; agentViews.clear(); treeView = null; renderedAnswer = null;
    if (hadRun || controller || run.eventCount) $('activity').replaceChildren(node('p', 'Send a prompt to watch live progress.', 'placeholder'));
  }
  setText($('agent-count'), run.agents.size);
  setText($('tool-count'), [...run.items.values()].filter(item => ['web_search_call', 'file_search_call', 'code_interpreter_call', 'mcp_call', ...clientKinds].includes(item.type)).length);
  const summary = run.agents.size ? [...run.agents].map(agent => `${agentLabel(agent)}: ${run.activity.get(agent) || 'observed'}`).join(' · ') : controller ? 'Request sent. Waiting for agent events…' : 'Progress updates appear as the gateway reports them.';
  setText(roster, summary);
  $('run-status').dataset.state = $('run-status').textContent.toLowerCase();
  const filter = $('agent-filter');
  for (const option of [...filter.options]) if (option.value !== 'all' && !run.agents.has(option.value)) option.remove();
  const options = new Set([...filter.options].map(option => option.value));
  for (const agent of run.agents) if (!options.has(agent)) filter.add(new Option(agent, agent));
  if (!filter.value) filter.value = 'all';
  if (view === 'activity' && (run.eventCount || controller)) {
    const pane = $('activity');
    const cards = [...run.agents].filter(agent => filter.value === 'all' || agent === filter.value).map(renderAgent);
    const unattributed = run.eventTypes.get('unattributed');
    if (unattributed?.size && filter.value === 'all') {
      let details = pane.querySelector('.global-events');
      if (!details) {
        details = node('details', undefined, 'sse-types global-events'); details.dataset.detailKey = 'unattributed';
        const body = node('div'); details.append(node('summary'), body);
        details.addEventListener('toggle', () => refreshEventTypes(details, body, run.eventTypes.get('unattributed')));
      }
      setText(details.firstElementChild, `Other received SSE types (${unattributed.size})`);
      refreshEventTypes(details, details.lastElementChild, unattributed);
      cards.push(details);
    }
    if (sessionConfig?.multiAgent) cards.unshift(renderAgentTree());
    if (!cards.length) cards.push(pane.querySelector('.placeholder') || node('p', 'Waiting for visible agent activity…', 'placeholder'));
    const existing = [...pane.children];
    if (existing.length !== cards.length || cards.some((card, i) => card !== existing[i])) {
      const scrollTop = pane.scrollTop;
      pane.replaceChildren(...cards);
      pane.scrollTop = scrollTop;
    }
  }
  if (view === 'answer') {
    const final = finalMessage(), text = final ? itemText(final) : '';
    const streaming = Boolean(controller) && final?.status !== 'completed' && run.status !== 'completed';
    if (renderedAnswer?.item !== final || renderedAnswer?.text !== text || renderedAnswer?.streaming !== streaming) {
      if (final && streaming) {
        if (!renderedAnswer?.streaming || renderedAnswer.id !== final.id) $('answer').replaceChildren(node('p', '', 'prose'));
        setText($('answer').firstElementChild, text || 'Writing…');
      } else $('answer').replaceChildren(...(final ? [renderItem(final)] : [node('p', 'The coordinator’s answer will appear here.', 'placeholder')]));
      renderedAnswer = { item: final, id: final?.id, text, streaming };
    }
  }
  if (view === 'api' && $('events-preview').parentElement.open) setText($('events-preview'), run.events.map(event => JSON.stringify(redact(event))).join('\n\n') || 'No events yet.');
  if (activeChatTurn) {
    const steps = run.progress.get('/root');
    activeChatTurn.messages = [...run.items.values()]
      .filter(item => item.type === 'message' && item.agent?.agent_name === '/root')
      .map(item => ({ id: item.id, text: itemText(item), complete: item.status === 'completed' || steps?.get(item.id)?.status === 'done' }));
    const latest = [...(steps?.values() || [])].at(-1);
    activeChatTurn.stage = activeChatTurn.status === 'running'
      ? $('run-status').textContent === 'Waiting for you' ? 'Waiting for you' : latest?.label || 'Waiting for the model'
      : activeChatTurn.status === 'completed' ? 'Completed' : activeChatTurn.status === 'disconnected' ? 'Disconnected' : 'Error';
    activeChatTurn.stageStatus = activeChatTurn.status === 'running'
      ? $('run-status').textContent === 'Waiting for you' ? 'active' : latest?.status || 'active'
      : activeChatTurn.status === 'completed' ? 'done' : 'failed';
  }
  renderChat();
}

function renderChat(forceScroll = false) {
  const transcript = $('chat-history');
  const followBottom = forceScroll || transcript.hidden || transcript.scrollHeight - transcript.clientHeight - transcript.scrollTop < 48;
  const cards = [];
  let changed = false;
  for (const turn of chatTurns) {
    let view = chatViews.get(turn);
    if (!view) {
      const user = node('article', undefined, 'chat-message chat-user');
      user.append(node('strong', 'You'), node('p', turn.prompt));
      const assistant = node('article', undefined, 'chat-message chat-assistant');
      const body = node('div', undefined, 'chat-root-messages');
      const stage = node('p', '', 'chat-stage'); stage.setAttribute('role', 'status');
      assistant.append(node('strong', turn.multiAgent ? 'Root agent · /root' : 'Assistant'), body, stage);
      view = { user, assistant, body, stage, messages: new Map() }; chatViews.set(turn, view);
    }
    const messages = turn.messages?.length ? turn.messages : turn.answer ? [{ id: 'answer', text: turn.answer, complete: true }] : [];
    const messageNodes = [];
    for (const message of messages) {
      if (!message.text) continue;
      let entry = view.messages.get(message.id);
      if (!entry) { entry = { element: node('div', undefined, 'chat-root-message') }; view.messages.set(message.id, entry); }
      const complete = message.complete || turn.status !== 'running';
      if (entry.text !== message.text || entry.complete !== complete) {
        if (complete) entry.element.replaceChildren(renderMarkdown(message.text));
        else {
          if (!entry.element.firstElementChild || entry.complete) entry.element.replaceChildren(node('p', '', 'chat-streaming-text'));
          setText(entry.element.firstElementChild, message.text);
        }
        entry.text = message.text; entry.complete = complete; changed = true;
      }
      messageNodes.push(entry.element);
    }
    const existing = [...view.body.children];
    if (existing.length !== messageNodes.length || messageNodes.some((element, i) => element !== existing[i])) {
      view.body.replaceChildren(...messageNodes); changed = true;
    }
    const stage = turn.stage || (turn.status === 'running' ? 'Waiting for the model' : turn.status === 'completed' ? 'Completed' : 'Turn stopped');
    const status = turn.stageStatus || (turn.status === 'running' ? 'active' : turn.status === 'completed' ? 'done' : 'failed');
    if (view.stage.textContent !== stage || view.stage.dataset.state !== status) {
      setText(view.stage, stage); view.stage.dataset.state = status; changed = true;
    }
    cards.push(view.user, view.assistant);
  }
  const existing = [...transcript.children];
  if (existing.length !== cards.length || cards.some((card, i) => card !== existing[i])) { transcript.replaceChildren(...cards); changed = true; }
  transcript.hidden = !chatTurns.length;
  if (followBottom && (changed || forceScroll)) transcript.scrollTop = transcript.scrollHeight;
}

function finalMessage() {
  const messages = [...run.items.values()].filter(item => item.type === 'message' && item.agent?.agent_name === '/root');
  return messages.filter(item => item.phase === 'final_answer').at(-1) || messages.at(-1);
}

function continuation() { return { apiMode: activeApi, conversationId, previousResponseId }; }

function validateSession(settings) {
  if ((previousResponseId && sessionConfig?.base !== settings.base) || (conversationId && conversationBase !== settings.base)) throw new Error('This session belongs to another gateway URL. Restore the URL or start a new session.');
  if (previousResponseId && sessionConfig?.multiAgent && !settings.multiAgent) throw new Error('An existing agent tree requires multi-agent execution. Start a new session before disabling it.');
  if (settings.multiAgent && !settings.store) throw new Error('Multi-agent execution requires store:true. Start a single-agent session to use store:false.');
}

function updateSessionHint() {
  $('session-checkpoint').textContent = activeApi === 'conversations' ? conversationId || 'No conversation selected' : previousResponseId ? `previous_response_id: ${previousResponseId}` : 'New Responses session';
  $('transport-hint').textContent = $('transport').value === 'websocket'
    ? 'WebSocket keeps one connection open between prompts, including store:false. Multi-agent execution still requires store:true. Browser bearer headers are unavailable; use HTTP if authentication is required.'
    : $('store').checked ? 'store:true saves a response so its ID can be used by the next prompt. Multi-agent execution requires this setting.' : 'store:false is a single-agent HTTP request. Its returned ID cannot be continued over HTTP; start a new session afterward. Use WebSocket for unstored multi-turn sessions.';
}

function snapshot() {
  return { run, lastRequest, sessionConfig, previousResponseId, history, chatTurns, currentResponses, conversationId, conversationBase, checkpointStored,
    status: $('run-status').textContent, elapsed: $('elapsed').textContent, continuationText: $('continuation').textContent,
    store: $('store').checked, transport: $('transport').value };
}

function restore(state = {}) {
  run = state.run || createRun(); lastRequest = state.lastRequest || null; sessionConfig = state.sessionConfig || null;
  previousResponseId = state.previousResponseId || null; history = state.history || []; chatTurns = state.chatTurns || []; renderChat(); currentResponses = state.currentResponses || [];
  conversationId = state.conversationId || null; conversationBase = state.conversationBase || null; checkpointStored = state.checkpointStored ?? true;
  $('run-status').textContent = state.status || 'Ready'; $('elapsed').textContent = state.elapsed || '—';
  $('continuation').textContent = state.continuationText || 'Send a prompt to begin.';
  $('store').checked = state.store ?? true; $('transport').value = state.transport || 'http';
  $('conversation-id').value = conversationId || ''; $('conversation-status').textContent = conversationId ? `Loaded ${conversationId}` : 'No conversation selected.';
  $('conversation-items').replaceChildren(); $('more-items').hidden = true; itemCursor = null;
  $('activity').replaceChildren(node('p', 'Send a prompt to watch live progress.', 'placeholder'));
  $('followup').value = ''; compactionNotice.hidden = true; notice(''); busy(false); render(); renderUsage(); preview();
}

function switchApi(api) {
  if (controller || activeApi === api) return;
  sessions.set(activeApi, snapshot()); activeApi = api;
  for (const name of ['responses', 'conversations']) {
    const selected = name === api;
    $(`${name}-tab`).setAttribute('aria-selected', String(selected)); $(`${name}-tab`).tabIndex = selected ? 0 : -1;
    $(`${name}-session`).hidden = !selected;
  }
  restore(sessions.get(api));
}
for (const api of ['responses', 'conversations']) {
  $(`${api}-tab`).addEventListener('click', () => switchApi(api));
  $(`${api}-tab`).addEventListener('keydown', event => {
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault(); const next = event.key === 'Home' ? 'responses' : event.key === 'End' ? 'conversations' : api === 'responses' ? 'conversations' : 'responses';
    switchApi(next); $(`${next}-tab`).focus();
  });
}
$('new-session').addEventListener('click', () => { transports[activeApi].close(); restore(); });
for (const id of ['store', 'transport']) $(id).addEventListener('change', () => { updateSessionHint(); preview(); });

async function refreshConversationItems(settings, more = false, signal) {
  if (!conversationId) throw new Error('Create or load a conversation first.');
  if (settings.base !== conversationBase) throw new Error('Restore this conversation’s gateway URL before loading items.');
  const query = new URLSearchParams({ order: 'asc', limit: '30' });
  if (more && itemCursor) query.set('after', itemCursor);
  const result = await jsonRequest(settings, `/v1/conversations/${encodeURIComponent(conversationId)}/items?${query}`, 'GET', undefined, signal);
  if (!Array.isArray(result.data)) throw new Error('The API did not return a conversation item list.');
  if (!more) $('conversation-items').replaceChildren();
  for (const item of result.data) {
    const card = node('article', undefined, 'conversation-item');
    card.append(node('strong', item.role || item.type));
    const text = typeof item.content === 'string' ? item.content : Array.isArray(item.content) ? item.content.map(part => part.text || part.refusal || '').join('\n') : item.encrypted_content;
    if (text) card.append(renderMarkdown(text));
    else { const details = node('details'); details.append(node('summary', item.name || item.id || 'Tool item'), node('pre', JSON.stringify(redact(item), null, 2))); card.append(details); }
    $('conversation-items').append(card);
  }
  if (!$('conversation-items').children.length) $('conversation-items').append(node('p', 'No stored items yet.', 'hint'));
  itemCursor = result.last_id || result.data.at(-1)?.id;
  $('more-items').hidden = !result.has_more || !itemCursor;
}

async function conversationAction(action) {
  if (controller) return;
  const settings = config(); controller = new AbortController(); busy(true); notice('');
  try {
    if (action === 'create' || action === 'load') {
      const id = $('conversation-id').value.trim();
      if (action === 'load' && !id) throw new Error('Enter a conversation ID to load.');
      const result = await jsonRequest(settings, action === 'create' ? '/v1/conversations' : `/v1/conversations/${encodeURIComponent(id)}`, action === 'create' ? 'POST' : 'GET', action === 'create' ? {} : undefined, controller.signal);
      if (!result.id) throw new Error('The API did not return a conversation ID.');
      // Keep the previous selection until the new conversation has been accepted.
      run = createRun(); history = []; chatTurns = []; renderChat(); currentResponses = []; previousResponseId = null; lastRequest = null;
      conversationId = result.id; conversationBase = settings.base; sessionConfig = settings; checkpointStored = true;
      $('conversation-id').value = conversationId; $('conversation-status').textContent = `Loaded ${conversationId}`;
      $('run-status').textContent = 'Ready'; $('elapsed').textContent = '—'; $('continuation').textContent = 'Send a prompt to continue this conversation.';
      $('activity').replaceChildren(node('p', 'Conversation ready. Send a prompt.', 'placeholder')); compactionNotice.hidden = true;
    }
    await refreshConversationItems(settings, action === 'more', controller.signal);
  } catch (error) { notice(error.message); }
  finally { controller = null; busy(false); render(); renderUsage(); preview(); }
}
$('create-conversation').addEventListener('click', () => conversationAction('create'));
$('load-conversation').addEventListener('click', () => conversationAction('load'));
$('refresh-items').addEventListener('click', () => conversationAction('refresh'));
$('more-items').addEventListener('click', () => conversationAction('more'));

async function execute(followup = null) {
  if (controller) return;
  let settings;
  let request;
  try {
    settings = config();
    validateSession(settings);
    if (settings.fileSearch && !settings.vectorStoreIds.trim()) {
      if (!await settingsUI.loadFileSearchDemo()) return;
      settings = config();
    }
    request = buildRequest(followup === null ? settings : { ...settings, mode: 'prompt', mission: followup });
    request = withContinuation(request, continuation());
    if (!Number.isInteger(settings.maxContinuations) || settings.maxContinuations < 1 || settings.maxContinuations > 50) throw new Error('Client-tool continuation limit must be between 1 and 50.');
  } catch (error) { notice(error.message); return; }
  const chatTurn = { prompt: request.input, status: 'running', multiAgent: settings.multiAgent };
  chatTurns.push(chatTurn);
  if (followup !== null) $('followup').value = '';
  renderChat(true);
  compactionNotice.hidden = true;
  let parentStored = checkpointStored;
  const requests = [];
  currentResponses = [];
  sessionConfig = settings;
  lastRequest = request;
  run = createRun();
  run.singleAgent = !settings.multiAgent;
  activeChatTurn = chatTurn;
  controller = new AbortController();
  started = performance.now();
  $('elapsed').textContent = '0s';
  timer = setInterval(() => { $('elapsed').textContent = `${Math.round((performance.now() - started) / 1000)}s`; }, 1000);
  $('request-label').textContent = settings.transport === 'websocket' ? 'Request · WebSocket response.create' : 'Request · POST /v1/responses';
  $('request-preview').textContent = JSON.stringify(redact(request), null, 2);
  $('run-status').textContent = 'Running';
  $('continuation').textContent = 'The gateway is coordinating this turn.';
  $('agent-filter').replaceChildren(new Option('All agents', 'all'));
  notice('');
  busy(true);
  render();
  renderUsage();
  if (followup === null) document.querySelector('.results').scrollIntoView({ block: 'start' });
  try {
    const turnSignal = controller.signal;
    const resolved = new Set();
    const injections = [];
    let injectionCount = 0;
    async function executeBrowserCall(call) {
      if (turnSignal.aborted) throw new DOMException('Aborted', 'AbortError');
      const owner = call.agent?.agent_name || '/root';
      if (!run.progress.has(owner)) run.progress.set(owner, new Map());
      const steps = run.progress.get(owner);
      const label = `Running client tool · ${call.name || 'tool search'}`;
      steps.set(call.call_id, { id: call.call_id, label, status: 'active', eventType: 'client execution · browser' });
      scheduleRender();
      const output = await executeClientCall(call, settings, executeHandler, turnSignal);
      let failed = false;
      try { failed = Boolean(JSON.parse(output.output)?.error); } catch { /* Non-string outputs have a typed shape. */ }
      steps.set(call.call_id, { id: call.call_id, label, status: failed ? 'failed' : 'done', eventType: 'client execution · browser' });
      const item = run.items.get(call.id);
      if (item) item.clientOutput = typeof output.output === 'string' ? output.output : JSON.stringify(output.output || output.tools);
      resolved.add(call.call_id); scheduleRender();
      return output;
    }
    const liveClient = settings.transport === 'websocket' && settings.multiAgent ? {
      execute: clientQueue(settings.parallel ? settings.clientParallel : 1, call => {
        if (++injectionCount > settings.maxContinuations) throw new Error('Live client-tool injection limit reached.');
        return executeBrowserCall(call);
      }),
      record: frame => injections.push(frame),
    } : undefined;
    run.injections = injections;
    for (let round = 0; ; round++) {
      run.status = 'streaming'; run.response = null;
      requests.push(request);
      $('request-preview').textContent = JSON.stringify(redact(request), null, 2);
      await transports[activeApi].send(settings, request, event => { applyEvent(run, event); scheduleRender(); }, controller.signal, parentStored, liveClient);
      parentStored = request.store;
      currentResponses.push(run.response);
      renderUsage();
      if (run.status !== 'completed') throw new Error(run.response?.error?.message || `Response ${run.status}: ${run.response?.incomplete_details?.reason || 'inspect the debug view for details'}`);
      if (run.response?.output?.some(item => item.type === 'mcp_approval_request')) throw new Error('This gateway only supports MCP tools with require_approval: never.');
      if (!liveClient && pendingCalls(run.response).some(call => resolved.has(call.call_id))) throw new Error('The API repeated an already executed client call. Stopping to avoid duplicate execution.');
      const calls = pendingCalls(run.response, resolved);
      if (new Set(calls.map(call => call.call_id)).size !== calls.length) throw new Error('The API returned duplicate client call IDs. No pending calls were executed.');
      if (!calls.length) break;
      if (round >= settings.maxContinuations) throw new Error('Client-tool continuation limit reached. Pending calls were not executed. Start a new run with an appropriate limit.');
      if (!run.response?.id) throw new Error('The response has no ID for client-tool continuation.');
      if (!request.store && settings.transport !== 'websocket') throw new Error('Client-tool continuation over HTTP requires store:true. No client tools were executed. Start a new session with storage enabled or WebSocket transport.');
      const outputs = await mapLimited(calls, settings.parallel ? settings.clientParallel : 1, executeBrowserCall, controller.signal);
      request = withContinuation({ ...request, input: outputs }, { ...continuation(), previousResponseId: run.response.id });
    }
    previousResponseId = run.response?.id || null;
    checkpointStored = request.store;
    compactionNotice.hidden = true;
    $('run-status').textContent = 'Completed';
    chatTurn.status = 'completed'; chatTurn.answer = itemText(finalMessage() || {});
    $('continuation').textContent = previousResponseId ? `Response: ${previousResponseId}. The next prompt uses ${activeApi === 'conversations' ? 'the conversation ID' : 'this previous_response_id'} and your current settings.` : 'The API did not return a response ID.';
    if (activeApi === 'conversations') await refreshConversationItems(settings, false, controller.signal).catch(error => notice(`Turn saved; item refresh failed: ${error.message}`));
    const delegated = [...run.agents].some(agent => agent.startsWith('/root/'));
    if (!delegated && settings.multiAgent && settings.mode !== 'prompt') notice('The response completed, but no attributed subagent activity was observed. A delegation prompt does not guarantee delegation.');
  } catch (error) {
    const aborted = error.name === 'AbortError';
    controller.abort();
    $('run-status').textContent = aborted ? 'Disconnected' : 'Error';
    run.status = aborted ? 'disconnected' : 'error';
    chatTurn.status = run.status; chatTurn.error = aborted ? 'Disconnected before the final response.' : error.message;
    notice(aborted ? 'Browser stream disconnected. This does not confirm cancellation of gateway work. Start a new run when ready.' : `${error.message} Check gateway configuration; the demo does not substitute simulated tool outputs.`);
    $('continuation').textContent = previousResponseId ? 'The previous completed response ID remains selected. Start a new session to clear it.' : 'No completed continuation checkpoint. Start a new session to retry.';
  } finally {
    clearInterval(timer);
    if (currentResponses.length < requests.length) currentResponses.push(null);
    history.push({ kind: 'turn', requests, injections: run.injections || [], status: run.status, responses: currentResponses, usage: sumUsage(currentResponses), items: [...run.items.values()], eventCount: run.eventCount, eventTail: run.events });
    controller = null;
    busy(false);
    render();
    activeChatTurn = null;
    renderUsage();
    if (followup !== null && !$('followup').disabled) $('followup').focus({ preventScroll: true });
  }
}

compactButton.addEventListener('click', async () => {
  if (controller || !(previousResponseId || conversationId)) return;
  let settings, request;
  try { settings = config(); validateSession(settings); request = compactionRequest(settings, continuation()); }
  catch (error) { notice(error.message); return; }
  controller = new AbortController(); busy(true);
  compactButton.textContent = 'Compacting…'; notice('');
  const compactRun = createRun(); compactRun.singleAgent = !settings.multiAgent;
  try {
    $('request-preview').textContent = JSON.stringify(redact(request), null, 2);
    await transports[activeApi].send(settings, request, event => {
      applyEvent(compactRun, event);
      compactionNotice.hidden = false;
      compactionNotice.textContent = `Compacting context · ${event.type}`;
    }, controller.signal, checkpointStored);
    const result = compactRun.response;
    const checkpoint = result?.output?.find(item => item.type === 'compaction');
    if (compactRun.status !== 'completed' || !checkpoint || !result.id) throw new Error(result?.error?.message || 'The API did not return a completed compaction checkpoint.');
    previousResponseId = result.id;
    checkpointStored = request.store;
    sessionConfig = settings;
    history.push({ kind: 'compaction', request, response: result, usage: sumUsage([result]), eventTail: compactRun.events });
    compactionNotice.replaceChildren(node('strong', 'Context compacted'), node('p', `Summary call: ${result.usage?.input_tokens ?? 'unavailable'} input tokens · ${result.usage?.output_tokens ?? 'unavailable'} output tokens. Next prompt continues from ${result.id}.`, 'hint'));
    const details = node('details'); details.append(node('summary', 'View compaction checkpoint'), renderMarkdown(checkpoint.encrypted_content)); compactionNotice.append(details);
    $('continuation').textContent = 'Compaction completed. The next prompt continues the saved context and agent tree.';
    if (activeApi === 'conversations') await refreshConversationItems(settings, false, controller.signal).catch(error => notice(`Turn saved; item refresh failed: ${error.message}`));
  } catch (error) {
    compactionNotice.hidden = true;
    notice(`Compaction failed: ${error.message}. No replacement checkpoint was accepted. A disconnected unstored WebSocket session must be restarted.`);
  } finally { controller = null; compactButton.textContent = 'Compact conversation'; busy(false); renderUsage(); }
});

$('run').addEventListener('click', () => execute());
$('disconnect').addEventListener('click', () => controller?.abort());
$('followup-form').addEventListener('submit', event => { event.preventDefault(); if ((previousResponseId || conversationId) && $('followup').value.trim()) execute($('followup').value); });
$('followup').addEventListener('keydown', event => {
  if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
    event.preventDefault();
    if (!$('send').disabled) $('followup-form').requestSubmit();
  }
});
$('add').addEventListener('click', () => { addTask(); preview(); });
$('example').addEventListener('click', () => {
  if (mode.value === 'tasks') { $('tasks').replaceChildren(); sampleTasks.forEach(addTask); }
  setMode();
});
$('agent-filter').addEventListener('change', render);
$('events-preview').parentElement.addEventListener('toggle', () => { if (view === 'api') render(); });
for (const id of ['base', 'model', 'search', 'code', 'concurrency']) $(id).addEventListener('input', preview);
const tabs = [...document.querySelectorAll('[data-view]')];
for (const [index, tab] of tabs.entries()) {
  tab.id = `tab-${tab.dataset.view}`;
  tab.tabIndex = index === 0 ? 0 : -1;
  tab.setAttribute('aria-controls', tab.dataset.view);
  $(tab.dataset.view).setAttribute('aria-labelledby', tab.id);
  tab.addEventListener('click', () => {
    view = tab.dataset.view;
    for (const button of tabs) {
      button.setAttribute('aria-selected', String(button === tab));
      button.tabIndex = button === tab ? 0 : -1;
    }
    for (const id of ['activity', 'answer', 'api']) $(id).hidden = id !== view;
    render();
  });
  tab.addEventListener('keydown', event => {
    const offset = { ArrowRight: 1, ArrowLeft: -1, Home: -index, End: tabs.length - 1 - index }[event.key];
    if (offset === undefined) return;
    event.preventDefault();
    const target = tabs[(index + offset + tabs.length) % tabs.length];
    target.click();
    target.focus();
  });
}
$('download').addEventListener('click', () => {
  const url = URL.createObjectURL(new Blob([JSON.stringify(redact({ turns: history }), null, 2)], { type: 'application/json' }));
  const anchor = node('a');
  anchor.href = url;
  anchor.download = 'agentic-api-playground-run.json';
  anchor.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
});
sampleTasks.forEach(addTask);
setMode();
preview();
render();
renderUsage();

updateSessionHint();
