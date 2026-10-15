import test from 'node:test';
import assert from 'node:assert/strict';
import { apiBase, buildRequest, consumeSSE, createRun, applyEvent, safeLink, itemText, comparisonPrompt } from './protocol.mjs';

const settings = { model: 'served-model', tasks: [{ problem: 'Fix sorting.' }, { problem: 'Test parsing.' }], search: true, code: true, concurrency: 2 };
const terminal = { type: 'response.completed', response: { id: 'resp_test', status: 'completed', output: [] } };
function stream(text, size = 1) {
  const bytes = new TextEncoder().encode(text);
  let offset = 0;
  return new ReadableStream({ pull(controller) {
    if (offset >= bytes.length) { controller.close(); return; }
    controller.enqueue(bytes.slice(offset, offset + size));
    offset += size;
  } });
}
const encode = event => `data: ${JSON.stringify(event)}\r\n\r\n`;

test('a single comparison prompt requests independent solvers and an evidence-based judge', () => {
  const request = buildRequest({ ...settings, mode: 'mission', mission: comparisonPrompt, tasks: [] });
  assert.equal(request.input, comparisonPrompt);
  assert.match(request.instructions, /identical problem/);
  assert.match(request.instructions, /solver_a and solver_b/);
  assert.match(request.instructions, /SAME additional edge cases/);
  assert.match(request.instructions, /total out of 40/);
  assert.throws(() => buildRequest({ ...settings, mode: 'mission', mission: ' ' }));
});

test('agent spawn requests do not invent an agent; successful API outputs confirm it', () => {
  const run = createRun();
  applyEvent(run, { type: 'response.output_item.done', item: { id: 'spawn1', type: 'multi_agent_call', action: 'spawn_agent', agent: { agent_name: '/root' }, arguments: '{"task_name":"solver_a"}' } });
  assert.deepEqual([...run.agents], ['/root']);
  assert.equal(run.activity.get('/root'), 'Requesting a subagent');
  applyEvent(run, { type: 'response.output_item.done', item: { id: 'result1', type: 'multi_agent_call_output', action: 'spawn_agent', output: [{ type: 'output_text', text: '{"error":"capacity exceeded","task_name":"/root/solver_a"}' }] } });
  assert.equal(run.agents.has('/root/solver_a'), false);
  applyEvent(run, { type: 'response.output_item.done', item: { id: 'result2', type: 'multi_agent_call_output', action: 'spawn_agent', output: [{ type: 'output_text', text: '{"task_name":"/root/solver_a"}' }] } });
  assert.equal(run.activity.get('/root/solver_a'), 'Spawn confirmed');
  applyEvent(run, { type: 'response.output_item.added', agent: { agent_name: '/root/solver_a' }, item: { id: 'search1', type: 'web_search_call', status: 'in_progress' } });
  assert.equal(run.activity.get('/root/solver_a'), 'Web search: in_progress');
});

test('one request delegates tasks and declares actual gateway tools', () => {
  const request = buildRequest(settings);
  assert.equal(request.stream, true);
  assert.equal(request.store, true);
  assert.deepEqual(request.multi_agent, { enabled: true, max_concurrent_subagents: 2 });
  assert.deepEqual(request.tools, [{ type: 'web_search' }, { type: 'code_interpreter', container: { type: 'auto' } }]);
  assert.match(request.input, /task_name: agent_1/);
  assert.match(request.input, /task_name: agent_2/);
  assert.equal('previous_response_id' in request, false);
});

test('continuation sends only new input and saved response ID', () => {
  const request = buildRequest({ ...settings, previousResponseId: 'resp_1', followup: 'Test Unicode too.' });
  assert.equal(request.previous_response_id, 'resp_1');
  assert.equal(request.input, 'Test Unicode too.');
  assert.equal(request.tools.length, 2);
});

test('tool opt-outs change both declarations and instructions', () => {
  const request = buildRequest({ ...settings, code: false, search: false });
  assert.deepEqual(request.tools, []);
  assert.match(request.instructions, /unexecuted/);
  assert.match(request.instructions, /Web search is disabled/);
});

test('invalid configuration never produces a request', () => {
  for (const change of [{ model: '' }, { tasks: [] }, { tasks: [{ problem: ' ' }] }, { concurrency: 0 }, { concurrency: 2.5 }, { concurrency: 9 }, { previousResponseId: 'resp_1', followup: '' }]) {
    assert.throws(() => buildRequest({ ...settings, ...change }));
  }
});

test('gateway URL normalization does not duplicate v1 or accept credentials', () => {
  assert.equal(apiBase('http://localhost:9000/v1/'), 'http://localhost:9000');
  assert.equal(apiBase('https://host.example/gateway/'), 'https://host.example/gateway');
  for (const url of ['file:///tmp', 'https://user:secret@example.com', 'https://example.com?key=x']) assert.throws(() => apiBase(url));
});

