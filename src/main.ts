import {invoke, isTauri} from '@tauri-apps/api/core';
import {listen} from '@tauri-apps/api/event';
import {getCurrentWebview} from '@tauri-apps/api/webview';
import {Workflow, freshRequest, formatBytes, progressPercent, resultTone, addSelectedFiles, removeSelectedFiles, isVolumePath, isManifestPath, type Operation, type Data, type Request} from './model.ts';
import {tr, type Language, type Key} from './i18n.ts';
import './style.css';

let language: Language=localStorage.getItem('language')==='en'?'en':'zh-CN';
let request=freshRequest(); const flow=new Workflow(); let closing=false;
const selectedRows=new Set<string>();
const root=document.querySelector<HTMLDivElement>('#app')!;
const t=(key:Key)=>tr(key,language);
function el<K extends keyof HTMLElementTagNameMap>(tag:K, className='', text?:string): HTMLElementTagNameMap[K] {
  const node=document.createElement(tag); if(className) node.className=className; if(text!==undefined) node.textContent=text; return node;
}
function button(text:string, action:()=>void, className=''): HTMLButtonElement {
  const node=el('button',className,text); node.type='button'; node.addEventListener('click',action); return node;
}
function appIcon() {
  const box=el('span','app-icon'); box.setAttribute('aria-hidden','true');
  // Static application-owned vector; user paths and errors always use textContent.
  box.innerHTML='<svg viewBox="0 0 40 40" fill="none"><rect x="7" y="9" width="26" height="25" rx="5" stroke="currentColor" stroke-width="2"/><path d="M6 15h28M20 16v4m0 3v4m0 3v3M13 5h14" stroke="currentColor" stroke-width="2" stroke-linecap="round"/></svg>';
  return box;
}
const readOnly=(operation=request.operation)=>['coverage','verify','transactions'].includes(operation);
const groupOperation=()=>['merge','verify','reshard'].includes(request.operation);
const canPreview=()=>!flow.busy&&!!request.input&&!(request.partPaths.length>0&&!readOnly()&&!request.outputDir);
function selectOperation(operation:Operation, input?:string) {
  if(flow.busy) return;
  const previous=request;const preserve=groupOperation()&&['merge','verify','reshard'].includes(operation)&&input===undefined;
  request=freshRequest(operation);
  if(preserve)request={...request,input:previous.input,partPaths:[...previous.partPaths],outputDir:previous.outputDir,expectedSha256:previous.expectedSha256};
  else request.input=input??'';
  selectedRows.clear(); flow.invalidate(); render();
  document.querySelector<HTMLInputElement>('#input')?.focus();
}
function invalidate() { flow.invalidate(); renderStatus(); }
async function choose(kind:string): Promise<string|null> {
  if(!isTauri()) { showError({code:'DESKTOP_REQUIRED',message:{en:tr('noTauri','en'),zh_cn:tr('noTauri','zh-CN')}}); return null; }
  try { return await invoke<string|null>('choose_path',{kind}); } catch(error) { showError(error); return null; }
}
function applyInput(path:string) {
  if(flow.busy) return;
  // Selection is an explicit path, never a shell command or archive-member read.
  request.input=path; invalidate(); render(); document.querySelector<HTMLInputElement>('#input')?.focus();
}
function addGroupFiles(paths:string[]) {
  if(flow.busy||!paths.length)return;
  try {
    const current=groupOperation()?request:freshRequest('merge');
    request=addSelectedFiles(current,paths);selectedRows.clear();flow.invalidate();render();
  } catch(error) {
    const limited=String(error).includes('LIMIT_EXCEEDED');const key=limited?'selectionLimit':'invalidSelection';
    showError({code:limited?'LIMIT_EXCEEDED':'INVALID_GROUP_SELECTION',message:{en:tr(key,'en'),zh_cn:tr(key,'zh-CN')}});
  }
}
async function chooseGroupFiles() {
  if(!isTauri())return;
  try {addGroupFiles(await invoke<string[]>('choose_paths'));}catch(error){showError(error);}
}
function enterPaths() {
  const dialog=el('dialog','paths-dialog');dialog.setAttribute('aria-labelledby','paths-label');
  const label=el('label','',t('pathsLabel'));label.id='paths-label';label.htmlFor='part-path-input';
  const input=el('textarea');input.id='part-path-input';input.rows=7;input.spellcheck=false;input.maxLength=4*1024*1024;
  const controls=el('div','controls');const add=button(t('add'),()=>{const paths=input.value.split(/\r?\n/).map(line=>{const path=line.trim();return path.startsWith('"')&&path.endsWith('"')?path.slice(1,-1):path;}).filter(Boolean);dialog.close();addGroupFiles(paths);},'primary');add.id='confirm-parts';
  controls.append(add,button(t('close'),()=>dialog.close(),'secondary'));dialog.append(label,input,controls);dialog.addEventListener('close',()=>dialog.remove());document.body.append(dialog);dialog.showModal();input.focus();
}
function selectedPartsForm():HTMLElement {
  const box=el('fieldset','part-selection');box.append(el('legend','',t('selectedParts')));
  const controls=el('div','controls file-controls');
  const add=button(t('addParts'),()=>{void chooseGroupFiles();},'secondary compact');add.id='add-parts';
  const check=button(t('checkParts'),()=>{void checkSelection();},'secondary compact');check.id='check-parts';check.disabled=!request.input||request.partPaths.length===0;
  const manifest=button(t('chooseManifest'),()=>{void choose('manifest').then(path=>{if(path)addGroupFiles([path]);});},'secondary compact');
  const remove=button(t('removeParts'),()=>{request=removeSelectedFiles(request,selectedRows);selectedRows.clear();invalidate();render();},'secondary compact');remove.id='remove-parts';remove.disabled=selectedRows.size===0;
  const clear=button(t('clearParts'),()=>{request={...request,partPaths:[],input:''};selectedRows.clear();invalidate();render();},'secondary compact');clear.disabled=request.partPaths.length===0;
  const enter=button(t('enterPaths'),enterPaths,'secondary compact');enter.id='enter-parts';
  controls.append(add,enter,manifest,check,remove,clear);box.append(controls,el('p','hint',t(request.partPaths.length?'selectionHint':'autoHint')));
  if(request.partPaths.length) {
    const list=el('div','source-list');list.setAttribute('role','table');list.setAttribute('aria-label',t('selectedParts'));
    const header=el('div','file-row file-list-head');header.setAttribute('role','row');
    for(const key of ['select','fileName','location'] as const){const cell=el('span','',t(key));cell.setAttribute('role','columnheader');header.append(cell);}list.append(header);
    appendPaged(list,request.partPaths,path=>{const row=el('div','file-row');row.setAttribute('role','row');const name=path.split(/[\\/]/).pop()!;
      const choose=el('input');choose.type='checkbox';choose.checked=selectedRows.has(path);choose.setAttribute('aria-label',`${t('select')} ${path}`);
      choose.addEventListener('change',()=>{if(choose.checked)selectedRows.add(path);else selectedRows.delete(path);remove.disabled=selectedRows.size===0;});
      const cell=el('span');cell.setAttribute('role','cell');cell.append(choose);
      const filename=el('span','filename',name);filename.setAttribute('role','cell');
      const directory=el('span','source-folder',path.slice(0,-name.length));directory.title=path;directory.setAttribute('role','cell');row.append(cell,filename,directory);return row;
    });box.append(list,el('p','hint',`${t('selectedCount')}: ${request.partPaths.length}`));
  }
  return box;
}
function field(key: keyof Request, label:Key, hint?:Key, kind?:string, placeholder?:Key): HTMLElement {
  const wrap=el('div','field'); const lab=el('label','',t(label)); lab.htmlFor=key; wrap.append(lab);
  const row=el('div','input-row'); const input=el('input'); input.id=key; input.value=String(request[key]); input.autocomplete='off'; input.spellcheck=false;
  if(placeholder) input.placeholder=t(placeholder);
  if(key==='part'||key==='tolerance') input.inputMode='numeric';
  if(hint) { input.setAttribute('aria-describedby',`${key}-hint`); const text=el('small','hint',t(hint)); text.id=`${key}-hint`; wrap.append(text); }
  input.addEventListener('input',()=>{ (request as any)[key]=input.value; invalidate(); }); row.append(input);
  if(kind) row.append(button(t('browse'),()=>{ void choose(kind).then(path=>{ if(path) { (request as any)[key]=path; input.value=path; invalidate(); } }); },'secondary compact'));
  wrap.insertBefore(row,wrap.children[1]??null); return wrap;
}
function checkField(label:Key, checked:boolean, onChange:(value:boolean)=>void) {
  const lab=el('label','check'); const input=el('input'); input.type='checkbox'; input.checked=checked; input.addEventListener('change',()=>{onChange(input.checked);invalidate();}); lab.append(input,el('span','',t(label))); return lab;
}
function buildForm(): HTMLElement {
  const form=el('fieldset','form'); form.disabled=flow.busy; const op=request.operation;
  const fileType=op==='split'?'archive':['rescue','coverage'].includes(op)?'manifest':['transactions','recover','restart'].includes(op)?'directory':'file';
  const label:Key=op==='rebuild'?'original':['rescue','coverage'].includes(op)?'target':op==='transactions'?'scanDir':['recover','restart'].includes(op)?'taskDir':['merge','reshard','verify'].includes(op)?'group':'input';
  form.append(field('input',label,undefined,fileType));
  if(groupOperation())form.append(selectedPartsForm());
  if(op==='rebuild') form.append(field('manifest','manifest',undefined,'manifest'));
  if(op==='rescue'||op==='coverage') {
    const wrap=el('div','field'); const lab=el('label','',t('sources')); lab.htmlFor='sources';
    const area=el('textarea'); area.id='sources'; area.rows=3; area.value=request.sources.join('\n'); area.spellcheck=false;
    area.addEventListener('input',()=>{request.sources=area.value.split(/\r?\n/).filter(v=>v.length>0);invalidate();});
    wrap.append(lab,area,button(t('addSource'),()=>{void choose('manifest').then(path=>{if(path && !request.sources.includes(path)) {request.sources.push(path);area.value=request.sources.join('\n');invalidate();}});},'secondary compact')); form.append(wrap);
  }
  if(['split','merge','reshard','rebuild','rescue'].includes(op)) {
    const row=el('div','two-col'); row.append(field('outputDir','outputDir',undefined,'directory',request.partPaths.length?'requiredOutput':'defaultDir'),field('basename','basename',undefined,undefined,op==='reshard'?'requiredName':'defaultName')); form.append(row);
    if(request.partPaths.length)form.querySelector<HTMLInputElement>('#outputDir')!.required=true;
  }
  if(op==='split'||op==='reshard') {
    const row=el('div','two-col'); row.append(field('size','size','sizeHint'));
    const wrap=el('div','field'); const lab=el('label','',t('policy')); lab.htmlFor='policy'; const select=el('select');select.id='policy';
    for(const [value,key] of [['hard','hard'],['balanced','balanced']] as const) {const option=el('option','',t(key));option.value=value;select.append(option);} select.value=request.policy;
    select.addEventListener('change',()=>{request.policy=select.value;invalidate();render();}); wrap.append(lab,select);row.append(wrap);form.append(row);
    if(request.policy==='balanced') form.append(field('tolerance','tolerance'));
    form.append(checkField('alignment',request.alignment==='1MiB',v=>request.alignment=v?'1MiB':''));
  }
  if(op==='rebuild'||op==='rescue'||op==='coverage') form.append(field('part','part',undefined,undefined,op==='coverage'?'allParts':undefined));
  if(['merge','reshard','rescue','verify'].includes(op)) form.append(field('expectedSha256','hash','hashHint'));
  if(op==='coverage') form.append(checkField('metadata',request.metadataOnly,v=>request.metadataOnly=v));
  return form;
}
function showAbout() {
  const dialog=el('dialog','about-dialog');dialog.setAttribute('aria-labelledby','about-title');
  const title=el('h2','',t('name'));title.id='about-title';dialog.append(title,el('p','',t('aboutText')),button(t('close'),()=>dialog.close(),'secondary'));dialog.addEventListener('close',()=>dialog.remove());document.body.append(dialog);dialog.showModal();
}
function render() {
  document.documentElement.lang=language; document.title=t('name')+'_V0.0.1'; root.replaceChildren();
  const shell=el('div','shell');const top=el('header','application-header');const brand=el('div','brand');brand.append(appIcon(),el('strong','',`${t('name')}  V0.0.1`));
  const actions=el('div','controls');actions.append(button(language==='zh-CN'?'English':'中文',()=>{language=language==='zh-CN'?'en':'zh-CN';localStorage.setItem('language',language);render();},'language'),button(t('about'),showAbout,'secondary compact'));top.append(brand,actions);shell.append(top);
  const nav=el('nav','operation-tabs');nav.setAttribute('role','tablist');nav.setAttribute('aria-label',t('tools'));
  const operations:Operation[]=['split','merge','reshard','rebuild','rescue','coverage','verify','transactions','recover','restart'];
  for(const op of operations){const b=button(t(op),()=>selectOperation(op),'nav-button');b.dataset.operation=op;b.disabled=flow.busy;b.id=`tab-${op}`;b.setAttribute('role','tab');b.setAttribute('aria-controls','operation-panel');b.setAttribute('aria-selected',String(op===request.operation));b.tabIndex=op===request.operation?0:-1;nav.append(b);}
  nav.addEventListener('keydown',event=>{if(flow.busy||!['ArrowLeft','ArrowRight','Home','End'].includes(event.key))return;event.preventDefault();const index=operations.indexOf(request.operation);const next=event.key==='Home'?0:event.key==='End'?operations.length-1:(index+(event.key==='ArrowRight'?1:-1)+operations.length)%operations.length;selectOperation(operations[next]);document.querySelector<HTMLButtonElement>(`[data-operation="${operations[next]}"]`)!.focus();});shell.append(nav);
  const main=el('main','main');main.id='operation-panel';main.setAttribute('role','tabpanel');main.setAttribute('aria-labelledby',`tab-${request.operation}`);
  const heading=el('div','heading');heading.append(el('h1','',t(request.operation)),el('p','description',t((request.operation+'Desc') as Key)));main.append(heading);
  if(!['transactions','recover','restart'].includes(request.operation)) {
    const drop=button('',()=>{if(groupOperation()){void chooseGroupFiles();return;}void choose(request.operation==='split'?'archive':['rescue','coverage'].includes(request.operation)?'manifest':'file').then(path=>{if(path)applyInput(path);});},'dropzone');drop.id='dropzone';drop.disabled=flow.busy;
    drop.append(el('span','drop-symbol','↓'),el('strong','',t('drop')),el('span','hint',t('dropHint')));main.append(drop);
  }
  const setup=el('section','card setup');setup.append(el('h2','section-title',t('step1')),buildForm());
  const controls=el('div','controls');const preview=button(t('preview'),()=>{void plan();},'primary');preview.id='preview';preview.disabled=!canPreview();controls.append(preview,el('small','safety',t('formatNote')));setup.append(controls);main.append(setup);
  const status=el('div');status.id='status';main.append(status);shell.append(main);
  const footer=el('footer','statusbar');footer.append(el('span','',t('safety')),el('span','','V0.0.1 · MIT'));shell.append(footer);root.append(shell);renderStatus();
}
function kv(parent:HTMLElement,label:string,value:string) {const item=el('div','metric');item.append(el('span','metric-label',label),el('strong','metric-value',value));parent.append(item);}
function rangeText(range:Data):string { return `[${range.start}, ${range.end_exclusive})`; }
function appendPaged(parent:HTMLElement, items:any[], draw:(item:any)=>HTMLElement, pageSize=100) {
  let page=0; const rows=el('div','paged-rows');const pager=el('div','pager');
  const update=()=>{rows.replaceChildren(...items.slice(page*pageSize,(page+1)*pageSize).map(draw));pager.replaceChildren();if(items.length>pageSize) {const previous=button(t('previous'),()=>{page--;update();},'secondary compact');previous.disabled=page===0;const next=button(t('next'),()=>{page++;update();},'secondary compact');next.disabled=(page+1)*pageSize>=items.length;pager.append(previous,el('span','hint',`${t('page')} ${page+1} / ${Math.ceil(items.length/pageSize)}`),next);}};
  update();parent.append(rows,pager);
}
function outputList(parent:HTMLElement, paths:string[]) {
  if(!paths.length)return;parent.append(el('h3','',t('outputs')));
  appendPaged(parent,paths,path=>el('div','path-row',path));
}
function details(parent:HTMLElement,data:any) {
  const detail=el('details');detail.append(el('summary','',t('details')));
  // Avoid constructing a many-megabyte DOM until explicitly requested.
  detail.addEventListener('toggle',()=>{if(detail.open&&detail.children.length===1)detail.append(el('pre','json',JSON.stringify(data,null,2)));});parent.append(detail);
}
function renderPreview(parent:HTMLElement) {
  const data=flow.preview!;const card=el('section','card preview-card');card.append(el('h2','section-title',t('step2')));
  const metrics=el('div','metrics');const size=data.source?.size??data.group?.source_size??data.plan?.original_size??data.original?.size??data.source_size;
  if(size!==undefined)kv(metrics,t('sourceSize'),formatBytes(size,language));
  const count=data.plan?.part_count??data.group?.part_count??data.part_count;if(count!==undefined)kv(metrics,t('count'),String(count));
  if(data.range)kv(metrics,t('range'),rangeText(data.range));
  if(data.space) {kv(metrics,t('required'),formatBytes(data.space.required_space_bytes,language));kv(metrics,t('available'),formatBytes(data.space.available_space_bytes??undefined,language));}
  card.append(metrics);if(data.space?.sufficient===false)card.append(el('p','notice bad',t('insufficient')));
  const group=data.group??(data.explicit_selection!==undefined?data:null);
  if(group?.explicit_selection){card.append(el('h3','',t('sourceFiles')));appendPaged(card,group.parts,part=>el('div','path-row',`${part.index}  ${part.path}  —  ${formatBytes(part.size,language)}`));}
  if(group&&!group.has_manifest&&!request.expectedSha256)card.append(el('p','notice caution',t('noManifest')));
  if(data.space)card.append(el('p','hint',t('spaceNote')));
  outputList(card,data.output_files??(data.output_path?[data.output_path]:[]));
  if(data.manifest_path)card.append(el('div','path-row',data.manifest_path));
  if(data.plan?.parts) {const d=el('details');d.append(el('summary','',t('range')));appendPaged(d,data.plan.parts,p=>el('div','path-row',`${p.index}  ${rangeText(p.range)}  ${formatBytes(p.size,language)}`));card.append(d);}
  card.append(el('p','notice',t(readOnly()?'readOnly':'planNote')));details(card,data);
  const start=button(t(readOnly()?'check':'start'),()=>{void execute();},'primary');start.id='start';start.disabled=flow.phase!=='planned'||data.space?.sufficient===false;card.append(start);parent.append(card);
}
function renderCoverage(card:HTMLElement,data:Data) {
  const metrics=el('div','metrics');kv(metrics,t('covered'),formatBytes(data.covered_bytes,language));kv(metrics,t('range'),rangeText(data.requested_range));card.append(metrics);
  const bar=el('div','coverage-bar');bar.setAttribute('aria-hidden','true');
  // A visual overview is supplementary to exact, paginated text intervals.
  const total=BigInt(data.requested_range.end_exclusive)-BigInt(data.requested_range.start);
  if(total>0n) for(const segment of data.segments.slice(0,2000)) {const mark=el('span',`segment ${segment.status}`);mark.style.flexGrow=String(Number((BigInt(segment.range.end_exclusive)-BigInt(segment.range.start))*1000000n/total));mark.title=`${t(segment.status)} ${rangeText(segment.range)}`;bar.append(mark);}card.append(bar);
  const legend=el('div','legend');for(const state of ['covered','missing','pending','conflict'] as const)legend.append(el('span',`legend-${state}`,t(state)));card.append(legend);
  appendPaged(card,data.segments,segment=>{const row=el('div','coverage-row');row.append(el('span',`state ${segment.status}`,t(segment.status)),el('code','',rangeText(segment.range)),el('span','hint',segment.sources.map((s:Data)=>`${Number(s.source_index)+1} / ${s.part_index}`).join(', ')));return row;});
  card.append(el('p','notice',t('coverageNote')),el('p','hint',t('provenance')));
}
function renderTasks(card:HTMLElement,tasks:Data[]) {
  if(!tasks.length)card.append(el('p','',t('noTasks')));
  appendPaged(card,tasks,task=>{const row=el('div','task-row');row.append(el('code','',task.directory),el('span','hint',`${t('taskStatus')}: ${task.status}`));
    const actions=el('div','controls');if(task.recovery_plan_present)actions.append(button(t('recover'),()=>selectOperation('recover',task.directory),'secondary compact'));if(task.restart_record_present)actions.append(button(t('restart'),()=>selectOperation('restart',task.directory),'secondary compact'));row.append(actions);return row;});card.append(el('p','notice',t('taskNote')));
}
function renderResult(parent:HTMLElement) {
  const operation=flow.resultOperation??request.operation;const data=flow.result;const card=el('section',`card result ${resultTone(operation,data)}`);card.id='result';card.setAttribute('role','status');
  const title:Key=operation==='coverage'?(data.has_integrity_errors?'integrityError':data.complete_coverage?'checked':'incomplete'):readOnly(operation)?'checked':'finished';card.append(el('h2','',t(title)));
  if(operation==='coverage')renderCoverage(card,data);
  else if(operation==='transactions')renderTasks(card,data);
  else {
    const metrics=el('div','metrics');if(data.source_size!==undefined)kv(metrics,t('sourceSize'),formatBytes(data.source_size,language));if(data.verification_scope)kv(metrics,t('scope'),t(data.verification_scope));if(data.target_range)kv(metrics,t('range'),rangeText(data.target_range));card.append(metrics);
    if(data.verification_level)card.append(el('p','verification',t(data.verification_level==='available'?'availableLevel':data.verification_level)));
    if(data.sha256)card.append(el('code','digest',`SHA-256  ${data.sha256}`));
    outputList(card,data.output_files??[]);if(data.manifest_path)card.append(el('div','path-row',data.manifest_path));
    if(data.staging_directory)card.append(el('p','path-row',`${t('taskDir')}: ${data.staging_directory}`));
    card.append(el('p','hint',t('provenance')));
  }
  details(card,data);parent.append(card);
}
function renderStatus() {
  const status=document.querySelector<HTMLElement>('#status');if(!status)return;status.replaceChildren();
  document.querySelector<HTMLButtonElement>('#preview')!.disabled=!canPreview();
  document.querySelector<HTMLFieldSetElement>('fieldset')!.disabled=flow.busy;
  for(const node of document.querySelectorAll<HTMLButtonElement>('button.nav-button,#dropzone'))node.disabled=flow.busy;
  if(flow.preview)renderPreview(status);
  if(flow.busy) {
    const card=el('section','card progress-card');
    const heading=el('h2');heading.id='phase-label';heading.setAttribute('role','status');heading.setAttribute('aria-live','polite');card.append(heading);
    const bytes=el('p','hint');bytes.id='progress-bytes';card.append(bytes);
    const progress=el('progress');progress.id='progress-bar';progress.max=100;card.append(progress);
    const detail=el('p','hint');detail.id='progress-detail';card.append(detail);
    const cancel=button(t('cancel'),()=>{cancel.disabled=true;cancel.textContent=t('cancelling');void invoke('cancel_task',{jobId:flow.jobId});},'secondary');cancel.id='cancel';card.append(cancel);status.append(card);updateProgress();
  }
  if(flow.error) {
    const data=flow.error;const card=el('section','card error');card.id='error';card.setAttribute('role','alert');card.append(el('h2','',t('error')),el('code','error-code',String(data.code??'TASK_FAILED')),el('p','',data.message?.[language==='en'?'en':'zh_cn']??t('malformed')));
    if(data.suggestion)card.append(el('p','hint',data.suggestion[language==='en'?'en':'zh_cn']));
    if(data.recovery_task_id)card.append(el('p','',`${t('recoveryId')}: ${data.recovery_task_id}`));
    if(data.coverage_details)for(const key of ['missing_ranges','pending_ranges'])for(const range of data.coverage_details[key]??[])card.append(el('p','path-row',`${t(key==='missing_ranges'?'missing':'pending')} ${rangeText(range)}`));
    if(data.recovery_task_id||data.code==='TRANSACTION_NEEDS_RECOVERY')card.append(button(t('inspectTasks'),()=>{const folder=request.outputDir||request.input.replace(/[\\/][^\\/]*$/,'');selectOperation('transactions',folder);},'secondary'));
    status.append(card);
  }
  if(flow.result!==null)renderResult(status);
  if(!flow.busy&&!flow.preview&&!flow.error&&flow.result===null)status.append(el('p','empty-state',t('noPlan')));
}
function updateProgress() {
  const heading=document.querySelector('#phase-label');if(!heading)return;
  const p=flow.progress;const label=closing?t('closed'):flow.phase==='planning'?t('planning'):p?t(p.phase):t('working');
  if(heading.textContent!==label)heading.textContent=label;
  const bar=document.querySelector<HTMLProgressElement>('#progress-bar')!;bar.setAttribute('aria-label',label);
  if(p) {bar.value=progressPercent(p.completed_bytes,p.total_bytes);document.querySelector('#progress-bytes')!.textContent=`${formatBytes(p.completed_bytes,language)} / ${formatBytes(p.total_bytes,language)}`;
    document.querySelector('#progress-detail')!.textContent=`${p.part_index?`${t('currentPart')} ${p.part_index} · `:''}${t('rate')} ${formatBytes(flow.rate,language)}/s`;
  }
}
function normalizeError(error:unknown): Data {
  if(error&&typeof error==='object')return error as Data;
  try {const value=JSON.parse(String(error));if(value&&typeof value==='object')return value;}catch{}
  return {code:'TASK_FAILED',message:{en:tr('malformed','en'),zh_cn:tr('malformed','zh-CN')}};
}
function showError(error:unknown) {flow.failed(flow.jobId,normalizeError(error));renderStatus();}
async function plan() {
  if(flow.busy)return; const id=crypto.randomUUID();flow.begin('planning',id);renderStatus();
  try {const result=await invoke<Data>('plan_operation',{request,jobId:id});flow.planned(id,result);}catch(error){flow.failed(id,normalizeError(error));}renderStatus();
}
async function checkSelection() {
  if(flow.busy||!request.partPaths.length)return;
  let id=crypto.randomUUID();flow.begin('planning',id);renderStatus();
  try {
    const planned=await invoke<Data>('plan_operation',{request:{...request,operation:'verify'},jobId:id});
    id=crypto.randomUUID();flow.begin('running',id);renderStatus();
    const result=await invoke<Data>('execute_plan',{token:planned.token,jobId:id});flow.completed(id,result,'verify');
  } catch(error){flow.failed(id,normalizeError(error));}
  renderStatus();
}
async function execute() {
  if(flow.phase!=='planned'||!flow.token)return;const token=flow.token;const id=crypto.randomUUID();flow.begin('running',id);renderStatus();
  try {const result=await invoke<Data>('execute_plan',{token,jobId:id});flow.completed(id,result,request.operation);}catch(error){flow.failed(id,normalizeError(error));}renderStatus();
  (document.querySelector('#result,#error') as HTMLElement|null)?.scrollIntoView({behavior:'smooth',block:'nearest'});
}
render();
if(isTauri()) {
  void listen<Data>('archive-progress',event=>{flow.acceptProgress(event.payload.jobId,event.payload.progress);if(flow.busy)updateProgress();});
  void listen('archive-closing',()=>{closing=true;renderStatus();});
  void getCurrentWebview().onDragDropEvent(event=>{
    const drop=document.querySelector('#dropzone');
    if(event.payload.type==='enter'||event.payload.type==='over')drop?.classList.add('drag-over');else drop?.classList.remove('drag-over');
    if(event.payload.type==='drop'&&!flow.busy) {
      const paths=event.payload.paths;
      if(paths.every(path=>isVolumePath(path)||isManifestPath(path))&&(paths.some(isVolumePath)||groupOperation())){addGroupFiles(paths);return;}
      if(event.payload.paths.length!==1) {showError({code:'ONE_INPUT_REQUIRED',message:{en:tr('addOne','en'),zh_cn:tr('addOne','zh-CN')}});return;}
      const path=event.payload.paths[0];
      if(request.operation==='split'&&(path.endsWith('.afparts.json')||/\.\d{3,}$/.test(path)))selectOperation('merge',path);else applyInput(path);
    }
  });
}
