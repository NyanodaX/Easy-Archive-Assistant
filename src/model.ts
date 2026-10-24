export type Operation = 'split'|'merge'|'reshard'|'rebuild'|'rescue'|'coverage'|'verify'|'transactions'|'recover'|'restart';
export type Request = { operation: Operation; input: string; partPaths: string[]; outputDir: string; basename: string; size: string; policy: string; tolerance: string; alignment: string; manifest: string; sources: string[]; part: string; expectedSha256: string; metadataOnly: boolean };
export function freshRequest(operation: Operation = 'split'): Request {
  return { operation, input:'', partPaths:[], outputDir:'', basename:'', size:'1.5GiB', policy:'hard', tolerance:'5', alignment:'', manifest:'', sources:[], part:'', expectedSha256:'', metadataOnly:false };
}
export const isManifestPath=(path:string)=>path.endsWith('.afparts.json');
export const isVolumePath=(path:string)=>/\.\d{3,}$/.test(path);
const pathKey=(path:string)=>/^[A-Za-z]:[\\/]/.test(path)?path.replaceAll('/','\\').toLowerCase():path;
export function addSelectedFiles(current:Request, incoming:string[]): Request {
  const manifests=incoming.filter(isManifestPath);
  if(manifests.length>1||incoming.some(path=>!isManifestPath(path)&&!isVolumePath(path)))throw Error('INVALID_GROUP_SELECTION');
  const paths=[...current.partPaths];const known=new Set(paths.map(pathKey));
  for(const path of incoming.filter(isVolumePath)) {const key=pathKey(path);if(!known.has(key)){paths.push(path);known.add(key);}}
  if(paths.length>10_000||new TextEncoder().encode(JSON.stringify(paths)).length>4*1024*1024)throw Error('LIMIT_EXCEEDED');
  return {...current,partPaths:paths,input:manifests[0]??(current.input||paths[0]||'')};
}
export function removeSelectedFiles(current:Request, removed:Set<string>): Request {
  const paths=current.partPaths.filter(path=>!removed.has(path));
  return {...current,partPaths:paths,input:removed.has(current.input)?paths[0]??'':current.input};
}
export type Data = Record<string, any>;
export class Workflow {
  phase: 'idle'|'planning'|'planned'|'running'|'completed'|'failed' = 'idle';
  jobId = ''; token = ''; preview: Data|null = null; result: any = null; error: Data|null = null; progress: Data|null = null;
  resultOperation: Operation|null = null;
  rate = '0'; lastProgressAt = 0;
  get busy() { return this.phase === 'planning' || this.phase === 'running'; }
  invalidate() { if (this.busy) return; this.phase='idle'; this.token=''; this.preview=null; this.result=null; this.resultOperation=null; this.error=null; this.progress=null; }
  begin(phase: 'planning'|'running', jobId: string) { this.phase=phase; this.jobId=jobId; this.progress=null; this.rate='0';this.lastProgressAt=0;this.error=null; this.result=null; this.resultOperation=null; if(phase==='planning') { this.preview=null; this.token=''; } }
  acceptProgress(jobId: string, progress: Data, now=Date.now()) {
    if (!this.busy || jobId!==this.jobId) return;
    const delta=this.progress ? BigInt(progress.completed_bytes)-BigInt(this.progress.completed_bytes) : 0n;
    this.rate=this.progress?.phase===progress.phase&&delta>=0n&&now>this.lastProgressAt ? String(delta*1000n/BigInt(now-this.lastProgressAt)) : '0';
    this.progress=progress;this.lastProgressAt=now;
  }
  planned(jobId: string, value: Data) { if(jobId!==this.jobId) return; this.preview=value.preview; this.token=value.token; this.phase='planned'; }
  completed(jobId: string, value: any, operation?:Operation) { if(jobId!==this.jobId) return; this.result=value; this.resultOperation=operation??null; this.token=''; this.phase='completed'; }
  failed(jobId: string, error: Data) { if(jobId!==this.jobId) return; this.error=error; this.token=''; this.phase='failed'; }
}
export function formatBytes(value: string|undefined, locale='en'): string {
  if (value===undefined || value===null) return '—';
  const bytes = BigInt(value); const units=['B','KiB','MiB','GiB','TiB','PiB','EiB'];
  let scale=1n, i=0; while(bytes>=scale*1024n && i<units.length-1) { scale*=1024n; i++; }
  const exact=bytes.toLocaleString(locale);
  return i===0 ? `${exact} B` : `${bytes/scale}.${(bytes%scale)*100n/scale < 10n ? '0':''}${(bytes%scale)*100n/scale} ${units[i]} (${exact} B)`;
}
export function progressPercent(completed: string, total: string): number {
  const all=BigInt(total), done=BigInt(completed); if(all===0n) return 0;
  const percent=done*100n/all;
  return Number(percent>100n ? 100n : percent<0n ? 0n : percent);
}
export function resultTone(operation: Operation, result: Data): 'good'|'caution'|'bad' {
  if(operation==='coverage') return result.has_integrity_errors ? 'bad' : result.complete_coverage ? 'good' : 'caution';
  if(operation==='transactions') return 'caution';
  return result.verification_level==='verified_against_original' || result.verification_level==='verified_against_manifest' ? 'good' : 'caution';
}
