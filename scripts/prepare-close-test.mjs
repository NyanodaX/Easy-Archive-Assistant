// Prepare an owned long-running split, then close the native window manually
// with Alt+F4. This tests cancellation on close rather than killing the process.
import {mkdir,open,writeFile,readdir} from 'node:fs/promises';
import {resolve,join} from 'node:path';
import {randomUUID} from 'node:crypto';
import assert from 'node:assert/strict';
const port=Number(process.argv[2]||9446);
const targets=await(await fetch(`http://127.0.0.1:${port}/json/list`)).json();
const pages=targets.filter(t=>t.type==='page'&&t.url==='http://tauri.localhost/'&&/Archive|压缩/.test(t.title));assert.equal(pages.length,1);
const socket=new WebSocket(pages[0].webSocketDebuggerUrl);await new Promise((r,j)=>{socket.onopen=r;socket.onerror=j;});
let serial=0;const pending=new Map();socket.onmessage=event=>{const data=JSON.parse(event.data);if(data.id&&pending.has(data.id)){const call=pending.get(data.id);pending.delete(data.id);data.error?call.reject(Error(JSON.stringify(data.error))):call.resolve(data.result);}};
function evaluate(expression){return new Promise((resolve,reject)=>{const id=++serial;pending.set(id,{resolve:r=>r.exceptionDetails?reject(Error(r.exceptionDetails.text)):resolve(r.result.value),reject});socket.send(JSON.stringify({id,method:'Runtime.evaluate',params:{expression,awaitPromise:true,returnByValue:true}}));});}
const qa=resolve('.artifacts',`desktop-close-${randomUUID()}`);await mkdir(qa);const input=join(qa,'close.zip');const file=await open(input,'wx');await file.truncate(8*1024*1024*1024);await file.close();
const request={operation:'split',input,outputDir:qa,basename:'close-output.zip',size:'512MiB',policy:'hard',tolerance:'5',alignment:'',manifest:'',sources:[],part:'',expectedSha256:'',metadataOnly:false};
const plan=await evaluate(`window.__TAURI_INTERNALS__.invoke('plan_operation',{request:${JSON.stringify(request)},jobId:crypto.randomUUID()})`);
await evaluate(`window.__TAURI_INTERNALS__.invoke('execute_plan',{token:${JSON.stringify(plan.token)},jobId:crypto.randomUUID()}).then(r=>{window.__closeTestResult=r},e=>{window.__closeTestError=e});true`);
const deadline=Date.now()+15000;let task;
while(Date.now()<deadline&&!task){for(const name of await readdir(qa)){if(name.startsWith('.eaa-')&&(await readdir(join(qa,name))).includes('start.json'))task=join(qa,name);}if(!task)await new Promise(r=>setTimeout(r,5));}
assert(task);assert.equal(await evaluate(`!!window.__closeTestResult||!!window.__closeTestError`),false);
await writeFile(join(qa,'QA.json'),JSON.stringify({input,task,expectedSize:8*1024*1024*1024},null,2),{flag:'wx'});
console.log(JSON.stringify({ready:true,fixtureDirectory:qa,task}));socket.close();
