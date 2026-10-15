import { toolDeclarations } from './settings.mjs';

export const clientKinds = ['function_call', 'custom_tool_call', 'shell_call', 'tool_search_call'];

export function pendingCalls(response, resolved = new Set()) {
  return (response?.output || []).filter(item => clientKinds.includes(item.type) && item.call_id && !resolved.has(item.call_id));
}

export async function executeClientCall(call, settings, executeHandler, shellOutput, signal) {
  if (call.type === 'shell_call') return shellOutput(call, signal);
  if (call.type === 'tool_search_call') {
    // Discovery returns public declarations unchanged; the gateway validates identity and activates them.
    const tools = toolDeclarations(settings).filter(tool => tool.defer_loading
      || (tool.type === 'namespace' && tool.tools.some(member => member.defer_loading)));
    return { type: 'tool_search_output', call_id: call.call_id, execution: 'client', status: 'completed', tools };
  }
  const fn = (settings.functions || []).find(fn => fn.name === call.name && (fn.namespace || '') === (call.namespace || '') && (fn.kind || 'function') === (call.type === 'custom_tool_call' ? 'custom' : 'function'));
  let output;
  try {
    if (!fn) throw new Error(`No matching client handler for ${call.namespace ? call.namespace + '.' : ''}${call.name}.`);
    const args = call.type === 'custom_tool_call' ? call.input : JSON.parse(call.arguments);
    output = await executeHandler(fn.handler, args, signal);
  } catch (error) {
    if (error.name === 'AbortError') throw error;
    output = JSON.stringify({ error: error.message });
  }
  return { type: call.type === 'custom_tool_call' ? 'custom_tool_call_output' : 'function_call_output', call_id: call.call_id, output };
}

export async function mapLimited(values, limit, fn, signal) {
  if (!Number.isInteger(limit) || limit < 1 || limit > 8) throw new Error('Browser function concurrency must be between 1 and 8.');
  const results = new Array(values.length);
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(limit, values.length) }, async () => {
    while (next < values.length) {
      if (signal?.aborted) throw new DOMException('Aborted', 'AbortError');
      const index = next++;
      results[index] = await fn(values[index]);
    }
  }));
  return results;
}

export function clientQueue(limit, execute) {
  if (!Number.isInteger(limit) || limit < 1 || limit > 8) throw new Error('Browser function concurrency must be between 1 and 8.');
  const lanes = Array.from({ length: limit }, () => Promise.resolve());
  let next = 0;
  return call => {
    const index = next++ % limit;
    const result = lanes[index].then(() => execute(call));
    // Keep rejected jobs observed without blocking cleanup of queued jobs.
    lanes[index] = result.catch(() => {});
    return result;
  };
}

export function sumUsage(responses) {
  const fields = ['input_tokens', 'output_tokens', 'total_tokens'];
  const result = { reportedResponses: responses.filter(r => r?.usage).length, responseCount: responses.length };
  for (const field of fields) {
    result[field] = responses.length && responses.every(r => Number.isFinite(r?.usage?.[field])) ? responses.reduce((n, r) => n + r.usage[field], 0) : null;
  }
  return result;
}

export function withContinuation(request, { apiMode, conversationId, previousResponseId }) {
  const next = { ...request };
  delete next.conversation;
  delete next.previous_response_id;
  if (apiMode === 'conversations') {
    if (!conversationId) throw new Error('Create or load a conversation first.');
    next.conversation = conversationId;
    next.store = true;
  } else if (previousResponseId) next.previous_response_id = previousResponseId;
  return next;
}

export function compactionRequest(settings, continuation) {
  return withContinuation({ model: settings.model, stream: true, store: settings.store ?? true, input: [{ type: 'compaction_trigger' }] }, continuation);
}
