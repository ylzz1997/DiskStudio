import net from 'node:net';
import path from 'node:path';
import fs from 'node:fs';
import { root, safePath } from './lab.mjs';
import { fileURLToPath } from 'node:url';
export async function verifyUbuntu(resume = false) {
  const socket=net.createConnection({host:'127.0.0.1',port:49366});
  const pending=new Map(); let buffer='',sequence=0;
  let greet,fail;
  const ready=new Promise((resolve,reject)=>{greet=resolve;fail=reject;});
  socket.setTimeout(8000,()=>socket.destroy(Error('Ubuntu QMP timeout')));
  socket.on('error',e=>{fail(e);for(const p of pending.values())p.reject(e);});
  socket.on('data',data=>{
    buffer+=data;
    let at;
    while((at=buffer.indexOf('\n'))>=0){
      const v=JSON.parse(buffer.slice(0,at));buffer=buffer.slice(at+1);
      if(v.QMP)greet();
      if(pending.has(v.id)){const p=pending.get(v.id);pending.delete(v.id);v.error?p.reject(Error(JSON.stringify(v.error))):p.resolve(v.return);}
    }
  });
  const execute=command=>new Promise((resolve,reject)=>{const id=++sequence;pending.set(id,{resolve,reject});socket.write(JSON.stringify({execute:command,id})+'\n');});
  try {
    await ready;await execute('qmp_capabilities');
    if((await execute('query-name')).name!=='DiskStudio-Ubuntu' || (await execute('query-uuid')).UUID!=='25c39913-ac45-4445-aa54-bdd0dbca7001')throw Error('Unexpected Ubuntu VM identity');
    const blocks=await execute('query-block');
    const expected=new Map([
      [safePath(path.join(root,'ubuntu/system.qcow2')).toLowerCase(),[16*1024**3,false]],
      [safePath(path.join(root,'windows/test-a.qcow2')).toLowerCase(),[16*1024**3,false]],
      [safePath(path.join(root,'windows/test-b.qcow2')).toLowerCase(),[16*1024**3,false]],
    ]);
    const state=JSON.parse(fs.readFileSync(safePath(path.join(root,'ubuntu/run.json')),'utf8'));
    const seed=['seed.iso','seed-populate.iso','seed-verify.iso','seed-ext-populate.iso','seed-ext-verify.iso'].find(name=>state.args.some(arg=>arg.includes(`file=${path.join(root,'ubuntu',name)},`)));
    if(!seed)throw Error('Unknown Ubuntu read-only seed');
    const media=safePath(path.join(root,'ubuntu',seed)); expected.set(media.toLowerCase(),[fs.statSync(media).size,true]);
    if(seed==='seed.iso'){expected.delete(path.join(root,'windows/test-a.qcow2').toLowerCase());expected.delete(path.join(root,'windows/test-b.qcow2').toLowerCase());}
    if(blocks.length!==expected.size)throw Error('Unexpected Ubuntu disk count');
    for(const b of blocks){
      const i=b.inserted;const name=i?.image?.filename;
      if(typeof name!=='string')throw Error('Missing Ubuntu file backend');
      const actual=safePath(name).toLowerCase(),wanted=expected.get(actual);
      if(!wanted || i.image['virtual-size']!==wanted[0] || i.ro!==wanted[1] || i.drv!==(wanted[1]?'raw':'qcow2') || safePath(i.file).toLowerCase()!==actual || i.image['backing-filename'])throw Error('Unexpected Ubuntu disk path, capacity or access mode');
      expected.delete(actual);
    }
    if(expected.size)throw Error('Incomplete Ubuntu disk inventory');
    if(resume){
      if((await execute('query-status')).status!=='prelaunch')throw Error('Ubuntu must remain paused until preflight completes');
      await execute('cont');
    }
    console.log('PASS: Ubuntu UUID, exact file-backed disk inventory, capacities and read-only seed; no host disk backend.');
  } finally {socket.destroy();}
}
if(process.argv[1] && path.resolve(process.argv[1])===fileURLToPath(import.meta.url))verifyUbuntu().catch(e=>{console.error(e.message);process.exitCode=1;});
