// Human-readable progress projected from observed API events, never a simulated timer.
export function trackProgress(run, event, item) {
  const agent = item?.agent?.agent_name || event.agent?.agent_name || (run.singleAgent ? '/root' : undefined);
  if (event.type !== 'snapshot') {
    const owner = agent || (/^response\.(created|in_progress|completed|failed|incomplete)$/.test(event.type) ? '/root' : 'unattributed');
    if (!run.eventTypes.has(owner)) run.eventTypes.set(owner, new Map());
    const types = run.eventTypes.get(owner);
    types.set(event.type, (types.get(event.type) || 0) + 1);
  }
  const done = event.type === 'response.output_item.done' || (event.type === 'snapshot' && (run.status === 'completed' || item?.status === 'completed'));
  function step(owner, id, label, status = 'active', detail = '') {
    if (!owner) return;
    if (!run.progress.has(owner)) run.progress.set(owner, new Map());
    const steps = run.progress.get(owner);
    const previous = steps.get(id);
    // Replayed openings cannot regress a finished step.
    if (previous && previous.status !== 'active' && status === 'active') return;
    steps.set(id, { id, label, status, detail, eventType: event.type === 'snapshot' ? previous?.eventType || 'response.completed' : event.type });
  }
  if (item?.type === 'multi_agent_call_output' && item.action === 'spawn_agent') {
    for (const part of item.output || []) {
      try {
        const result = JSON.parse(part.text);
        if (!result.error && typeof result.task_name === 'string' && result.task_name.startsWith('/root/')) {
          step(result.task_name, 'spawn', 'Agent spawned', 'done');
        }
      } catch { /* Not a confirmed spawn. */ }
    }
  }
  if (!agent) return;
  if (!run.progress.has(agent)) run.progress.set(agent, new Map());
  if (item?.type === 'reasoning' || event.type.startsWith('response.reasoning')) {
    step(agent, item?.id || event.item_id || 'thinking', 'Thinking', toolStatus(item?.status, done));
  }
  if (item?.type === 'web_search_call') {
    step(agent, item.id, 'Searching the web', toolStatus(item.status, done), item.action?.query || (item.action?.queries || []).join(', '));
  }
  if (item?.type === 'code_interpreter_call') {
    const label = item.status === 'in_progress' ? 'Preparing Python' : 'Running Python';
    step(agent, item.id, label, toolStatus(item.status, done), 'Code interpreter');
  }
  if (item?.type === 'message') {
    step(agent, item.id, item.phase === 'final_answer' ? 'Reporting the solution' : 'Writing an update', toolStatus(item.status, done));
  }
  if (item?.type === 'mcp_call') step(agent, item.id, `MCP · ${item.name}`, item.error ? 'failed' : toolStatus(item.status, done), item.server_label);
  if (item?.type === 'mcp_list_tools') step(agent, item.id, 'Discovering MCP tools', item.error ? 'failed' : toolStatus(item.status, done), item.server_label);
  if (item?.type === 'compaction') step(agent, item.id || 'compaction', 'Compacting context', done ? 'done' : 'active');
  if (['function_call', 'custom_tool_call', 'shell_call', 'tool_search_call'].includes(item?.type)) {
    step(agent, item.call_id, item.type === 'shell_call' ? 'Waiting for shell output' : item.type === 'tool_search_call' ? 'Discovering client tools' : `Client function · ${item.name}`, 'active', done ? 'Waiting for client execution' : 'Receiving tool arguments');
  }
  if (item?.type === 'multi_agent_call') {
    const labels = { spawn_agent: 'Spawning an agent', wait_agent: 'Waiting for agents', send_message: 'Sending a message', followup_task: 'Assigning follow-up work', list_agents: 'Checking the team', interrupt_agent: 'Requesting interruption' };
    step(agent, item.call_id, labels[item.action] || 'Coordinating the team');
  }
  if (item?.type === 'multi_agent_call_output') {
    const steps = run.progress.get(agent);
    const previous = steps?.get(item.call_id);
    if (previous) {
      let error = '';
      for (const part of item.output || []) {
        try { error ||= JSON.parse(part.text)?.error || ''; } catch { /* Some acknowledgements are plain text. */ }
      }
      step(agent, item.call_id, previous.label, error ? 'failed' : 'done', String(error));
    }
  }
}

function toolStatus(status, done) {
  if (status === 'failed' || status === 'incomplete') return 'failed';
  return status === 'completed' || done ? 'done' : 'active';
}
