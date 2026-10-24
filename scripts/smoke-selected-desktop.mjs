// Run only against the opt-in QA desktop, never an ordinary browser.
import assert from 'node:assert/strict';
import {mkdir,writeFile,readFile,readdir} from 'node:fs/promises';
import {join,resolve} from 'node:path';
import {randomUUID,createHash} from 'node:crypto';
const port=Number(process.argv[2]||9451);
assert(Number.isInteger(port)&&port>=1024&&port<=65535);
const targets=await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
const matches=targets.filter(t=>t.type==='page'&&t.url==='http://tauri.localhost/'&&/Archive|压缩/.test(t.title));
assert.equal(matches.length,1,'Require the explicitly launched QA webview');
const socket=new WebSocket(matches[0].webSocketDebuggerUrl);await new Promise((r,j)=>{socket.onopen=r;socket.onerror=j;});
let serial=0;const pending=new Map(),requests=[],consoleErrors=[];
socket.onmessage=event=>{const data=JSON.parse(event.data);if(data.id){const call=pending.get(data.id);if(call){clearTimeout(call.timer);pending.delete(data.id);data.error?call.reject(Error(JSON.stringify(data.error))):call.resolve(data.result);}}
  else if(data.method==='Network.requestWillBeSent')requests.push(data.params.request.url);
  else if(data.method==='Runtime.exceptionThrown')consoleErrors.push(data.params.exceptionDetails.text);};
