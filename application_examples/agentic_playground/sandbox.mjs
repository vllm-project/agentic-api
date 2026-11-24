// The user authors the handler. Model-generated arguments are data, never executable source.
function workerMain() {
  self.onmessage = async event => {
    try {
      const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
      const result = await new AsyncFunction('args', '"use strict";\n' + event.data.code)(event.data.args);
      const output = JSON.stringify(result ?? null);
      if (output.length > 65536) throw new Error('Function output exceeds 64 KiB.');
      self.postMessage({ output });
    } catch (error) { self.postMessage({ error: String(error.message || error).slice(0, 2000) }); }
  };
}

function frameMain(workerSource) {
  let worker, url, timeout;
  function clean() { clearTimeout(timeout); worker?.terminate(); if (url) URL.revokeObjectURL(url); }
  addEventListener('message', event => {
    if (event.source !== parent) return;
    if (event.data.cancel) { clean(); return; }
    const { id, code, args } = event.data;
    if (!id || worker) return;
    const send = result => { clean(); parent.postMessage({ id, ...result }, '*'); };
    try {
      url = URL.createObjectURL(new Blob([workerSource], { type: 'text/javascript' }));
      worker = new Worker(url);
      worker.onmessage = message => send(message.data);
      worker.onerror = () => send({ error: 'Client handler failed to initialize or execute.' });
      timeout = setTimeout(() => send({ error: 'Client handler exceeded the 5 second limit.' }), 5000);
      worker.postMessage({ code, args });
    } catch (error) { send({ error: String(error.message || error) }); }
  });
  parent.postMessage({ ready: true }, '*');
}

export function executeHandler(code, args, signal) {
  if (signal?.aborted) return Promise.reject(new DOMException('Aborted', 'AbortError'));
  if (code.length > 65536 || JSON.stringify(args).length > 65536) return Promise.reject(new Error('Handler or arguments exceed 64 KiB.'));
  return new Promise((resolve, reject) => {
    const id = crypto.randomUUID();
    const frame = document.createElement('iframe');
    frame.hidden = true;
    frame.title = 'Isolated client function';
    frame.setAttribute('sandbox', 'allow-scripts');
    frame.referrerPolicy = 'no-referrer';
    const workerSource = `(${workerMain.toString()})();`;
    frame.srcdoc = `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline' 'unsafe-eval' blob:; worker-src blob:; connect-src 'none'; img-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'"><script>(${frameMain.toString()})(${JSON.stringify(workerSource)});<\/script>`;
    let sent = false;
    function cleanup() {
      clearTimeout(timeout);
      window.removeEventListener('message', receive);
      signal?.removeEventListener('abort', abort);
      frame.contentWindow?.postMessage({ cancel: true }, '*');
      frame.remove();
    }
    function abort() { cleanup(); reject(new DOMException('Aborted', 'AbortError')); }
    function receive(event) {
      if (event.source !== frame.contentWindow) return;
      if (event.data?.ready && !sent) { sent = true; frame.contentWindow.postMessage({ id, code, args }, '*'); return; }
      if (event.data?.id !== id) return;
      cleanup();
      if (event.data.error) reject(new Error(String(event.data.error)));
      else if (typeof event.data.output !== 'string' || event.data.output.length > 65536) reject(new Error('Invalid or oversized handler output.'));
      else resolve(event.data.output);
    }
    const timeout = setTimeout(() => { cleanup(); reject(new Error('Client function sandbox timed out.')); }, 8000);
    window.addEventListener('message', receive);
    signal?.addEventListener('abort', abort, { once: true });
    document.body.append(frame);
  });
}
