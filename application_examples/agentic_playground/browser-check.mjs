// Optional browser integration check. See README for the temporary Playwright setup.
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
import assert from 'node:assert/strict';
import http from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';
const root = fileURLToPath(new URL('../../', import.meta.url));
const requests = [];
let discovery = null;
let releaseChat;
let expectedGatewayAuth='Bearer demo-token';
let setupFailure = 'upload';
const setupRequests = [];
const storeLookups = [];
let sampleStore = null, sampleStoreId = 'vs_archive', olderSamplePage = false;
const message = { id:'answer', type:'message', role:'assistant', phase:'final_answer', content:[{type:'output_text',text:'# Result\n\nThe **average** is `7.5`.\n\n| Metric | Value |\n| --- | --- |\n| Average | 7.5 |\n\n```python\nprint(7.5)\n```\n\n- Verified example\n\n<script>window.unsafe = true</script>\n\n[unsafe](javascript:alert)'}] };
const usage = {input_tokens:10, output_tokens:5, total_tokens:15};
const teamAgents = ['code_analyst', 'web_researcher', 'mcp_researcher', 'file_researcher'];
const fileResult = { file_id: 'file_archive', filename: 'archive-requirements.md', score: 0.9, text: 'Retain documents for 90 days. <script>window.unsafe = true</script>' };
const server = http.createServer(async (req,res) => {
  try {
    if (req.url.startsWith('/v1/vector_stores?')) {
      assert.equal(req.method,'GET');
      assert.equal(req.headers.authorization,expectedGatewayAuth);
      storeLookups.push(req.url);
      res.setHeader('Content-Type','application/json');
      if (olderSamplePage && !new URL(req.url,'http://localhost').searchParams.has('after')) {
        return res.end(JSON.stringify({data:[{id:'vs_empty',metadata:{playground_sample:'archive-requirements.md'},status:'completed',file_counts:{completed:0}}],has_more:true,last_id:'vs_empty'}));
      }
      return res.end(JSON.stringify({data:sampleStore ? [sampleStore] : [],has_more:false}));
    }
    if (req.url === '/v1/files' || req.url === '/v1/vector_stores') {
      setupRequests.push(req.url);
      assert.equal(req.method,'POST');
      assert.equal(req.headers.authorization,expectedGatewayAuth);
      let raw=''; for await (const chunk of req) raw+=chunk;
      res.setHeader('Content-Type','application/json');
      if (req.url === '/v1/files') {
        assert.match(req.headers['content-type'],/^multipart\/form-data; boundary=/);
        assert.match(raw,/filename="archive-requirements.md"/);
        assert.match(raw,/250 GiB/);
        assert.match(raw,/name="purpose"\r\n\r\nassistants/);
        if (setupFailure === 'upload') { res.statusCode=404; return res.end(); }
        return res.end(JSON.stringify({id:'file_archive'}));
      }
      assert.deepEqual(JSON.parse(raw),{name:'playground-archive',file_ids:['file_archive'],metadata:{playground_sample:'archive-requirements.md'}});
      if (setupFailure === 'index') { res.statusCode=500; return res.end(JSON.stringify({error:{message:'Indexing unavailable'}})); }
      sampleStore = {id:sampleStoreId,name:'playground-archive',metadata:{playground_sample:'archive-requirements.md'},status:'completed',file_counts:{completed:1}};
      return res.end(JSON.stringify(sampleStore));
    }
    if (req.url.startsWith('/v1/conversations')) {
      res.setHeader('Content-Type','application/json');
      if (req.url.includes('/items')) return res.end(JSON.stringify({data:[{...message, id:'stored1'}],has_more:false,last_id:'stored1'}));
      return res.end(JSON.stringify({id:'conv_demo',object:'conversation'}));
    }
    if (req.url === '/v1/responses') {
      let raw=''; for await(const chunk of req) raw+=chunk;
      const request=JSON.parse(raw); requests.push(request);
      res.setHeader('Content-Type','text/event-stream');
      const send = event => res.write(`data: ${JSON.stringify(event)}\n\n`);
      const first = requests.length===1;
      const compact = request.input?.[0]?.type === 'compaction_trigger';
      if (compact) assert.equal(request.store, true);
      let item=compact ? {type:'compaction',id:'cp',encrypted_content:'The average is 7.5.'} : first ? {id:'fn1',type:'function_call',call_id:'call1',name:'summarize_numbers',arguments:'{"values":[2,4,8,16]}'} : message;
      if (request.input === 'Discover and call summarize_numbers.') {
        discovery = { definition: request.tools.find(tool => tool.name === 'summarize_numbers'), responseId: 'r'+requests.length, stage: 'search' };
        assert.equal(discovery.definition.defer_loading, true);
        assert.ok(request.tools.some(tool => tool.type === 'tool_search'));
        item = { id: 'search1', type: 'tool_search_call', call_id: 'find_summary', execution: 'client', arguments: { query: 'summarize_numbers' } };
      } else if (discovery?.stage === 'search') {
        assert.equal(request.previous_response_id, discovery.responseId);
        assert.deepEqual(request.input, [{ type: 'tool_search_output', call_id: 'find_summary', execution: 'client', status: 'completed', tools: [discovery.definition] }]);
        discovery.stage = 'execute'; discovery.responseId = 'r'+requests.length;
        item = { id: 'deferred1', type: 'function_call', call_id: 'deferred_summary', name: 'summarize_numbers', arguments: '{"values":[2,4,8,16]}' };
      } else if (discovery?.stage === 'execute') {
        assert.equal(request.previous_response_id, discovery.responseId);
        assert.equal(request.input[0].type, 'function_call_output');
        assert.equal(request.input[0].call_id, 'deferred_summary');
        assert.equal(JSON.parse(request.input[0].output).average, 7.5);
        discovery = null;
      }
      send({type:'response.created',response:{id:'r'+requests.length}});
      if (request.tools?.some(tool => tool.type === 'file_search')) {
        for (const name of teamAgents) send({type:'response.output_item.done',item:{id:'spawn_'+name,type:'multi_agent_call_output',action:'spawn_agent',agent:{agent_name:'/root'},output:[{type:'output_text',text:JSON.stringify({task_name:'/root/'+name})}]}});
        const calls = [
          {id:'ci_team',type:'code_interpreter_call',status:'completed',code:'print(240)',outputs:[{type:'logs',logs:'240'}]},
          {id:'web_team',type:'web_search_call',status:'completed',action:{type:'search',query:'Azure Blob Storage retention'}},
          {id:'mcp_team',type:'mcp_call',status:'completed',server_label:'microsoft_learn',name:'microsoft_docs_search',output:'Upload documentation'},
          {id:'fs_team',type:'file_search_call',status:'in_progress',queries:['archive retention'],results:null},
        ].map((call, i) => ({...call,agent:{agent_name:'/root/'+teamAgents[i]}}));
        for (const call of calls) send({type:'response.output_item.added',item:call});
        send({type:'response.file_search_call.searching',item_id:'fs_team'});
        await new Promise(resolve=>setTimeout(resolve,200));
        calls[3] = {...calls[3],status:'completed',results:request.include?.includes('file_search_call.results') ? [fileResult] : undefined};
        send({type:'response.file_search_call.completed',item_id:'fs_team'});
        for (const call of calls) send({type:'response.output_item.done',item:call});
        const answer = {...message,agent:{agent_name:'/root'},content:[{type:'output_text',text:'Retain documents for 90 days. 【file_archive】',annotations:[{type:'file_citation',file_id:'file_archive',filename:'archive-requirements.md',index:30}]}]};
        send({type:'response.completed',response:{id:'r'+requests.length,status:'completed',usage,output:[...calls,answer]}});
        return res.end();
      }
      send({type:'response.output_item.added',item});
      if (request.input === 'What was the average?') await new Promise(resolve=>{releaseChat=resolve;});
      else await new Promise(r=>setTimeout(r,200));
      send({type:'response.output_item.done',item});
      send({type:'response.completed',response:{id:'r'+requests.length,status:'completed',usage,output:[item]}});
      return res.end();
    }
    const pathname = new URL(req.url,'http://localhost').pathname;
    const file = pathname.endsWith('/') ? `${pathname}index.html` : pathname;
    res.setHeader('Content-Type',file.endsWith('.mjs')?'text/javascript':file.endsWith('.css')?'text/css':file.endsWith('.svg')?'image/svg+xml':'text/html');
    res.end(await readFile(root+file));
  } catch(error) {res.statusCode=500;res.end(error.message);}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const base=`http://127.0.0.1:${server.address().port}`;
const browser=await chromium.launch({headless:true,args:['--no-sandbox']});
try {
  const page=await browser.newPage({viewport:{width:1440,height:1100}});
  const errors=[];page.on('pageerror',e=>errors.push(e.message));
  await page.goto(`${base}/application_examples/agentic_playground/`);
  await page.locator('#mode').waitFor();
  assert.equal(await page.getByLabel('Enable multi-agent execution',{exact:true}).isChecked(),false);
  assert.equal(await page.locator('#concurrency').inputValue(),'1');
  assert.equal(await page.locator('#store').isChecked(),true);
  const sandbox = await page.evaluate(async()=> {
    const {executeHandler}=await import('./sandbox.mjs');
    const success=await executeHandler('return { sum: args.a + args.b, dom: typeof document, storage: typeof localStorage };',{a:2,b:3});
    const blocked=await executeHandler('try { await fetch("https://example.com"); return false; } catch { return true; }',{});
    let timedOut=false;try{await executeHandler('while(true) {}',{});}catch(e){timedOut=/5 second/.test(e.message);}
    const controller=new AbortController();const pending=executeHandler('await new Promise(()=>{});',{},controller.signal);controller.abort();
    let aborted=false;try{await pending;}catch(e){aborted=e.name==='AbortError';}
    return {success,blocked,timedOut,aborted,frames:document.querySelectorAll('iframe').length};
  });
  assert.deepEqual(JSON.parse(sandbox.success),{sum:5,dom:'undefined',storage:'undefined'});
  assert.equal(sandbox.blocked,'true');assert.equal(sandbox.timedOut,true);assert.equal(sandbox.aborted,true);assert.equal(sandbox.frames,0);
  await page.locator('#base').fill(base);await page.locator('#model').fill('demo');
  await page.getByText('Client functions · JavaScript',{exact:true}).click();
  await page.getByRole('button',{name:'Use function demo',exact:true}).click();
  await page.getByText('Responses settings',{exact:true}).click();
  await page.getByLabel('Enable multi-agent execution',{exact:true}).uncheck();
  await page.locator('#search').uncheck();await page.locator('#code').uncheck();
  await page.locator('#run').click();
  await page.waitForFunction(()=>document.getElementById('run-status').textContent==='Completed');
  assert.equal(requests.length,2);
  assert.equal(requests[1].previous_response_id,'r1');
  assert.equal(JSON.parse(requests[1].input[0].output).average,7.5);
  assert.deepEqual(await page.locator('#usage-panel strong').allTextContents(),['20','10','30']);
  assert.match(await page.locator('#activity').innerText(),/summarize_numbers/);
  await page.getByRole('button',{name:'Compact conversation',exact:true}).click();
  await page.locator('.compaction-notice').waitFor({state:'visible'});
  await page.locator('#followup').fill('What was the average?');await page.locator('#send').click();
  await page.waitForFunction(()=>document.getElementById('followup').disabled&&document.querySelector('.chat-user')!==null);
  assert.equal(await page.locator('#followup').inputValue(),'');
  assert.equal(await page.locator('#send').isDisabled(),true);
  assert.equal(await page.locator('.chat-user').last().innerText(),'You\n\nWhat was the average?');
  assert.match(await page.locator('.chat-assistant').last().innerText(),/Working on your message/);
  await page.waitForTimeout(100); releaseChat();
  await page.waitForFunction(()=>document.getElementById('run-status').textContent==='Completed'&&!document.getElementById('send').disabled);
  assert.match(await page.locator('.chat-assistant').last().innerText(),/7.5/);
  assert.equal(await page.locator('#followup').inputValue(),'');
  const last=requests.at(-1); assert.equal(last.previous_response_id,'r3');assert.equal(last.input,'What was the average?');
  await page.getByRole('tab',{name:'Final answer'}).click();assert.match(await page.locator('#answer').innerText(),/7.5/);
  assert.equal(await page.locator('#answer h1').textContent(),'Result');
  assert.equal(await page.locator('#answer table tbody tr').count(),1);
  assert.equal(await page.locator('#answer pre code').textContent(),'print(7.5)');
  assert.equal(await page.locator('#answer script').count(),0);
  assert.equal(await page.locator('#answer a[href^=javascript]').count(),0);
  assert.equal(await page.evaluate(()=>window.unsafe),undefined);
  await page.locator('#answer').screenshot({path:`${tmpdir()}/agentic-playground-answer.png`});
  const downloadEvent = page.waitForEvent('download');
  await page.locator('#answer').getByRole('button',{name:'Download Markdown',exact:true}).click();
  assert.equal((await downloadEvent).suggestedFilename(),'agentic-answer.md');
  assert.equal(await page.locator('.team-diagram').count(),0);
  assert.equal(await page.locator('.agent-tree').count(),0);
  // API tabs keep independent IDs; prompts in the main editor continue the chain.
  await page.getByRole('tab',{name:'Conversations API',exact:true}).click();
  await page.locator('#create-conversation').click();
  await page.waitForFunction(()=>document.getElementById('conversation-status').textContent.includes('conv_demo')&&!document.getElementById('run').disabled);
  await page.locator('#run').click();
  await page.waitForFunction(()=>document.getElementById('run-status').textContent==='Completed'&&!document.getElementById('run').disabled);
  assert.equal(requests.at(-1).conversation,'conv_demo'); assert.equal(requests.at(-1).previous_response_id,undefined);
  await page.locator('#run').click();
  await page.waitForFunction(()=>document.getElementById('run-status').textContent==='Completed'&&!document.getElementById('run').disabled);
  assert.equal(requests.at(-1).conversation,'conv_demo'); assert.equal(requests.at(-1).previous_response_id,undefined);
  assert.match(await page.locator('#conversation-items').textContent(),/7.5/);
  await page.getByRole('tab',{name:'Responses API',exact:true}).click();
  await page.locator('#run').click();
  await page.waitForFunction(()=>document.getElementById('run-status').textContent==='Completed'&&!document.getElementById('run').disabled);
  assert.equal(requests.at(-1).previous_response_id,'r4');

  // A loaded tool must match the declaration sent before discovery, including defer_loading.
  await page.locator('#new-session').click();
  await page.getByLabel('Enable client tool discovery (tool_search)',{exact:true}).check();
  await page.getByLabel('Defer until client tool discovery',{exact:true}).check();
  await page.locator('#mission').fill('Discover and call summarize_numbers.');
  const discoveryStart = requests.length;
  await page.locator('#run').click();
  await page.waitForFunction(()=>['Completed','Error'].includes(document.getElementById('run-status').textContent)&&!document.getElementById('run').disabled);
  assert.equal(await page.locator('#run-status').textContent(),'Completed',await page.locator('#notice').textContent());
  assert.equal(requests.length-discoveryStart,3);
  assert.equal(discovery,null);
  assert.deepEqual(await page.locator('#usage-panel strong').allTextContents(),['30','15','45']);
  await page.getByLabel('Defer until client tool discovery',{exact:true}).uncheck();
  await page.getByLabel('Enable client tool discovery (tool_search)',{exact:true}).uncheck();

  // Browser-native WebSocket events, with a mocked server at the transport boundary.
  let connections=0, wsCount=0; const frames=[];
  await page.routeWebSocket('**/v1/responses', ws=> {
    connections++;
    let pendingId;
    const send = event => ws.send(JSON.stringify({...event,stream_id:'playground'}));
    const complete = id => {send({type:'response.output_item.done',item:{...message,agent:{agent_name:'/root'}}});send({type:'response.completed',response:{id,status:'completed',usage,output:[{...message,agent:{agent_name:'/root'}}]}});};
    ws.onMessage(raw=> {
      const frame=JSON.parse(raw);frames.push(frame);
      if(frame.type==='response.inject') {
        assert.equal(frame.response_id,pendingId);
        assert.equal(JSON.parse(frame.input[0].output).average,7.5);
        send({type:'response.inject.created',response_id:pendingId});complete(pendingId);return;
      }
      assert.equal(frame.type,'response.create');
      const id='ws'+(++wsCount);send({type:'response.created',response:{id}});
      if(frame.multi_agent?.enabled) {
        pendingId=id;
        for(const name of ['solver_a','solver_b']) send({type:'response.output_item.done',item:{id:'spawn_'+name,type:'multi_agent_call_output',action:'spawn_agent',agent:{agent_name:'/root'},output:[{type:'output_text',text:JSON.stringify({task_name:'/root/'+name})}]}});
        send({type:'response.output_item.done',item:{id:'live',type:'function_call',call_id:'live1',name:'summarize_numbers',arguments:'{"values":[2,4,8,16]}',agent:{agent_name:'/root/solver_a'}}});
      } else { complete(id); if(frame.input==='close socket') ws.close(); }
    });
  });
  await page.locator('#new-session').click();
  await page.locator('#transport').selectOption('websocket');await page.locator('#store').uncheck();
  await page.locator('#run').click();
  await page.waitForFunction(()=>document.getElementById('run-status').textContent==='Completed'&&!document.getElementById('run').disabled);
  await page.locator('#run').click();
  await page.waitForFunction(()=>document.getElementById('run-status').textContent==='Completed'&&!document.getElementById('run').disabled);
  assert.equal(connections,1);assert.equal(frames[0].store,false);assert.equal(frames[1].previous_response_id,'ws1');
  await page.locator('#new-session').click();
  await page.locator('#transport').selectOption('websocket');
  await page.getByLabel('Enable multi-agent execution',{exact:true}).check();await page.locator('#concurrency').selectOption('2');
  await page.locator('#run').click();
  await page.waitForFunction(()=>document.getElementById('run-status').textContent==='Completed'&&!document.getElementById('run').disabled);
  assert.equal(connections,2);assert.equal(frames.at(-1).type,'response.inject');
  await page.getByRole('tab',{name:'Agent progress',exact:true}).click();
  assert.equal(await page.locator('.agent-tree .tree-agent').count(),3);
  assert.deepEqual(errors,[]);
  await page.getByRole('tab',{name:'Agent progress'}).click();
  await page.screenshot({path:`${tmpdir()}/agentic-playground-desktop.png`,fullPage:true});
  await page.setViewportSize({width:390,height:844});
  assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
  await page.screenshot({path:`${tmpdir()}/agentic-playground-mobile.png`,fullPage:true});
  await page.setViewportSize({width:1440,height:1100});
  await page.locator('#new-session').click();
  await page.locator('#transport').selectOption('websocket');
  await page.locator('#store').uncheck();
  await page.getByLabel('Enable multi-agent execution',{exact:true}).uncheck();
  await page.locator('#mission').fill('close socket');
  await page.locator('#run').click();
  await page.waitForFunction(()=>document.getElementById('run-status').textContent==='Completed'&&!document.getElementById('run').disabled);
  await page.locator('#mission').fill('continue after socket loss');await page.locator('#run').click();
  await page.waitForFunction(()=>document.getElementById('run-status').textContent==='Error');
  assert.match(await page.locator('#notice').textContent(),/no longer available/);
  assert.equal(connections,3);
  assert.deepEqual(errors,[]);

  // A fresh four-agent demo validates setup, streams file evidence, and keeps retrieval on follow-up.
  const teamPage = await browser.newPage({viewport:{width:1440,height:1100}});
  teamPage.on('pageerror', error=>errors.push(error.message));
  await teamPage.goto(`${base}/application_examples/agentic_playground/`);
  await teamPage.locator('#base').fill(base); await teamPage.locator('#model').fill('demo');
  await teamPage.locator('#token').fill('demo-token');
  await teamPage.locator('#mode').selectOption('team');
  await teamPage.waitForFunction(()=>!document.getElementById('run').disabled&&document.getElementById('notice').textContent.includes('does not expose the Files API'));
  assert.equal(await teamPage.locator('#concurrency').inputValue(),'4');
  assert.equal(await teamPage.getByLabel('Enable multi-agent execution',{exact:true}).isChecked(),true);
  assert.equal(await teamPage.getByLabel('Enable file search',{exact:true}).isChecked(),true);
  assert.equal(await teamPage.locator('#search').isChecked(),true);
  assert.equal(await teamPage.locator('#code').isChecked(),true);
  assert.equal(await teamPage.locator('#store').isChecked(),true);
  const beforeInvalid = requests.length;
  await teamPage.locator('#run').click();
  await teamPage.waitForFunction(()=>!document.getElementById('run').disabled&&document.getElementById('notice').textContent.includes('does not expose the Files API'));
  assert.equal(requests.length,beforeInvalid);
  assert.equal(await teamPage.getByLabel('Vector store IDs · comma or whitespace separated',{exact:true}).inputValue(),'');
  await teamPage.getByLabel('Vector store IDs · comma or whitespace separated',{exact:true}).fill('vs_existing');
  await teamPage.locator('#mode').selectOption('prompt');
  await teamPage.locator('#mode').selectOption('team');
  assert.equal(await teamPage.getByLabel('Vector store IDs · comma or whitespace separated',{exact:true}).inputValue(),'vs_existing');
  assert.equal(setupRequests.length,2);
  setupFailure = 'index';
  await teamPage.getByRole('button',{name:'Load sample document',exact:true}).click();
  await teamPage.waitForFunction(()=>!document.getElementById('run').disabled&&document.getElementById('notice').textContent.includes('vector store setup failed'));
  assert.match(await teamPage.locator('#notice').textContent(),/file_archive.*Indexing unavailable/);
  assert.equal(await teamPage.getByLabel('Vector store IDs · comma or whitespace separated',{exact:true}).inputValue(),'vs_existing');
  setupFailure = null;
  await teamPage.getByRole('button',{name:'Load sample document',exact:true}).click();
  await teamPage.waitForFunction(()=>!document.getElementById('run').disabled&&document.getElementById('notice').textContent.includes('Sample document ready'));
  assert.equal(await teamPage.getByLabel('Vector store IDs · comma or whitespace separated',{exact:true}).inputValue(),'vs_archive');
  assert.deepEqual(setupRequests,['/v1/files','/v1/files','/v1/files','/v1/vector_stores','/v1/files','/v1/vector_stores']);
  await teamPage.locator('#example').click();
  assert.equal(await teamPage.getByLabel('Vector store IDs · comma or whitespace separated',{exact:true}).inputValue(),'vs_archive');
  assert.equal(await teamPage.getByLabel('Server label',{exact:true}).count(),1);
  await teamPage.locator('#run').click();
  await teamPage.waitForFunction(()=>document.getElementById('run-status').textContent==='Completed'&&!document.getElementById('run').disabled);
  const teamRequest = requests.at(-1), teamId = 'r'+requests.length;
  assert.deepEqual(teamRequest.multi_agent,{enabled:true,max_concurrent_subagents:4});
  assert.deepEqual(teamRequest.tools.map(tool=>tool.type),['web_search','code_interpreter','file_search','mcp']);
  assert.deepEqual(teamRequest.tools[2],{type:'file_search',vector_store_ids:['vs_archive'],max_num_results:5});
  assert.deepEqual(teamRequest.include,['file_search_call.results']);
  for (const name of teamAgents) assert.equal(teamRequest.input.includes(name),false);
  assert.equal(await teamPage.locator('.agent-tree .tree-agent').count(),5);
  assert.equal(await teamPage.locator('#tool-count').textContent(),'4');
  await teamPage.locator('#agent-filter').selectOption('/root/file_researcher');
  await teamPage.getByText('View code, outputs & sources (1)',{exact:true}).click();
  await teamPage.locator('.agent-evidence .evidence-item').waitFor({state:'visible'});
  assert.match(await teamPage.locator('#activity').innerText(),/Searching files/);
  assert.match(await teamPage.locator('#activity').innerText(),/archive-requirements.md/);
  assert.match(await teamPage.locator('#activity').innerText(),/Score: 0.9/);
  assert.match(await teamPage.locator('#activity').innerText(),/Retain documents for 90 days/);
  assert.equal(await teamPage.locator('#activity script').count(),0);
  assert.equal(await teamPage.evaluate(()=>window.unsafe),undefined);
  await teamPage.getByRole('tab',{name:'Final answer',exact:true}).click();
  assert.match(await teamPage.locator('#answer').innerText(),/Source file: archive-requirements.md/);
  await teamPage.getByLabel('Show retrieved passages',{exact:true}).uncheck();
  await teamPage.locator('#followup').fill('Explain the retention requirement.'); await teamPage.locator('#send').click();
  await teamPage.waitForFunction(()=>document.getElementById('run-status').textContent==='Completed'&&!document.getElementById('send').disabled);
  assert.equal(requests.at(-1).previous_response_id,teamId);
  assert.deepEqual(requests.at(-1).tools,teamRequest.tools);
  assert.equal(requests.at(-1).include,undefined);
  await teamPage.getByRole('tab',{name:'Agent progress',exact:true}).click();
  await teamPage.locator('#agent-filter').selectOption('/root/file_researcher');
  await teamPage.locator('.agent-evidence').evaluate(element=>{element.open=true;});
  await teamPage.locator('.agent-evidence .evidence-item').waitFor({state:'visible'});
  assert.match(await teamPage.locator('#activity').innerText(),/Retrieved passages were not included/);
  await teamPage.setViewportSize({width:390,height:844});
  assert.equal(await teamPage.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);

  // The four-agent workflow reuses automatic sample setup and sends all four tools with brief progress instructions.
  const reusePage = await browser.newPage();
  reusePage.on('pageerror',error=>errors.push(error.message));
  await reusePage.goto(`${base}/application_examples/agentic_playground/`);
  await reusePage.locator('#base').fill(base); await reusePage.locator('#token').fill('demo-token');
  await reusePage.locator('#model').fill('demo');
  const beforeReuse = setupRequests.length, beforeLookup = storeLookups.length;
  olderSamplePage = true;
  await reusePage.locator('#mode').selectOption('team');
  await reusePage.waitForFunction(()=>!document.getElementById('run').disabled&&document.getElementById('notice').textContent.includes('Sample document ready'));
  assert.equal(await reusePage.getByLabel('Vector store IDs · comma or whitespace separated',{exact:true}).inputValue(),'vs_archive');
  assert.equal(setupRequests.length,beforeReuse);
  assert.deepEqual(storeLookups.slice(beforeLookup),['/v1/vector_stores?limit=100','/v1/vector_stores?limit=100&after=vs_empty']);
  const reusedPrompt = await reusePage.locator('#mission').inputValue();
  await reusePage.locator('#mission').fill('An edited prompt');
  await reusePage.locator('#example').click();
  assert.equal(await reusePage.locator('#mission').inputValue(),reusedPrompt);
  await reusePage.locator('#run').click();
  await reusePage.waitForFunction(()=>document.getElementById('run-status').textContent==='Completed'&&!document.getElementById('run').disabled);
  const reusedRequest = requests.at(-1);
  assert.equal(reusedRequest.input,reusedPrompt);
  assert.match(reusedRequest.input,/Run 4 agents/);
  assert.deepEqual(reusedRequest.multi_agent,{enabled:true,max_concurrent_subagents:4});
  assert.deepEqual(reusedRequest.tools.map(tool=>tool.type),['web_search','code_interpreter','file_search','mcp']);
  assert.match(reusedRequest.instructions,/one search or execution per subagent/);
  assert.match(reusedRequest.instructions,/while other agents are still working/);
  assert.doesNotMatch(reusedRequest.instructions,/implement a fix, run tests, inspect failures, and retry/);
  assert.equal(setupRequests.length,beforeReuse);
  await reusePage.getByLabel('Vector store IDs · comma or whitespace separated',{exact:true}).fill('');
  await reusePage.getByRole('button',{name:'Use file search demo',exact:true}).click();
  await reusePage.waitForFunction(()=>!document.getElementById('run').disabled&&document.getElementById('notice').textContent.includes('Sample document ready'));
  assert.equal(await reusePage.locator('#mode').inputValue(),'prompt');
  assert.equal(await reusePage.getByLabel('Vector store IDs · comma or whitespace separated',{exact:true}).inputValue(),'vs_archive');
  assert.equal(setupRequests.length,beforeReuse);

  // A fresh database produces a new ID automatically when the demo is selected after reload.
  sampleStore = null; olderSamplePage = false; sampleStoreId = 'vs_after_reset';
  await reusePage.reload();
  await reusePage.locator('#base').fill(base); await reusePage.locator('#token').fill('demo-token');
  await reusePage.locator('#mode').selectOption('team');
  await reusePage.waitForFunction(()=>!document.getElementById('run').disabled&&document.getElementById('notice').textContent.includes('Sample document ready'));
  assert.equal(await reusePage.getByLabel('Vector store IDs · comma or whitespace separated',{exact:true}).inputValue(),'vs_after_reset');
  assert.deepEqual(setupRequests.slice(beforeReuse),['/v1/files','/v1/vector_stores']);

  // Streaming must not replace controls between pointer-down and click.
  const stressPage=await browser.newPage({viewport:{width:1440,height:1100}});
  stressPage.on('pageerror',error=>errors.push(error.message));
  let stressStream, stressSend;
  const stressTool=i=>({id:'stress_tool_'+i,type:'mcp_call',name:'docs',server_label:'learn',status:'completed',output:'Large tool result. '.repeat(15000),agent:{agent_name:'/root/worker_'+i}});
  try {
    await stressPage.routeWebSocket('**/v1/responses',ws=>ws.onMessage(()=>{
      stressSend=event=>ws.send(JSON.stringify({...event,stream_id:'playground'}));
      stressSend({type:'response.created',response:{id:'stress'}});
      for(let i=0;i<5;i++) {
        const agent={agent_name:'/root/worker_'+i};
        stressSend({type:'response.output_item.done',item:{id:'stress_spawn_'+i,type:'multi_agent_call_output',action:'spawn_agent',agent:{agent_name:'/root'},output:[{type:'output_text',text:JSON.stringify({task_name:agent.agent_name})}]}});
        stressSend({type:'response.output_item.done',item:stressTool(i)});
        stressSend({type:'response.output_item.added',item:{id:'stress_message_'+i,type:'message',role:'assistant',status:'in_progress',content:[{type:'output_text',text:''}],agent}});
      }
      stressStream=setInterval(()=>{for(let i=0;i<5;i++)stressSend({type:'response.output_text.delta',item_id:'stress_message_'+i,content_index:0,delta:'working ',agent:{agent_name:'/root/worker_'+i}});},12);
    }));
    await stressPage.goto(`${base}/application_examples/agentic_playground/`);
    await stressPage.locator('#model').fill('demo');await stressPage.locator('#transport').selectOption('websocket');
    await stressPage.getByText('Responses settings',{exact:true}).click();
    await stressPage.getByLabel('Enable multi-agent execution',{exact:true}).check();await stressPage.locator('#concurrency').selectOption('5');
    await stressPage.locator('#run').click();await stressPage.locator('.agent-evidence').first().waitFor();
    const summary=await stressPage.locator('.agent-evidence summary').first().elementHandle();
    const option=await stressPage.locator('#agent-filter option[value="/root/worker_0"]').elementHandle();
    await stressPage.waitForTimeout(1200);
    assert.equal(await summary.evaluate(e=>e.isConnected),true);
    assert.equal(await option.evaluate(e=>e.isConnected),true);
    assert.equal(await stressPage.locator('.agent-evidence:not([open]) pre').count(),0);
    await stressPage.locator('.agent-evidence summary').first().click({delay:100});
    await stressPage.locator('.agent-evidence .evidence-item').first().waitFor({state:'visible'});
    assert.equal(await stressPage.locator('.agent-evidence').first().evaluate(e=>e.open),true);
    stressSend({type:'response.output_item.done',item:{...stressTool(0),output:'Updated evidence from the stream'}});
    await stressPage.getByText('Updated evidence from the stream',{exact:true}).waitFor();
    await stressPage.locator('.agent-tree').getByRole('button',{name:/worker 3/}).click({delay:100});
    assert.equal(await stressPage.locator('#agent-filter').inputValue(),'/root/worker_3');
    assert.equal(await stressPage.locator('.agent-tree .tree-agent').count(),6);
    assert.equal(await stressPage.locator('.agent-tree .tree-path').first().textContent(),'/root');
    assert.equal(await stressPage.locator('.agent-tree .tree-agent[aria-pressed=true]').count(),1);
    await stressPage.locator('.agent-tree .tree-agent').first().click({delay:100});
    assert.equal(await stressPage.locator('#agent-filter').inputValue(),'/root');
    assert.equal(await stressPage.locator('.agent-tree .tree-agent').count(),6);
    await stressPage.locator('#agent-filter').selectOption('all');
    assert.equal(await stressPage.locator('.agent-evidence').first().evaluate(e=>e.open),true);
    await stressPage.getByRole('tab',{name:'Debug',exact:true}).click();
    await stressPage.getByText('Raw streaming events · latest 250',{exact:true}).click();
    await stressPage.waitForFunction(()=>document.getElementById('events-preview').textContent.includes('response.output_text.delta'));
    await stressPage.locator('#disconnect').click();
    await stressPage.waitForFunction(()=>document.getElementById('run-status').textContent==='Disconnected');
    assert.equal(await stressPage.locator('#run').isEnabled(),true);
  } finally {clearInterval(stressStream);await stressPage.close();}
  assert.deepEqual(errors,[]);
  console.log('PASS browser: sandbox success/network isolation/timeout/abort, function continuation, deferred discovery declaration equality, usage, stored compaction, safe Markdown, API tab isolation, HTTP ID chaining, WebSocket continuity and injection, observed agent tree, four-agent demo with progress instructions, file search automatic setup/reuse/pagination/database reset/retry/authentication/evidence/citations/continuation, mobile overflow, chat input clearing/locking/history, stable clickable controls and lazy evidence under a busy five-agent stream');
} finally {await browser.close();server.close();}