function cdp(method,params={}) {return new Promise((resolve,reject)=>{const id=++serial;const timer=setTimeout(()=>{pending.delete(id);reject(Error(`CDP timeout: ${method}`));},60000);pending.set(id,{resolve,reject,timer});socket.send(JSON.stringify({id,method,params}));});}
async function evaluate(expression) {const result=await cdp('Runtime.evaluate',{expression,awaitPromise:true,returnByValue:true});if(result.exceptionDetails)throw Error(result.exceptionDetails.exception?.description??result.exceptionDetails.text);return result.result.value;}
const delay=ms=>new Promise(r=>setTimeout(r,ms));
async function until(expression) {const end=Date.now()+30000;while(Date.now()<end){const result=await evaluate(expression);if(result)return result;await delay(30);}throw Error(`Timed out: ${expression}`);}
const qa=resolve('.artifacts',`selected-desktop-e2e-${randomUUID()}`);await mkdir(qa);const checks=[];
const check=name=>{checks.push(name);console.log(`PASS ${name}`);};
async function click(selector) {return evaluate(`(()=>{const node=document.querySelector(${JSON.stringify(selector)});if(!node||node.disabled)throw Error('Unavailable control: '+${JSON.stringify(selector)});node.click();})()`);}
async function field(id,value) {await evaluate(`(()=>{const node=document.getElementById(${JSON.stringify(id)});node.value=${JSON.stringify(value)};node.dispatchEvent(new Event('input',{bubbles:true}));})()`);}
async function select(op) {await click(`[data-operation="${op}"]`);}
async function add(paths) {await click('#enter-parts');await field('part-path-input',paths.map(p=>`"${p}"`).join('\n'));await click('#confirm-parts');}
async function result() {await until(`!!document.querySelector('#result')||!!document.querySelector('#error')`);assert.equal(await evaluate(`document.querySelector('#error')?.textContent??''`),'');await evaluate(`document.querySelector('#result details').open=true`);await until(`!!document.querySelector('#result pre.json')`);return JSON.parse(await evaluate(`document.querySelector('#result pre.json').textContent`));}
async function preview() {await click('#preview');return until(`document.querySelector('#error')?.textContent || (!!document.querySelector('#start')&&!document.querySelector('#start').disabled&&'planned')`);}
async function execute() {await click('#start');return result();}
async function reset() {await select('split');await select('merge');}
await cdp('Runtime.enable');await cdp('Network.enable');await cdp('Network.emulateNetworkConditions',{offline:true,latency:0,downloadThroughput:-1,uploadThroughput:-1});
try {
  await evaluate(`window.__qaReload=true;localStorage.setItem('language','zh-CN');location.reload()`);await until(`!window.__qaReload&&!!document.querySelector('#preview')`);
  const input=join(qa,'sample.zip'),output=join(qa,'output');await mkdir(output);const bytes=Buffer.from('跨目录 independent USB volumes');await writeFile(input,bytes,{flag:'wx'});const hash=createHash('sha256').update(bytes).digest('hex');
  await field('input',input);await field('size','12B');assert.equal(await preview(),'planned');const split=await execute();assert.equal(split.part_count,'3');
  const manifest=join(qa,'sample.zip.afparts.json'),parts=[];
  for(let i=1;i<=3;i++){const folder=join(qa,`USB_${i}`);await mkdir(folder);const path=join(folder,`sample.zip.00${i}`);await writeFile(path,await readFile(join(qa,`sample.zip.00${i}`)),{flag:'wx'});parts.push(path);}
  await select('merge');await add([parts[2],parts[0]]);await add([parts[1],parts[0]]);
  assert.equal(await evaluate(`document.querySelectorAll('.source-list input[type=checkbox]').length`),3);assert.equal(await evaluate(`document.querySelector('#preview').disabled`),true);check('repeated additions across three directories deduplicate paths and require output');
  await click('#check-parts');let verified=await result();assert.equal(verified.sha256,hash);assert.equal(verified.has_manifest,false);assert.equal(await evaluate(`document.querySelector('#result').classList.contains('good')`),false);assert.deepEqual(await readdir(output),[]);check('read-only check before output reports limited trust without writing');
  await add([manifest]);await click('#check-parts');verified=await result();assert.equal(verified.has_manifest,true);assert.equal(verified.sha256,hash);check('explicit manifest validates all independently selected bytes');
  await field('outputDir',output);await field('basename','restored.zip');assert.equal(await preview(),'planned');assert.deepEqual(await readdir(output),[]);check('selected merge preview writes no files');
  const merged=await execute();assert.equal(merged.sha256,hash);assert.deepEqual(await readFile(join(output,'restored.zip')),bytes);check('selected merge publishes exact bytes only to chosen output');
  await select('reshard');assert.equal(await evaluate(`document.querySelectorAll('.source-list input[type=checkbox]').length`),3);await field('size','9B');await field('basename','new.zip');assert.equal(await preview(),'planned');const shard=await execute();assert.equal(shard.sha256,hash);assert.equal(shard.part_count,String(Math.ceil(bytes.length/9)));check('selection survives mode change and direct ReShard');
  await select('verify');await field('expectedSha256',hash);assert.equal(await preview(),'planned');verified=await execute();assert.equal(verified.verification_level,'verified_against_original');check('external digest verifies selected full stream');
  await field('expectedSha256','0'.repeat(64));await click('#check-parts');await until(`!!document.querySelector('#error')`);assert.match(await evaluate(`document.querySelector('#error').textContent`),/HASH_MISMATCH/);check('wrong original digest cannot show success');
  await reset();await add([manifest,parts[0],parts[2]]);await click('#check-parts');await until(`!!document.querySelector('#error')`);assert.match(await evaluate(`document.querySelector('#error').textContent`),/PART_MISSING/);check('missing explicit part is not silently loaded from manifest folder');
  const duplicateFolder=join(qa,'duplicate');await mkdir(duplicateFolder);const duplicate=join(duplicateFolder,'sample.zip.001');await writeFile(duplicate,await readFile(parts[0]),{flag:'wx'});
  await reset();await add([...parts,duplicate]);await click('#check-parts');await until(`!!document.querySelector('#error')`);assert.match(await evaluate(`document.querySelector('#error').textContent`),/PART_DUPLICATE/);check('same index in different folders is rejected');
  const other=join(duplicateFolder,'other.zip.004');await writeFile(other,'x',{flag:'wx'});await reset();await add([...parts,other]);await click('#check-parts');await until(`!!document.querySelector('#error')`);assert.match(await evaluate(`document.querySelector('#error').textContent`),/PART_GROUP_MISMATCH/);check('mixed filename groups are rejected');
  await reset();await add([manifest,...parts]);await field('outputDir',output);await field('basename','remove-check.zip');assert.equal(await preview(),'planned');await click('.source-list input[type=checkbox]');await click('#remove-parts');assert.equal(await evaluate(`!!document.querySelector('#start')`),false);assert.equal(await evaluate(`document.querySelectorAll('.source-list input[type=checkbox]').length`),2);check('removing an input invalidates write authorization');
  await add(parts);await field('expectedSha256',hash);await click('#check-parts');await result();
  // Capture both rendered languages; no OS drag delivery is inferred from CDP.
  await evaluate(`window.scrollTo(0,0)`);let screenshot=await cdp('Page.captureScreenshot',{format:'png'});await writeFile(join(qa,'classic-selected-zh.png'),Buffer.from(screenshot.data,'base64'),{flag:'wx'});
  await click('.language');assert.match(await evaluate(`document.querySelector('h1').textContent`),/Merge/);screenshot=await cdp('Page.captureScreenshot',{format:'png'});await writeFile(join(qa,'classic-selected-en.png'),Buffer.from(screenshot.data,'base64'),{flag:'wx'});check('classic selected-file interface renders in Chinese and English');
  for(let i=0;i<parts.length;i++)assert.deepEqual(await readFile(parts[i]),await readFile(join(qa,`sample.zip.00${i+1}`)));assert.deepEqual(await readFile(input),bytes);check('all original and selected volumes remain unchanged');
  const external=requests.filter(url=>!url.startsWith('http://tauri.localhost/')&&!url.startsWith('http://ipc.localhost/')&&!url.startsWith('ipc:')&&!url.startsWith('data:'));assert.deepEqual(external,[]);assert.deepEqual(consoleErrors,[]);check('offline webview has no external requests or uncaught errors');
  await writeFile(join(qa,'VALIDATION.json'),JSON.stringify({checks,passed:checks.length,offline:true,consoleErrors,externalRequests:external,physicalUsbTested:false,osDragDropTested:false},null,2),{flag:'wx'});console.log(JSON.stringify({fixtureDirectory:qa,passed:checks.length}));
} finally {await cdp('Network.emulateNetworkConditions',{offline:false,latency:0,downloadThroughput:-1,uploadThroughput:-1}).catch(()=>{});socket.close();}
