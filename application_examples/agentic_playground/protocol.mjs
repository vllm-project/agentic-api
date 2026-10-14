// Browser-side Responses projection. The gateway owns delegation and built-in tool execution.
import { trackProgress } from './progress.mjs';
import { toolDeclarations, responseOptions } from './settings.mjs';
export function apiBase(value) {
  const url = new URL(value);
  if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
    throw new Error('Use an HTTP(S) API URL without credentials, a query, or a fragment.');
  }
  return url.href.replace(/\/+$/, '').replace(/\/v1$/, '');
}

export const comparisonPrompt = 'Search the web for a popular LeetCode algorithm problem. Pick one problem and give the exact same problem, constraints, and example inputs to two independent coding agents. Have each agent implement a Python solution, execute the examples and edge-case tests, inspect failures, and fix its code. After both agents finish, compare their solutions and rate which agent solved it better, with evidence from their actual test outputs.';

export function buildRequest(settings) {
  const { model, tasks = [], mission, mode = 'tasks', search, code, concurrency, previousResponseId, followup, multiAgent = true } = settings;
  if (!model.trim()) throw new Error('Enter or load a served model.');
  if (!Number.isInteger(concurrency) || concurrency < 1 || concurrency > 8) {
    throw new Error('Choose between 1 and 8 concurrent subagents.');
  }
  if (previousResponseId ? !followup?.trim() : mode !== 'tasks' ? !mission?.trim() : !tasks.length || tasks.some(t => !t.problem.trim())) {
    throw new Error('Give every agent a problem, or enter a follow-up.');
  }
  const tools = [];
  if (search) tools.push({ type: 'web_search' });
  if (code) tools.push({ type: 'code_interpreter', container: { type: 'auto' } });
  tools.push(...toolDeclarations(settings));
  if (multiAgent && settings.store === false) throw new Error('Multi-agent execution requires store:true. Disable multi-agent execution for a new store:false session.');
  const request = {
    model: model.trim(), stream: true, store: settings.store ?? true,
    multi_agent: { enabled: multiAgent, ...(multiAgent ? { max_concurrent_subagents: concurrency } : {}) },
    parallel_tool_calls: settings.parallel ?? true, tools,
    instructions: [
      mode === 'mission'
        ? 'You coordinate a coding comparison requested by the user. First research and choose ONE problem. State its source URL, exact specification, constraints, and shared examples before delegating. Spawn exactly two independent subagents named solver_a and solver_b, giving both the identical problem and examples. Neither should see the other solution until both have reported. Use collaboration tools to wait for both results. Then evaluate both solutions yourself on the SAME additional edge cases using code_interpreter when enabled. Return a comparison table scoring correctness (0–10), test coverage (0–10), time/space efficiency (0–10), and clarity (0–10), with a total out of 40 and a reasoned winner or tie. Ground scores in the submitted code and actual execution evidence. Distinguish observed test results from your assessment. Do not invent benchmark timings or claim an official popularity ranking without evidence. If one agent fails or execution is unavailable, report that limitation and do not invent a completed comparison.'
        : mode === 'tasks' ? 'You coordinate a team solving coding problems. Delegate each numbered task to a distinct subagent using its requested task_name. Collect their findings and give a concise combined final answer.' : 'Complete the user’s task using the available tools. Delegate separate work when the user asks for agents and multi-agent execution is enabled. Explain the result and cite evidence from tool outputs.',
      multiAgent ? 'Multi-agent execution is enabled.' : 'Multi-agent execution is disabled. Complete the task yourself; do not spawn agents.',
      'Give short public progress updates when choosing the problem, delegating, collecting results, and evaluating.',
      'On follow-up turns, continue with the existing team and findings; do not repeat the initial research and spawning unless the user asks for a fresh comparison.',
      'Each coding agent should reproduce the problem, implement a fix, run tests, inspect failures, and retry.',
      code ? 'Use code_interpreter for actual Python execution. Each call is a fresh sandbox: include all code and tests in every call and print results. No persistent files, shell, packages, or network are available in the interpreter.' : 'The Python interpreter is disabled. Only claim code execution when another available tool supplies actual execution evidence; otherwise label tests as unexecuted.',
      search ? 'Use web_search when documentation or research would help. Cite the sources actually consulted.' : 'Web search is disabled. Do not claim to have searched.',
      'Report the code, actual test outputs, and any unresolved problems. Do not claim that an execution succeeded without tool evidence.',
      settings.instructions || '',
    ].join(' '),
    input: previousResponseId ? followup.trim() : mode !== 'tasks' ? mission.trim() : 'Delegate these separate tasks:\n\n' + tasks.map((task, i) =>
      `${i + 1}. task_name: agent_${i + 1}\nProblem:\n${task.problem.trim()}`).join('\n\n'),
  };
  if (previousResponseId) request.previous_response_id = previousResponseId;
  Object.assign(request, responseOptions(settings));
  return request;
}