test('SSE handles split UTF-8, split CRLF, comments, event names and DONE', async () => {
  const event = { type: 'response.output_text.delta', item_id: 'msg_1', delta: 'héllo 世界' };
  const received = [];
  await consumeSSE(stream(': keepalive\r\nevent: response.output_text.delta\r\n' + encode(event) + encode(terminal) + 'data: [DONE]\n\n'), value => received.push(value));
  assert.deepEqual(received, [event, terminal]);
});

test('SSE supports multiline data and final event without trailing separator', async () => {
  const received = [];
  const body = 'data: {"type":"response.completed",\ndata: "response":{"id":"resp_1","output":[]}}';
  await consumeSSE(stream(body, 11), value => received.push(value));
  assert.equal(received[0].response.id, 'resp_1');
});

test('truncated streams and malformed JSON are not reported as completed', async () => {
  await assert.rejects(consumeSSE(stream('data: {"type":"response.created"}\n\n'), () => {}), /disconnected/);
  await assert.rejects(consumeSSE(stream('data: broken\n\n'), () => {}), SyntaxError);
  await assert.rejects(consumeSSE(stream('data: [DONE]\n\n'), () => {}), /disconnected/);
});

test('API error events surface their diagnostics', async () => {
  await assert.rejects(consumeSSE(stream(encode({ type: 'error', message: 'Interpreter unavailable' })), () => {}), /Interpreter unavailable/);
});

test('failed and incomplete terminal events preserve their actual status', async () => {
  for (const status of ['failed', 'incomplete']) {
    const run = createRun();
    await consumeSSE(stream(encode({ type: `response.${status}`, response: { status, output: [] } })), event => applyEvent(run, event));
    assert.equal(run.status, status);
  }
});

test('interleaved agents are attributed by item, and authoritative snapshots replace deltas', () => {
  const run = createRun();
  for (const id of ['a', 'b']) applyEvent(run, { type: 'response.output_item.added', agent: { agent_name: `/root/${id}` }, item: { id, type: 'message', content: [] } });
  for (const [id, delta] of [['a', 'hello'], ['b', 'second'], ['a', ' world']]) applyEvent(run, { type: 'response.output_text.delta', item_id: id, delta });
  assert.equal(itemText(run.items.get('a')), 'hello world');
  assert.equal(itemText(run.items.get('b')), 'second');
  applyEvent(run, { type: 'response.output_item.done', item: { id: 'a', type: 'message', content: [{ type: 'output_text', text: 'hello world' }] } });
  applyEvent(run, { ...terminal, response: { ...terminal.response, output: [{ id: 'a', type: 'message', content: [{ type: 'output_text', text: 'final text' }] }] } });
  assert.equal(run.items.size, 2);
  assert.equal(itemText(run.items.get('a')), 'final text');
  assert.equal(run.items.get('a').agent.agent_name, '/root/a');
  assert.deepEqual([...run.agents].sort(), ['/root/a', '/root/b']);
});

test('Python call status and actual logs come from API items', () => {
  const run = createRun();
  applyEvent(run, { type: 'response.output_item.added', agent: { agent_name: '/root/test' }, item: { id: 'ci_1', type: 'code_interpreter_call', code: '', status: 'in_progress', outputs: null } });
  applyEvent(run, { type: 'response.code_interpreter_call_code.delta', item_id: 'ci_1', delta: 'print(1)' });
  applyEvent(run, { type: 'response.code_interpreter_call.interpreting', item_id: 'ci_1' });
  assert.equal(run.items.get('ci_1').status, 'interpreting');
  applyEvent(run, { type: 'response.output_item.done', item: { id: 'ci_1', type: 'code_interpreter_call', code: 'print(1)', status: 'completed', outputs: [{ type: 'logs', logs: '1\n' }] } });
  assert.equal(run.items.get('ci_1').code, 'print(1)');
  assert.equal(run.items.get('ci_1').outputs[0].logs, '1\n');
  assert.equal(run.items.get('ci_1').agent.agent_name, '/root/test');
});

test('diagnostic event history is bounded', () => {
  const run = createRun();
  for (let i = 0; i < 400; i++) applyEvent(run, { type: 'response.created', sequence_number: i });
  assert.equal(run.eventCount, 400);
  assert.equal(run.events.length, 250);
  assert.equal(run.events[0].sequence_number, 150);
});

test('source URLs cannot execute script or open local files', () => {
  assert.equal(safeLink('javascript:alert(1)'), null);
  assert.equal(safeLink('file:///etc/passwd'), null);
  assert.equal(safeLink('https://docs.python.org/3/'), 'https://docs.python.org/3/');
});
