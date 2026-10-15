export const mcpPresets = [
  { name: 'Microsoft Learn', label: 'microsoft_learn', url: 'https://learn.microsoft.com/api/mcp', allowed: 'microsoft_docs_search, microsoft_docs_fetch, microsoft_code_sample_search', docs: 'https://learn.microsoft.com/en-us/training/support/mcp-developer-reference', description: 'Search documentation, fetch articles, and find code samples. No authentication required.', prompt: 'Use Microsoft Learn to search for Python examples for Azure Blob Storage. Fetch the relevant documentation and explain a working upload example with source links.' },
  { name: 'GitMCP · repository docs', label: 'gitmcp', url: 'https://gitmcp.io/docs', allowed: '', docs: 'https://github.com/idosal/git-mcp', description: 'Fetch and search public repository documentation and code. No authentication required.', prompt: 'Use GitMCP to search the fastapi/fastapi repository documentation for dependency injection. Fetch the relevant documentation and explain a minimal example.' },
  { name: 'Hugging Face', label: 'huggingface', url: 'https://huggingface.co/mcp', allowed: 'hf_fs', docs: 'https://huggingface.co/docs/hub/en/agents-mcp', description: 'Explore models, datasets, and documentation with hf_fs. Check your Hugging Face MCP settings for token requirements and available tools.', prompt: 'Use Hugging Face to find documentation about LoRA adapters with PEFT. Compare the documented approaches and cite the sources.' },
];

export function toolDeclarations(settings) {
  const tools = [];
  const labels = new Set();
  for (const server of settings.mcp || []) {
    const url = new URL(server.url);
    if (!['https:', 'http:'].includes(url.protocol) || url.username || url.password) throw new Error('MCP requires an HTTP(S) URL without embedded credentials.');
    if (!/^[A-Za-z0-9_-]+$/.test(server.label) || labels.has(server.label)) throw new Error('MCP labels must be unique names using letters, digits, underscores, or hyphens.');
    labels.add(server.label);
    const tool = { type: 'mcp', server_label: server.label, server_url: url.href, require_approval: 'never' };
    if (server.allowed.trim()) tool.allowed_tools = server.allowed.split(',').map(s => s.trim()).filter(Boolean);
    if (server.authorization) tool.authorization = server.authorization;
    if (server.headers?.trim()) {
      tool.headers = JSON.parse(server.headers);
      if (!tool.headers || Array.isArray(tool.headers) || typeof tool.headers !== 'object' || Object.values(tool.headers).some(v => typeof v !== 'string')) throw new Error('MCP headers must be a JSON object of string values.');
    }
    tools.push(tool);
  }
  const names = new Set();
  const namespaces = new Map();
  for (const fn of settings.functions || []) {
    if (!/^[A-Za-z_][A-Za-z0-9_-]*$/.test(fn.name)) throw new Error('Client functions need a valid name.');
    if (fn.namespace && !/^[A-Za-z_][A-Za-z0-9_-]*$/.test(fn.namespace)) throw new Error('Invalid namespace name.');
    const key = `${fn.namespace || ''}:${fn.name}`;
    if (names.has(key)) throw new Error(`Duplicate client tool: ${key}`);
    names.add(key);
    if (!fn.handler?.trim()) throw new Error(`Enter a JavaScript handler for ${fn.name}.`);
    const tool = { type: fn.kind || 'function', name: fn.name, description: fn.description };
    if (tool.type === 'function') {
      tool.parameters = JSON.parse(fn.schema);
      if (!tool.parameters || tool.parameters.type !== 'object') throw new Error(`${fn.name} needs an object JSON schema.`);
      tool.strict = Boolean(fn.strict);
    }
    if (fn.deferred) tool.defer_loading = true;
    if (fn.namespace) {
      if (tool.type !== 'function') throw new Error('This gateway supports only function tools inside namespaces.');
      if (!namespaces.has(fn.namespace)) namespaces.set(fn.namespace, { type: 'namespace', name: fn.namespace, tools: [] });
      namespaces.get(fn.namespace).tools.push(tool);
    } else tools.push(tool);
  }
  tools.push(...namespaces.values());
  if (settings.toolSearch) tools.push({ type: 'tool_search', execution: 'client', description: 'Find available client tools by their name or description.', parameters: { type: 'object', properties: { query: { type: 'string' } }, required: ['query'], additionalProperties: false } });
  if ((settings.functions || []).some(fn => fn.deferred) && !settings.toolSearch) throw new Error('Enable client tool discovery to use deferred functions.');
  if (settings.shell) tools.push({ type: 'shell', environment: { type: 'local' } });
  return tools;
}