// SSE is framed independently of network reads; a read may split a UTF-8 character or CRLF.
export async function consumeSSE(body, onEvent) {
  if (!body) throw new Error('The API returned an empty response body.');
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let pending = '';
  let data = [];
  let dataLength = 0;
  let terminal = false;
  function line(value) {
    if (value === '') {
      if (!data.length) return;
      const payload = data.join('\n');
      data = [];
      dataLength = 0;
      if (payload === '[DONE]') return;
      const event = JSON.parse(payload);
      onEvent(event);
      if (['response.completed', 'response.failed', 'response.incomplete'].includes(event.type)) terminal = true;
      if (event.type === 'error') throw new Error(event.message || event.error?.message || 'The API reported a stream error.');
    } else if (value.startsWith('data:')) {
      dataLength += value.length;
      if (dataLength > 8 * 1024 * 1024) throw new Error('An SSE event exceeded the demo’s size limit.');
      data.push(value.slice(5).replace(/^ /, ''));
    }
  }
  function drain(final = false) {
    let match;
    while ((match = /\r\n|\r|\n/.exec(pending))) {
      if (!final && match[0] === '\r' && match.index === pending.length - 1) break;
      line(pending.slice(0, match.index));
      pending = pending.slice(match.index + match[0].length);
    }
    if (pending.length > 8 * 1024 * 1024) {
      throw new Error('An SSE event exceeded the demo’s size limit.');
    }
  }
  try {
    while (!terminal) {
      const { value, done } = await reader.read();
      pending += done ? decoder.decode() : decoder.decode(value, { stream: true });
      drain(done);
      if (done) {
        if (pending) line(pending);
        line('');
        break;
      }
    }
    if (!terminal) throw new Error('The stream disconnected before a terminal response event. Start a new run to retry.');
  } finally {
    await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

export function createRun() {
  return { items: new Map(), agents: new Set(), activity: new Map(), progress: new Map(), eventTypes: new Map(), events: [], eventCount: 0, response: null, status: 'streaming' };
}

function observe(run, agent, activity) {
  if (!agent) return;
  run.agents.add(agent);
  if (activity) run.activity.set(agent, activity);
}

function observeItem(run, item, fallbackAgent) {
  const agent = item.agent?.agent_name || fallbackAgent;
  if (item.type === 'multi_agent_call' && item.action === 'spawn_agent') observe(run, agent, 'Requesting a subagent');
  if (item.type === 'multi_agent_call_output' && item.action === 'spawn_agent') {
    for (const part of item.output || []) {
      try {
        const result = JSON.parse(part.text);
        if (!result.error && typeof result.task_name === 'string' && result.task_name.startsWith('/root/')) observe(run, result.task_name, 'Spawn confirmed');
      } catch { /* Opaque/non-JSON outputs do not confirm a spawn. */ }
    }
  }
  if (item.type === 'code_interpreter_call') observe(run, agent, `Python: ${item.status || 'requested'}`);
  if (item.type === 'web_search_call') observe(run, agent, `Web search: ${item.status || 'requested'}`);
  if (item.type === 'message') observe(run, agent, item.phase === 'final_answer' ? 'Answer reported' : 'Message received');
}

export function applyEvent(run, event) {
  run.eventCount++;
  // Keep a bounded diagnostic tail; output items have their own authoritative state.
  run.events.push(event);
  if (run.events.length > 250) run.events.shift();
  const embedded = event.item;
  const key = embedded?.id || event.item_id || (event.output_index != null ? `index:${event.output_index}` : null);
  let item = key ? run.items.get(key) : null;
  const attribution = embedded?.agent || event.agent || item?.agent || (run.singleAgent ? { agent_name: '/root' } : undefined);
  const agent = attribution?.agent_name;
  observe(run, agent);
  if (embedded && key) {
    item = { ...item, ...embedded, agent: attribution };
    run.items.set(key, item);
    observeItem(run, item, agent);
  }
  if (event.type === 'response.output_text.delta' && key) {
    item ||= { id: key, type: 'message', agent: attribution, content: [] };
    item.content ||= [];
    const index = event.content_index ?? 0;
    item.content[index] ||= { type: 'output_text', text: '' };
    item.content[index].text += event.delta;
    run.items.set(key, item);
    observe(run, agent, 'Writing');
  }
  if (event.type === 'response.code_interpreter_call_code.delta' && item) item.code = (item.code || '') + event.delta;
  if (event.type.startsWith('response.code_interpreter_call.') && item) item.status = event.type.split('.').at(-1);
  if (event.type.startsWith('response.web_search_call.') && item) item.status = event.type.split('.').at(-1);
  if (event.type.startsWith('response.code_interpreter_call.') || event.type.startsWith('response.web_search_call.')) {
    if (item) observeItem(run, item, agent);
  }
  if (/^response\.(mcp_call|mcp_list_tools)\.(in_progress|completed|failed)$/.test(event.type) && item) item.status = event.type.split('.').at(-1);
  trackProgress(run, event, item);
  if (['response.completed', 'response.failed', 'response.incomplete'].includes(event.type)) {
    run.response = event.response;
    run.status = event.type.slice('response.'.length);
    for (const finalItem of event.response?.output || []) {
      const existing = run.items.get(finalItem.id);
      const merged = { ...existing, ...finalItem, agent: finalItem.agent || existing?.agent || (run.singleAgent ? { agent_name: '/root' } : undefined) };
      run.items.set(finalItem.id, merged);
      observe(run, merged.agent?.agent_name);
      observeItem(run, merged);
      trackProgress(run, { type: 'snapshot' }, merged);
    }
  }
}

export function itemText(item) {
  return (item.content || []).filter(p => p?.type === 'output_text' || p?.type === 'refusal')
    .map(p => p.text || p.refusal || '').join('\n');
}

export function safeLink(value) {
  try { const url = new URL(value); return ['http:', 'https:'].includes(url.protocol) ? url.href : null; }
  catch { return null; }
}
