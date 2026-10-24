// Opt-in release-webview integration test. The QA launcher alone enables CDP.
// Never connects to an ordinary browser or uses user archives; all fixtures are new.
import assert from 'node:assert/strict';
import {mkdir,writeFile,readFile,readdir,stat,open} from 'node:fs/promises';
import {join,resolve} from 'node:path';
import {randomUUID,createHash} from 'node:crypto';

const port=Number(process.argv[2]||9446);
assert(Number.isInteger(port)&&port>=1024&&port<=65535);
const targets=await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
const matches=targets.filter(t=>t.type==='page'&&t.url==='http://tauri.localhost/'&&/Archive|压缩/.test(t.title));
assert.equal(matches.length,1,'Require one explicitly launched application QA webview');
const socket=new WebSocket(matches[0].webSocketDebuggerUrl);await new Promise((r,j)=>{socket.onopen=r;socket.onerror=j;});
let serial=0;const pending=new Map();const requests=[];const consoleErrors=[];
socket.onmessage=event=>{const data=JSON.parse(event.data);if(data.id){const call=pending.get(data.id);if(call){clearTimeout(call.timer);pending.delete(data.id);data.error?call.reject(Error(JSON.stringify(data.error))):call.resolve(data.result);}}
  else if(data.method==='Network.requestWillBeSent')requests.push(data.params.request.url);
  else if(data.method==='Runtime.exceptionThrown')consoleErrors.push(data.params.exceptionDetails.text);};