export function responseOptions(settings) {
  const result = {};
  for (const [field, value, min, max, integer] of [
    ['max_output_tokens', settings.maxOutput, 1, 10000000, true],
    ['max_tool_calls', settings.maxCalls, 1, Number.MAX_SAFE_INTEGER, true],
    ['temperature', settings.temperature, 0, 2, false],
    ['top_p', settings.topP, 0, 1, false],
  ]) {
    if (value === '' || value == null) continue;
    const number = Number(value);
    if (!Number.isFinite(number) || number < min || number > max || (integer && !Number.isInteger(number))) throw new Error(`Invalid ${field}.`);
    result[field] = number;
  }
  if (result.max_tool_calls && settings.multiAgent !== false) throw new Error('This gateway does not support max_tool_calls with multi-agent execution. Disable multi-agent or leave the maximum empty.');
  if (settings.summary && settings.multiAgent !== false) throw new Error('Reasoning summaries are not supported with multi-agent execution.');
  if (settings.effort || settings.summary) result.reasoning = { ...(settings.effort ? { effort: settings.effort } : {}), ...(settings.summary ? { summary: settings.summary } : {}) };
  if (settings.toolChoice) result.tool_choice = settings.toolChoice;
  if (settings.compactThreshold) {
    const threshold = Number(settings.compactThreshold);
    if (!Number.isSafeInteger(threshold) || threshold < 1) throw new Error('Compaction threshold must be a positive integer.');
    result.context_management = [{ type: 'compaction', compact_threshold: threshold }];
  }
  if (settings.format) {
    result.text = { format: settings.format === 'json_schema' ? { type: 'json_schema', name: 'playground_output', strict: true, schema: JSON.parse(settings.outputSchema) } : { type: settings.format } };
  }
  return result;
}

// Redact transport credentials even when a response echoes tool declarations.
export function redact(value) {
  if (Array.isArray(value)) return value.map(redact);
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([key, entry]) => [key, /^(authorization|headers|token|api_key)$/i.test(key) ? '[redacted]' : redact(entry)]));
  return value;
}

