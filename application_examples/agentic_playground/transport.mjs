import { consumeSSE } from './protocol.mjs';

export function authHeaders(settings) {
  return { 'Content-Type': 'application/json', ...(settings.token ? { Authorization: `Bearer ${settings.token}` } : {}) };
}

export async function checkResponse(response) {
  if (!response.ok) {
    const body = await response.text();
    let message = body;
    try { const parsed = JSON.parse(body); message = parsed.error?.message || parsed.message || body; } catch { /* plain HTTP error */ }
    throw new Error(`HTTP ${response.status}: ${String(message).slice(0, 1500)}`);
  }
  return response;
}

export async function jsonRequest(settings, path, method = 'GET', body, signal) {
  const response = await checkResponse(await fetch(`${settings.base}${path}`, {
    method, headers: authHeaders(settings), ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal,
  }));
  return response.json();
}

// One serial lane stays open across prompts, including store:false continuations.
export function createResponsesTransport() {
  let socket = null, endpoint = null, retainedId = null;
  function close() { socket?.close(); socket = null; endpoint = null; retainedId = null; }
  async function send(settings, request, onEvent, signal, parentStored = true, liveClient) {
    if (signal?.aborted) throw new DOMException('Aborted', 'AbortError');
    if (settings.transport !== 'websocket') {
      if (request.previous_response_id && !parentStored) throw new Error('HTTP cannot continue an unstored response ID. Start a new session with store:true, or use WebSocket from the first prompt.');
      const response = await checkResponse(await fetch(`${settings.base}/v1/responses`, {
        method: 'POST', headers: authHeaders(settings), body: JSON.stringify(request), signal,
      }));
      if (!response.headers.get('content-type')?.includes('text/event-stream')) throw new Error('Expected an SSE response (text/event-stream).');
      await consumeSSE(response.body, onEvent);
      return;
    }
    if (settings.token) throw new Error('Browser WebSockets cannot set the gateway’s Authorization header. Use HTTP for bearer-authenticated gateways, or a trusted same-origin proxy that supplies authentication.');
    const url = new URL(`${settings.base}/v1/responses`);
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    if (request.previous_response_id && !parentStored && (socket?.readyState !== WebSocket.OPEN || endpoint !== url.href || retainedId !== request.previous_response_id)) {
      throw new Error('The WebSocket holding this unstored response is no longer available. Start a new session; the ID cannot be restored after reconnecting.');
    }
    if (endpoint !== url.href || socket?.readyState !== WebSocket.OPEN) {
      close(); endpoint = url.href; socket = new WebSocket(endpoint);
    }
    const connection = socket;
    await new Promise((resolve, reject) => {
      let settled = false, responseId = null, pendingExecutions = 0;
      const liveCalls = new Set();
      const timeout = connection.readyState === WebSocket.OPEN ? null : setTimeout(() => finish(new Error('WebSocket connection timed out.')), 15000);
      function cleanup() {
        clearTimeout(timeout);
        signal?.removeEventListener('abort', abort);
        for (const [name, listener] of [['open', open], ['message', message], ['error', error], ['close', closed]]) connection.removeEventListener(name, listener);
      }
      function finish(failure) {
        if (settled) return;
        settled = true; cleanup();
        if (failure) { close(); reject(failure); } else resolve();
      }
      function open() {
        clearTimeout(timeout);
        connection.send(JSON.stringify({ ...request, type: 'response.create', stream_id: 'playground' }));
      }
      function message(event) {
        try {
          const data = JSON.parse(event.data);
          if (data.stream_id && data.stream_id !== 'playground') return;
          onEvent(data);
          if (data.type === 'response.created') responseId = data.response?.id;
          if (data.type === 'response.inject.failed') return finish(new Error(`Client output injection failed: ${data.error?.message || 'output was not accepted'}`));
          if (liveClient && data.type === 'response.output_item.done' && ['function_call', 'custom_tool_call', 'shell_call', 'tool_search_call'].includes(data.item?.type)) {
            if (!responseId || !data.item.call_id || liveCalls.has(data.item.call_id)) return finish(new Error('Invalid or repeated live client call.'));
            liveCalls.add(data.item.call_id);
            pendingExecutions++;
            liveClient.execute({ ...data.item, agent: data.item.agent || data.agent }).then(output => {
              pendingExecutions--;
              if (settled) return;
              const frame = { type: 'response.inject', response_id: responseId, input: [output] };
              liveClient.record(frame);
              connection.send(JSON.stringify(frame));
            }).catch(finish);
          }
          if (data.type === 'error') return finish(new Error(data.error?.message || data.message || 'WebSocket request failed.'));
          if (['response.completed', 'response.failed', 'response.incomplete'].includes(data.type)) {
            if (pendingExecutions) return finish(new Error('The WebSocket response ended while client tools were still executing.'));
            retainedId = data.type === 'response.completed' ? data.response?.id : null;
            finish();
          }
        } catch (failure) { finish(failure); }
      }
      function error() { finish(new Error('WebSocket connection failed. Check the gateway URL, authentication, and proxy upgrade support.')); }
      function closed() { finish(new Error('WebSocket closed before a terminal response event.')); }
      function abort() { finish(new DOMException('Aborted', 'AbortError')); }
      for (const [name, listener] of [['open', open], ['message', message], ['error', error], ['close', closed]]) connection.addEventListener(name, listener);
      signal?.addEventListener('abort', abort, { once: true });
      if (connection.readyState === WebSocket.OPEN) open();
    });
  }
  return { send, close };
}
