import {test} from 'node:test';
import assert from 'node:assert/strict';
import {Workflow, formatBytes, progressPercent, resultTone, freshRequest, addSelectedFiles, removeSelectedFiles} from './model.ts';
test('u64 display and progress remain exact above JavaScript safe integers',()=>{
  assert.match(formatBytes('18446744073709551615'), /18,446,744,073,709,551,615 B/);
  assert.equal(progressPercent('9007199254740993','18014398509481986'),50);
  assert.equal(progressPercent('0','0'),0); assert.equal(progressPercent('200','100'),100);
});
test('late progress and results cannot overwrite the current job',()=>{
  const flow=new Workflow(); flow.begin('planning','old'); flow.begin('planning','current');
  flow.acceptProgress('old',{completed_bytes:'900'}); flow.planned('old',{token:'old',preview:{}});
  assert.equal(flow.progress,null); assert.equal(flow.phase,'planning');
  flow.planned('current',{token:'valid',preview:{source_size:'20'}}); assert.equal(flow.token,'valid');
  flow.invalidate(); assert.equal(flow.token,''); assert.equal(flow.preview,null);
});
test('cancellation and incomplete diagnostics never produce a success state',()=>{
  const flow=new Workflow(); flow.begin('running','one'); flow.failed('one',{code:'CANCELLED'});
  assert.equal(flow.phase,'failed'); assert.equal(flow.result,null); assert.equal(flow.token,'');
  assert.equal(resultTone('coverage',{complete_coverage:false,has_integrity_errors:false}),'caution');
  assert.equal(resultTone('coverage',{complete_coverage:true,has_integrity_errors:true}),'bad');
  assert.equal(resultTone('rescue',{verification_level:'available'}),'caution');
});
test('separate-drive selections accumulate without modifying input paths',()=>{
  const original=freshRequest('merge');
  const first=addSelectedFiles(original,['E:\\backup.zip.003','F:\\backup.zip.001']);
  const second=addSelectedFiles(first,['G:\\backup.zip.002','e:/backup.zip.003']);
  assert.equal(original.partPaths.length,0);assert.equal(second.partPaths.length,3);
  const manifest=addSelectedFiles(second,['C:\\manifests\\backup.zip.afparts.json']);
  assert.equal(manifest.input,'C:\\manifests\\backup.zip.afparts.json');
  assert.deepEqual(manifest.partPaths,second.partPaths);
  const removed=removeSelectedFiles(first,new Set(['E:\\backup.zip.003']));
  assert.equal(removed.input,'F:\\backup.zip.001');assert.equal(removed.partPaths.length,1);
});
test('selection failures are bounded and do not partially replace a collection',()=>{
  const original=addSelectedFiles(freshRequest('merge'),['E:\\backup.zip.001']);
  assert.throws(()=>addSelectedFiles(original,['F:\\backup.zip.002','C:\\other.exe']),/INVALID_GROUP_SELECTION/);
  assert.equal(original.partPaths.length,1);
  assert.throws(()=>addSelectedFiles(original,['a.afparts.json','b.afparts.json']),/INVALID_GROUP_SELECTION/);
  assert.throws(()=>addSelectedFiles(original,Array.from({length:10001},(_,i)=>`E:\\backup.zip.${String(i+1).padStart(3,'0')}`)),/LIMIT_EXCEEDED/);
});
