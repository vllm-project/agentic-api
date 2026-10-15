import test from 'node:test';
import assert from 'node:assert/strict';
import { buildRequest, createRun, applyEvent } from './protocol.mjs';
import { toolDeclarations, responseOptions, redact, mcpPresets } from './settings.mjs';
import { executeClientCall, pendingCalls, sumUsage, mapLimited, withContinuation, compactionRequest } from './client-tools.mjs';

const fn = { name: 'sum', handler: 'return args.a + args.b;', schema: '{"type":"object"}', namespace: 'math' };

test('tool budget and parallel tool calling are independent request fields', () => {
  const request = buildRequest({ model: 'demo', mode: 'prompt', mission: 'Hello', concurrency: 2, multiAgent: false, parallel: false, maxCalls: 4 });
  assert.equal(request.parallel_tool_calls, false);
  assert.equal(request.max_tool_calls, 4);
  assert.throws(() => responseOptions({ multiAgent: true, maxCalls: 4 }), /multi-agent/);
  assert.throws(() => responseOptions({ multiAgent: true, summary: 'auto' }), /summaries/);
  assert.throws(() => responseOptions({ maxOutput: 'NaN' }), /max_output_tokens/);
});

test('MCP declarations use explicit hosts and allowed tools and redact credentials', () => {
  const tools = toolDeclarations({ mcp: [{ ...mcpPresets[0], authorization: 'secret', headers: '{"X-Key":"secret"}' }] });
  assert.equal(tools[0].server_url, 'https://learn.microsoft.com/api/mcp');
  assert.equal(tools[0].allowed_tools.length, 3);
  assert.equal(tools[0].require_approval, 'never');
  assert.equal(JSON.stringify(redact({ tools })).includes('secret'), false);
  assert.equal(tools[0].authorization, 'secret');
});

test('client namespaces route arguments to user-authored handlers and return typed outputs', async () => {
  const call = { type: 'function_call', call_id: 'c1', namespace: 'math', name: 'sum', arguments: '{"a":2,"b":3}' };
  assert.equal(toolDeclarations({ functions: [fn] })[0].type, 'namespace');
  const output = await executeClientCall(call, { functions: [fn] }, async (code, args) => {
    assert.equal(code, fn.handler); assert.deepEqual(args, { a: 2, b: 3 }); return '5';
  });
  assert.deepEqual(output, { type: 'function_call_output', call_id: 'c1', output: '5' });
  const failure = await executeClientCall({ ...call, arguments: '{' }, { functions: [fn] });
  assert.ok(JSON.parse(failure.output).error);
  assert.deepEqual(pendingCalls({ output: [call] }, new Set(['c1'])), []);
});

test('custom tools pass text, and discovery returns configured deferred definitions', async () => {
  const output = await executeClientCall({ type: 'custom_tool_call', call_id: 'c2', name: 'echo', input: 'plain text' }, { functions: [{ ...fn, namespace: '', kind: 'custom', name: 'echo' }] }, async (_, args) => JSON.stringify(args));
  assert.equal(output.type, 'custom_tool_call_output');
  assert.equal(output.output, '"plain text"');
  const discovery = await executeClientCall({ type: 'tool_search_call', call_id: 'find' }, { functions: [{ ...fn, deferred: true }], toolSearch: true });
  assert.equal(discovery.tools[0].tools[0].name, 'sum');
  assert.equal(discovery.tools[0].tools[0].defer_loading, true);
  assert.throws(() => toolDeclarations({ functions: [{ ...fn, deferred: true }] }), /discovery/);
});

test('discovery preserves complete declared functions and mixed namespaces without loading unrelated tools', async () => {
  const settings = {
    model: 'demo', mode: 'prompt', mission: 'Find a tool.', concurrency: 1, multiAgent: false,
    toolSearch: true, shell: true,
    functions: [
      { ...fn, namespace: '', name: 'deferred_sum', deferred: true, strict: true, description: 'Add two numbers.' },
      { ...fn, namespace: '', name: 'available_sum' },
      { ...fn, deferred: true, strict: true, description: 'Namespaced sum.' },
      { ...fn, name: 'available_member' },
      { ...fn, namespace: 'other', name: 'unrelated' },
    ],
  };
  const declared = buildRequest(settings).tools;
  const before = structuredClone(settings);
  const output = await executeClientCall({ type: 'tool_search_call', call_id: 'search1' }, settings);
  assert.deepEqual(output, {
    type: 'tool_search_output', call_id: 'search1', execution: 'client', status: 'completed',
    tools: [declared.find(tool => tool.name === 'deferred_sum'), declared.find(tool => tool.name === 'math')],
  });
  assert.equal(output.tools[0].defer_loading, true);
  assert.equal(output.tools[1].tools[0].defer_loading, true);
  assert.equal(output.tools[1].tools[1].name, 'available_member');
  assert.deepEqual(settings, before);
  const empty = await executeClientCall({ type: 'tool_search_call', call_id: 'search2' }, { toolSearch: true, functions: [fn] });
  assert.deepEqual(empty.tools, []);
});