export function createSettingsUI(parent, onChange, onPreset, onTest, notify) {
  const node = (tag, text, cls) => { const e = document.createElement(tag); if (text !== undefined) e.textContent = text; if (cls) e.className = cls; return e; };
  let sequence = 0;
  function section(title) { const e = node('details', undefined, 'settings-section'); e.append(node('summary', title)); parent.append(e); return e; }
  function field(container, title, value = '', type = 'text', choices) {
    const label = node('label', title);
    const input = node(choices ? 'select' : type === 'textarea' ? 'textarea' : 'input');
    input.id = `setting-${++sequence}`; label.htmlFor = input.id;
    if (choices) for (const [label, val] of choices) input.append(new Option(label, val));
    else if (type !== 'textarea') input.type = type;
    if (type === 'checkbox') { input.checked = value; label.className = 'toggle'; label.prepend(input); }
    else { input.value = value; label.append(input); }
    if (type === 'password') input.autocomplete = 'off';
    input.addEventListener('input', onChange); container.append(label); return input;
  }
  function button(container, title, action) { const b = node('button', title, 'secondary'); b.type = 'button'; b.addEventListener('click', action); container.append(b); return b; }
  const mcps = [], functions = [];
  const mcpSection = section('Remote MCP servers');
  mcpSection.append(node('p', 'The gateway connects to these servers and runs their allowed tools automatically. Hosts must be allowed in gateway configuration.', 'hint'));
  const presetSelect = field(mcpSection, 'Demo server', '0', 'text', mcpPresets.map((preset, i) => [preset.name, String(i)]));
  const actions = node('div', undefined, 'editor-actions'); mcpSection.append(actions);
  function addMcp(preset = { label: '', url: '', allowed: '' }) {
    const card = node('div', undefined, 'editor-card'); mcpSection.append(card);
    const row = {};
    const remove = button(card, 'Remove', () => { mcps.splice(mcps.indexOf(row), 1); card.remove(); onChange(); }); remove.className = 'remove text-button';
    card.append(node('h4', preset.name || 'Custom MCP server'));
    if (preset.description) card.append(node('p', preset.description, 'hint'));
    if (preset.docs) { const link = node('a', 'Provider documentation ↗', 'hint'); link.href = preset.docs; link.target = '_blank'; link.rel = 'noreferrer'; card.append(link); }
    row.label = field(card, 'Server label', preset.label);
    row.url = field(card, 'Endpoint URL', preset.url, 'url');
    row.allowed = field(card, 'Allowed tools · comma-separated; empty means all', preset.allowed);
    row.authorization = field(card, 'Authorization · optional, memory only', '', 'password');
    row.headers = field(card, 'Extra headers · optional JSON', '', 'textarea'); row.headers.rows = 2;
    mcps.push(row); onChange();
  }
  button(actions, 'Add selected server', () => addMcp(mcpPresets[Number(presetSelect.value)]));
  button(actions, 'Use demo prompt', () => onPreset(mcpPresets[Number(presetSelect.value)].prompt));
  button(actions, 'Add custom', () => addMcp());

  const functionSection = section('Client functions · JavaScript');
  functionSection.append(node('p', 'Define the tool schema and its JavaScript handler. The model supplies arguments; your handler runs in a worker inside an isolated frame, then its output is sent back to the API.', 'hint'));
  const functionActions = node('div', undefined, 'editor-actions'); functionSection.append(functionActions);
  function addFunction() {
    const card = node('div', undefined, 'editor-card'); functionSection.append(card);
    const row = {};
    const remove = button(card, 'Remove', () => { functions.splice(functions.indexOf(row), 1); card.remove(); onChange(); }); remove.className = 'remove text-button';
    card.append(node('h4', 'Client tool'));
    row.kind = field(card, 'Tool type', 'function', 'text', [['Function · JSON arguments', 'function'], ['Custom · free-form text', 'custom']]);
    row.name = field(card, 'Name', functions.length ? `client_tool_${functions.length + 1}` : 'summarize_numbers');
    row.namespace = field(card, 'Namespace · optional, functions only');
    row.description = field(card, 'Description', 'Calculate count, minimum, maximum, and average of a list of numbers.');
    row.schema = field(card, 'Parameters · JSON schema', JSON.stringify({ type: 'object', properties: { values: { type: 'array', items: { type: 'number' } } }, required: ['values'], additionalProperties: false }, null, 2), 'textarea'); row.schema.rows = 7;
    row.strict = field(card, 'Strict schema', true, 'checkbox');
    row.deferred = field(card, 'Defer until client tool discovery', false, 'checkbox');
    row.handler = field(card, 'Handler body · args is JSON; custom tools receive a string', 'const values = args.values;\nif (!Array.isArray(values) || !values.length || values.some(v => !Number.isFinite(v))) throw new Error("Expected nonempty finite numbers");\nreturn { count: values.length, min: Math.min(...values), max: Math.max(...values), average: values.reduce((a, b) => a + b, 0) / values.length };', 'textarea'); row.handler.rows = 8;
    const sample = field(card, 'Test input · JSON for functions, text for custom', '{"values":[2,4,8]}', 'textarea'); sample.rows = 2;
    const output = node('pre'); output.hidden = true;
    button(card, 'Test handler', async () => {
      try { const args = row.kind.value === 'custom' ? sample.value : JSON.parse(sample.value); output.textContent = await onTest(row.handler.value, args); }
      catch (error) { output.textContent = `Error: ${error.message}`; }
      output.hidden = false;
    }); card.append(output);
    functions.push(row); onChange();
  }
  button(functionActions, 'Add function', addFunction);
  button(functionActions, 'Use function demo', () => { if (!functions.length) addFunction(); onPreset('Use summarize_numbers to calculate the count, minimum, maximum, and average of [2, 4, 8, 16]. Explain the output.'); });
  const toolSearch = field(functionSection, 'Enable client tool discovery (tool_search)', false, 'checkbox');
  const shell = field(functionSection, 'Local shell · manually supply execution outputs', false, 'checkbox');
  functionSection.append(node('p', 'Browsers cannot execute a local shell. When enabled, calls pause for you to run the commands elsewhere and supply their actual outputs. File search is not implemented by this gateway and is not offered.', 'hint'));

  const options = section('Responses settings');
  const multiAgent = field(options, 'Enable multi-agent execution', false, 'checkbox');
  const parallel = field(options, 'Parallel tool calls (parallel_tool_calls)', true, 'checkbox');
  const maxCalls = field(options, 'Maximum tool calls per response (max_tool_calls)', '', 'number'); maxCalls.min = 1;
  const maxCallsHint = node('p', 'Unavailable with multi-agent execution in this gateway.', 'hint'); options.append(maxCallsHint);
  function sync() { maxCalls.disabled = multiAgent.checked; maxCallsHint.textContent = multiAgent.checked ? 'Maximum tool calls and reasoning summaries are unavailable with multi-agent execution in this gateway.' : 'Maximum built-in tool calls per response; separate from parallel tool calling.'; const summaryInput = options.querySelector('[data-reasoning-summary]'); if (summaryInput) summaryInput.disabled = multiAgent.checked; }
  multiAgent.addEventListener('change', () => { sync(); onChange(); }); sync();
  const toolChoice = field(options, 'Tool choice', 'auto', 'text', [['Auto', 'auto'], ['Required', 'required'], ['None', 'none']]);
  const maxOutput = field(options, 'Maximum output tokens · optional', '', 'number'); maxOutput.min = 1;
  const temperature = field(options, 'Temperature · optional', '', 'number'); temperature.min = 0; temperature.max = 2; temperature.step = .1;
  const topP = field(options, 'Top P · optional', '', 'number'); topP.min = 0; topP.max = 1; topP.step = .05;
  const effort = field(options, 'Reasoning effort · model dependent', '', 'text', [['Model default', ''], ['Low', 'low'], ['Medium', 'medium'], ['High', 'high'], ['Extra high', 'xhigh']]);
  const summary = field(options, 'Reasoning summary · model dependent', '', 'text', [['Model default', ''], ['Auto', 'auto'], ['Concise', 'concise'], ['Detailed', 'detailed']]);
  summary.dataset.reasoningSummary = ''; sync();
  const format = field(options, 'Output format', '', 'text', [['Default text', ''], ['JSON object', 'json_object'], ['JSON schema', 'json_schema']]);
  const outputSchema = field(options, 'Output schema · used for JSON schema format', '{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}', 'textarea'); outputSchema.rows = 3;
  const instructions = field(options, 'Additional instructions · optional', '', 'textarea'); instructions.rows = 3;
  const compactThreshold = field(options, 'Automatic compaction threshold · estimated input tokens', '', 'number'); compactThreshold.min = 1;
  options.append(node('p', 'Automatic compaction is triggered by the gateway’s estimate, not an exact tokenizer count. Compaction usage is included in response usage. Manual compaction is available after a completed turn.', 'hint'));
  const clientParallel = field(options, 'Concurrent browser functions · client execution only', '2', 'number'); clientParallel.min = 1; clientParallel.max = 8;
  const maxContinuations = field(options, 'Client-tool continuation / injection limit per turn', '12', 'number'); maxContinuations.min = 1; maxContinuations.max = 50;
  options.append(node('p', 'Responses transport and storage are selected above. Sampling and structured output support depend on the served model.', 'hint'));
  return {
    sync,
    setMultiAgent(value) { multiAgent.checked = value; sync(); },
    read() {
      const values = list => list.map(row => Object.fromEntries(Object.entries(row).map(([key, input]) => [key, input.type === 'checkbox' ? input.checked : input.value])));
      return { mcp: values(mcps), functions: values(functions), shell: shell.checked, toolSearch: toolSearch.checked, multiAgent: multiAgent.checked, parallel: parallel.checked, maxCalls: multiAgent.checked ? '' : maxCalls.value, toolChoice: toolChoice.value, maxOutput: maxOutput.value, temperature: temperature.value, topP: topP.value, effort: effort.value, summary: multiAgent.checked ? '' : summary.value, format: format.value, outputSchema: outputSchema.value, instructions: instructions.value, compactThreshold: compactThreshold.value, clientParallel: Number(clientParallel.value), maxContinuations: Number(maxContinuations.value) };
    },
  };
}