function cdp(method,params={}) {return new Promise((resolve,reject)=>{const id=++serial;const timer=setTimeout(()=>{pending.delete(id);reject(Error(`CDP timeout: ${method}`));},60000);pending.set(id,{resolve,reject,timer});socket.send(JSON.stringify({id,method,params}));});}
async function evaluate(expression) {const result=await cdp('Runtime.evaluate',{expression,awaitPromise:true,returnByValue:true});if(result.exceptionDetails)throw Error(result.exceptionDetails.exception?.description??result.exceptionDetails.text);return result.result.value;}
const delay=ms=>new Promise(r=>setTimeout(r,ms));
async function until(expression,timeout=30000) {const end=Date.now()+timeout;while(Date.now()<end){const result=await evaluate(expression);if(result)return result;await delay(40);}throw Error(`Timed out: ${expression}`);}
const qa=resolve('.artifacts',`desktop-e2e-${randomUUID()}`);await mkdir(qa);const checks=[];
const check=(name)=>{checks.push(name);console.log(`PASS ${name}`);};
const input=join(qa,'输入 sample.zip');await writeFile(input,Buffer.from('hello'),{flag:'wx'});
const hash=createHash('sha256').update('hello').digest('hex');
await cdp('Runtime.enable');await cdp('Network.enable');
await cdp('Network.emulateNetworkConditions',{offline:true,latency:0,downloadThroughput:-1,uploadThroughput:-1});
async function select(operation,fields) {
  await evaluate(`document.querySelector('[data-operation="${operation}"]').click()`);
  for(const [key,value] of Object.entries(fields)) {
    await evaluate(`(()=>{const node=document.getElementById(${JSON.stringify(key)});if(!node)throw Error('Missing field');node.value=${JSON.stringify(Array.isArray(value)?value.join('\n'):value)};node.dispatchEvent(new Event('input',{bubbles:true}));})()`);
  }
}
async function preview() {await evaluate(`document.querySelector('#preview').click()`);return await until(`document.querySelector('#error')?.innerText || (!!document.querySelector('#start')&&!document.querySelector('#start').disabled&&'planned')`);}
async function resultData() {
  await evaluate(`document.querySelector('#result details').open=true`);await until(`!!document.querySelector('#result pre.json')`);
  return JSON.parse(await evaluate(`document.querySelector('#result pre.json').textContent`));
}
async function execute() {
  await evaluate(`document.querySelector('#start').click()`);
  await until(`document.querySelector('#error')?.innerText || (!!document.querySelector('#result')&&'complete')`,60000);
  const error=await evaluate(`document.querySelector('#error')?.innerText`);assert(!error,error);return await resultData();
}
async function run(operation,fields) {await select(operation,fields);assert.equal(await preview(),'planned');return await execute();}
try {
  const before=await readdir(qa);
  await select('split',{input,size:'2B',basename:'A.zip'});assert.equal(await preview(),'planned');
  assert.deepEqual(await readdir(qa),before);check('preview creates no files');
  // Editing a field invalidates authorization in the actual UI.
  await evaluate(`(()=>{const node=document.querySelector('#size');node.value='3B';node.dispatchEvent(new Event('input',{bubbles:true}));})()`);
  assert.equal(await evaluate(`!!document.querySelector('#start')`),false);check('editing invalidates preview');
  await select('split',{input,size:'2B',basename:'A.zip'});assert.equal(await preview(),'planned');
  const split=await execute();assert.equal(split.part_count,'3');assert.equal(split.sha256,hash);assert.equal(split.verification_scope,'full_stream');
  assert.deepEqual(await readFile(input),Buffer.from('hello'));check('offline split and full-stream verification');
  const manifest=join(qa,'A.zip.afparts.json');
  const merged=await run('merge',{input:manifest,basename:'restored.zip'});assert.equal(merged.sha256,hash);assert.deepEqual(await readFile(join(qa,'restored.zip')),Buffer.from('hello'));check('merge round trip');
  const shard=await run('reshard',{input:manifest,size:'3B',basename:'B.zip'});assert.equal(shard.part_count,'2');check('direct ReShard');
  const rebuilt=await run('rebuild',{input,manifest,part:'2',basename:'rebuilt.zip.002'});assert.equal(rebuilt.verification_scope,'target_range_only');assert.deepEqual(await readFile(join(qa,'rebuilt.zip.002')),Buffer.from('ll'));check('range-only rebuild');
  const backup=join(qa,'B.zip.afparts.json');
  // Target manifest-only folder models a missing group without deleting fixtures.
  const missing=join(qa,'missing');await mkdir(missing);const target=join(missing,'A.zip.afparts.json');await writeFile(target,await readFile(manifest),{flag:'wx'});
  const rescued=await run('rescue',{input:target,sources:[backup],part:'2',expectedSha256:hash});
  assert.equal(rescued.verification_scope,'target_range_only');assert.deepEqual(await readFile(join(missing,'A.zip.002')),Buffer.from('ll'));check('cross-set rescue across source boundary');
  const covered=await run('coverage',{input:target,sources:[backup],part:'2'});assert.equal(covered.complete_coverage,true);assert.equal(covered.covered_bytes,'2');check('coverage map exact intervals');
  const verified=await run('verify',{input:manifest,expectedSha256:hash});assert.equal(verified.sha256,hash);check('external digest verification');
  await select('split',{input,size:'2B',basename:'A.zip'});assert.match(await preview(),/OUTPUT_EXISTS/);assert.deepEqual(await readFile(join(qa,'A.zip.002')),Buffer.from('ll'));check('existing outputs are preserved');
  const malformed=join(qa,'evil.afparts.json');const evil=JSON.parse(await readFile(manifest,'utf8'));evil.parts[0].filename='../escape.001';await writeFile(malformed,JSON.stringify(evil),{flag:'wx'});
  await select('rescue',{input:malformed,sources:[backup],part:'1'});assert.match(await preview(),/MANIFEST_INVALID/);check('unsafe manifest rejected');
  const incomplete=join(qa,'incomplete');await mkdir(incomplete);const incompleteManifest=join(incomplete,'B.zip.afparts.json');await writeFile(incompleteManifest,await readFile(backup),{flag:'wx'});await writeFile(join(incomplete,'B.zip.001'),Buffer.from('hel'),{flag:'wx'});
  const incompleteMap=await run('coverage',{input:target,sources:[incompleteManifest],part:'2'});assert.equal(incompleteMap.complete_coverage,false);assert.equal(await evaluate(`document.querySelector('#result').classList.contains('good')`),false);check('incomplete coverage never shows verified success');
  await select('rescue',{input:target,sources:[incompleteManifest],part:'1'});assert.match(await preview(),/MISSING_COVERAGE/);check('unverified partial sources cannot repair');
  // Bounded synthetic file makes cancellation observable; only test-owned data.
  const large=join(qa,'cancel.zip');const file=await open(large,'wx');await file.truncate(512*1024*1024);await file.close();
  const cancelDir=join(qa,'cancel');await mkdir(cancelDir);
  await select('split',{input:large,size:'64MiB',outputDir:cancelDir});assert.equal(await preview(),'planned');
  await evaluate(`document.querySelector('#start').click()`);await until(`!!document.querySelector('#cancel')`);
  // Wait for the durable start record, rather than cancelling during task setup.
  const startDeadline=Date.now()+15000;let durable=false;
  while(Date.now()<startDeadline&&!durable) {for(const name of await readdir(cancelDir)){if(!name.startsWith('.eaa-'))continue;const records=await readdir(join(cancelDir,name));durable=records.includes('start.json');}if(!durable)await delay(5);}
  assert(durable,'Copy must reach its durable restart record before this cancellation scenario');
  await evaluate(`document.querySelector('#cancel').click()`);await until(`!!document.querySelector('#error')`);assert.match(await evaluate(`document.querySelector('#error').textContent`),/CANCELLED/);assert.equal((await stat(large)).size,512*1024*1024);check('desktop cancellation preserves input');
  const tasks=await run('transactions',{input:cancelDir});assert.equal(tasks.length,1);assert.equal(tasks[0].restart_record_present,true);check('retained-task discovery after cancellation');
  const restarted=await run('restart',{input:tasks[0].directory});assert.equal(restarted.part_count,'8');check('desktop restart re-executes cancelled copying');
  const recovered=await run('recover',{input:restarted.staging_directory});assert.equal(recovered.verification_scope,'full_stream');check('desktop committed recovery is idempotent');
  // A token cannot bypass preview or replay a previous operation.
  const ipcError=await evaluate(`window.__TAURI_INTERNALS__.invoke('execute_plan',{token:'forged',jobId:crypto.randomUUID()}).then(()=>null,e=>e)`);assert.equal(ipcError.code,'PLAN_REQUIRED');check('forged plan token rejected');
  const denied=await evaluate(`window.__TAURI_INTERNALS__.invoke('plugin:fs|read_file',{path:'ignored'}).then(()=>null,e=>String(e))`);assert(denied);check('arbitrary filesystem plugin unavailable');
  // Native drag/drop forwarding uses Tauri's registered event listener. OS drag
  // delivery is a separate manual check, explicitly not claimed by this test.
  await select('split',{});
  check('all desktop operation forms render');
  // Local preference survives reload; network remains offline throughout.
  await evaluate(`localStorage.setItem('language','zh-CN');location.reload()`);await until(`document.querySelector('h1')?.textContent==='拆分文件'`);check('Chinese preference persists offline');
  await evaluate(`document.querySelector('.language').click()`);await until(`document.querySelector('h1')?.textContent==='Split a file'`);check('English language switch');
  const screenshot=await cdp('Page.captureScreenshot',{format:'png'});await writeFile(join(qa,'desktop-en.png'),Buffer.from(screenshot.data,'base64'),{flag:'wx'});
  assert.deepEqual(consoleErrors,[],'No uncaught webview errors');
  const external=requests.filter(url=>!url.startsWith('http://tauri.localhost/')&&!url.startsWith('http://ipc.localhost/')&&!url.startsWith('ipc:')&&!url.startsWith('data:'));
  assert.deepEqual(external,[]);check('no external webview requests while offline');
  await writeFile(join(qa,'VALIDATION.json'),JSON.stringify({checks,consoleErrors,externalRequests:external,offline:true,osDragDropTested:false,cleanMachineTested:false},null,2),{flag:'wx'});
  console.log(JSON.stringify({fixtureDirectory:qa,passed:checks.length}));
} finally {await cdp('Network.emulateNetworkConditions',{offline:false,latency:0,downloadThroughput:-1,uploadThroughput:-1}).catch(()=>{});socket.close();}