test('browser concurrency is bounded and preserves output order', async () => {
  let running = 0, peak = 0;
  const outputs = await mapLimited([1, 2, 3, 4], 2, async value => {
    peak = Math.max(peak, ++running);
    await new Promise(resolve => setTimeout(resolve, 5));
    running--; return value * 2;
  });
  assert.equal(peak, 2);
  assert.deepEqual(outputs, [2, 4, 6, 8]);
});

test('turn usage sums continuations, preserves zeros, and never invents missing counts', () => {
  const usage = { input_tokens: 10, output_tokens: 0, total_tokens: 10 };
  assert.deepEqual(sumUsage([{ usage }, { usage }]), { reportedResponses: 2, responseCount: 2, input_tokens: 20, output_tokens: 0, total_tokens: 20 });
  assert.equal(sumUsage([{ usage }, null]).total_tokens, null);
  assert.equal(sumUsage([]).total_tokens, null);
});

test('compaction retains storage and continues the returned response ID', () => {
  const request = compactionRequest({ model: 'demo', store: true }, { apiMode: 'responses', previousResponseId: 'tree' });
  assert.equal(request.store, true);
  assert.equal(request.previous_response_id, 'tree');
  assert.deepEqual(request.input, [{ type: 'compaction_trigger' }]);
  const next = withContinuation({ model: 'demo', input: 'continue' }, { apiMode: 'responses', previousResponseId: 'compacted' });
  assert.equal(next.previous_response_id, 'compacted');
  assert.equal(next.input, 'continue');
  assert.deepEqual(responseOptions({ compactThreshold: 5000 }).context_management, [{ type: 'compaction', compact_threshold: 5000 }]);
});

test('Conversations and Responses references are mutually exclusive', () => {
  const request = withContinuation({ previous_response_id: 'stale', store: false, input: 'hello' }, { apiMode: 'conversations', conversationId: 'conv1', previousResponseId: 'ignored' });
  assert.equal(request.previous_response_id, undefined);
  assert.equal(request.conversation, 'conv1');
  assert.equal(request.store, true);
  assert.throws(() => withContinuation({}, { apiMode: 'conversations' }), /Create or load/);
  const response = withContinuation({ conversation: 'stale' }, { apiMode: 'responses', previousResponseId: 'r1' });
  assert.equal(response.conversation, undefined);
  assert.equal(response.previous_response_id, 'r1');
});

test('storage defaults true and store:false requires a single-agent request', () => {
  const settings = { model: 'demo', mode: 'prompt', mission: 'hello', concurrency: 2 };
  assert.equal(buildRequest(settings).store, true);
  assert.equal(buildRequest({ ...settings, multiAgent: false, store: false }).store, false);
  assert.throws(() => buildRequest({ ...settings, store: false }), /store:true/);
});

test('single-agent SSE attribution, MCP failure, and exact event names drive progress', () => {
  const run = createRun(); run.singleAgent = true;
  applyEvent(run, { type: 'response.output_item.added', item: { id: 'm1', type: 'mcp_call', name: 'search', server_label: 'learn', status: 'in_progress' } });
  applyEvent(run, { type: 'response.mcp_call.failed', item_id: 'm1' });
  assert.equal(run.progress.get('/root').get('m1').status, 'failed');
  assert.equal(run.progress.get('/root').get('m1').eventType, 'response.mcp_call.failed');
  applyEvent(run, { type: 'response.new_event', extra: 'retained' });
  assert.equal(run.eventTypes.get('/root').get('response.new_event'), 1);
  applyEvent(run, { type: 'response.completed', response: { output: [{ id: 'a1', type: 'message', content: [{ type: 'output_text', text: 'answer' }] }] } });
  assert.equal(run.items.get('a1').agent.agent_name, '/root');
  assert.equal(run.agents.has('/root'), true);
});

test('an incomplete terminal snapshot cannot mark an unfinished answer as done', () => {
  const run = createRun(); run.singleAgent = true;
  applyEvent(run, { type: 'response.incomplete', response: { output: [{ id: 'partial', type: 'message', status: 'incomplete', phase: 'final_answer', content: [] }] } });
  assert.equal(run.progress.get('/root').get('partial').status, 'failed');
});
