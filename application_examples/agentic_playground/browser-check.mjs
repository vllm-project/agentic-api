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
const message = { id:'answer', type:'message', role:'assistant', phase:'final_answer', content:[{type:'output_text',text:'# Result\n\nThe **average** is `7.5`.\n\n| Metric | Value |\n| --- | --- |\n| Average | 7.5 |\n\n```python\nprint(7.5)\n```\n\n- Verified example\n\n<script>window.unsafe = true</script>\n\n[unsafe](javascript:alert)'}] };
const usage = {input_tokens:10, output_tokens:5, total_tokens:15};
const server = http.createServer(async (req,res) => {
  try {
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
      send({type:'response.output_item.added',item});
      await new Promise(r=>setTimeout(r,200));
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
  await page.waitForFunction(()=>document.getElementById('run-status').textContent==='Completed'&&!document.getElementById('send').disabled);
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
  await page.getByRole('button',{name:'Download Markdown',exact:true}).click();
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
  console.log('PASS browser: sandbox success/network isolation/timeout/abort, function continuation, deferred discovery declaration equality, usage, stored compaction, safe Markdown, API tab isolation, HTTP ID chaining, WebSocket continuity and injection, observed agent tree, mobile overflow');
} finally {await browser.close();server.close();}
