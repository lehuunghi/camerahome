const {test}=require('node:test');
const assert=require('node:assert/strict');
const vm=require('node:vm');
const fs=require('node:fs');
const html=fs.readFileSync(require('node:path').join(__dirname,'../web/index.html'),'utf8');
const block=html.slice(html.indexOf("const timeline = el('timeline')"),html.indexOf('async function openCamera'));
class Element {
  constructor(){this.style={};this.dataset={};this.hidden=true;this.value='';this.listeners={};this.children=[];this.currentTime=0;this.duration=10;this.readyState=0;this.paused=true;this.ended=false;this.error=null;this.seeking=false;this.classList={toggle(){}};}
  addEventListener(name,fn){(this.listeners[name]??=[]).push(fn);}
  emit(name){for(const fn of this.listeners[name]||[])fn();}
  pause(){this.paused=true;}
  play(){this.paused=false;return this.playResult||Promise.resolve();}
  load(){this.readyState=0;this.currentTime=0;this.ended=false;this.error=null;}
  removeAttribute(name){delete this[name];}
  replaceChildren(...items){this.children=items;}
  append(...items){this.children.push(...items);}
  querySelectorAll(){return this.children.flatMap(item=>item.children);}
}
const paths=['2026-11-28/01/Cuu/00.00.mp4','2026-11-28/09/Cuu/00.00.mp4','2026-11-28/15/Cuu/00.00.mp4'];
function harness(read=async()=>[]){
  const els={},timers=new Map(),intervals=[],clock={now:1000};let nextTimer=0;
  const el=id=>els[id]??=(new Element());el('day').value='2026-11-28';el('filter').value='Cuu';el('speed').value='1';el('recording-source').value='r2';
  const context={el,player:el('player'),filter:el('filter'),day:el('day'),document:{hidden:false,createElement:()=>new Element()},currentView:'recordings',performance:{now:()=>clock.now},setIcon(){},cameraName:id=>id,resetReplayZoom(){},setInterval(fn,ms){intervals.push({fn,ms});},setTimeout(fn){const id=++nextTimer;timers.set(id,fn);return id;},clearTimeout(id){timers.delete(id);},read,allowed:()=>true,requestId:0,dayMs:86400000,URLSearchParams,console};
  vm.createContext(context);vm.runInContext(block+`;globalThis.api={playClip,showClips,advance,resetReplay,stopReplay,clips,playlist,prefetchNext,applyRate,retryReplay,snap:()=>({activePath,playing,transition,player,slots,rows,replayGeneration}),setClock};`,context);
  const api=context.api;api.showClips(paths);
  function ready(video=api.snap().transition?.video,duration=10){video.duration=duration;video.readyState=1;video.emit('loadedmetadata');video.readyState=2;video.emit('loadeddata');return video;}
  function finish(){const video=api.snap().player;video.currentTime=video.duration;video.ended=true;video.emit('ended');return video;}
  function flushTimer(){const [id,fn]=timers.entries().next().value||[];if(fn){timers.delete(id);fn();}}
  return {api,el,ready,finish,timers,intervals,clock,context,flushTimer};
}
test('preloads the next clip and jumps across an eight-hour gap',()=>{
 const h=harness();h.api.playClip(paths[0]);const first=h.ready();const idle=[...h.api.snap().slots].find(([v])=>v!==first);assert.equal(idle[1].path,paths[1]);h.ready(idle[0]);h.finish();assert.equal(h.api.snap().activePath,paths[1]);assert.equal(h.api.snap().player,idle[0]);assert.equal(h.api.snap().transition,null);assert.equal(h.api.snap().playing,true);
});
test('keeps the old frame visible until the next clip has decoded data',()=>{
 const h=harness();h.api.playClip(paths[0]);const first=h.ready();h.finish();let s=h.api.snap();assert.equal(first.hidden,false);assert.equal(first.style.opacity,'1');assert.equal(s.transition.video.style.opacity,'0');const next=h.ready();assert.equal(next.style.opacity,'1');assert.equal(first.style.opacity,'0');
});
test('duplicate ended and stale events cannot skip or return to another clip',()=>{
 const h=harness();h.api.playClip(paths[0]);const first=h.ready();h.finish();first.emit('ended');assert.equal(h.api.snap().activePath,paths[1]);const second=h.ready();first.emit('ended');first.emit('timeupdate');assert.equal(h.api.snap().activePath,paths[1]);assert.equal(h.api.snap().player,second);
});
test('a rejected play promise for an old clip cannot pause the new clip',async()=>{
 const h=harness();let reject;h.api.playClip(paths[0]);const next=h.api.snap().transition.video;next.playResult=new Promise((_,r)=>reject=r);h.ready();h.api.playClip(paths[1]);h.ready();reject(Object.assign(new Error('old request'),{name:'NotAllowedError'}));await Promise.resolve();assert.equal(h.api.snap().playing,true);assert.equal(h.api.snap().activePath,paths[1]);
});
test('a late AbortError does not stop playback',async()=>{
 const h=harness();h.api.playClip(paths[0]);const next=h.api.snap().transition.video;next.playResult=Promise.reject(Object.assign(new Error(),{name:'AbortError'}));h.ready();await Promise.resolve();assert.equal(h.api.snap().playing,true);
});
test('changing selection cancels a pending transition and its late events',()=>{
 const h=harness();h.api.playClip(paths[0]);const loading=h.api.snap().transition.video;h.api.resetReplay();loading.readyState=2;loading.emit('loadeddata');loading.emit('ended');assert.equal(h.api.snap().activePath,'');assert.equal(h.api.snap().playing,false);assert.equal(h.el('replay-frame').hidden,true);
});
test('seeking into an unbuffered position holds the old frame until seeked',()=>{
 const h=harness();h.api.playClip(paths[0]);const first=h.ready();h.api.playClip(paths[1],5);const next=h.api.snap().transition.video;next.duration=10;next.readyState=1;next.emit('loadedmetadata');next.seeking=true;next.readyState=2;next.emit('loadeddata');assert.equal(h.api.snap().player,first);next.seeking=false;next.emit('seeked');assert.equal(h.api.snap().player,next);assert.equal(next.currentTime,5);
});
test('refreshing a list retains the current clip and does not change its source',()=>{
 const h=harness();h.api.playClip(paths[1]);const video=h.ready();const src=video.src;h.api.showClips([paths[2]]);assert.equal(h.api.snap().activePath,paths[1]);assert.equal(h.api.snap().player,video);assert.equal(video.src,src);assert.equal(h.api.snap().rows.length,2);
});
test('UTC day results are applied atomically, never resetting the current video',async()=>{
 const requests=[];const h=harness(()=>new Promise(resolve=>requests.push(resolve)));h.api.playClip(paths[1]);h.ready();const promise=h.api.clips();assert.equal(requests.length,2);requests[0]([paths[0]]);await Promise.resolve();await Promise.resolve();assert.equal(h.api.snap().rows.length,3);requests[1]([paths[1],paths[2]]);await promise;assert.equal(h.api.snap().activePath,paths[1]);assert.equal(h.api.snap().rows.length,3);
});
test('partial cloud-list failure preserves previously loaded clips',async()=>{
 let call=0;const h=harness(async()=>++call===1?[paths[0]]:Promise.reject(new Error('temporary limit')));await h.api.clips();assert.equal(h.api.snap().rows.length,1);call=0;h.api.showClips(paths);await h.api.clips();assert.equal(h.api.snap().rows.length,3);
});
test('a paused clip never auto-advances on a delayed ended event',()=>{
 const h=harness();h.api.playClip(paths[0]);h.ready();h.api.stopReplay();h.finish();assert.equal(h.api.snap().activePath,paths[0]);assert.equal(h.api.snap().playing,false);
});
test('end of playlist holds the last frame and never wraps to an old clip',()=>{
 const h=harness();h.api.playClip(paths[2]);const last=h.ready();h.finish();h.el('toggle-replay').onclick();assert.equal(h.api.snap().activePath,paths[2]);assert.equal(h.api.snap().playing,false);assert.equal(last.style.opacity,'1');
});
test('prefetch errors do not stop the clip currently playing',()=>{
 const h=harness();h.api.playClip(paths[0]);const current=h.ready();const idle=[...h.api.snap().slots.keys()].find(v=>v!==current);idle.error={code:2};idle.emit('error');assert.equal(h.api.snap().playing,true);assert.equal(h.timers.size,0);
});
test('failed transitions retry only the selected clip and then skip forward',()=>{
 const h=harness();h.api.playClip(paths[0]);h.ready();h.finish();for(let attempt=0;attempt<3;attempt++){const next=h.api.snap().transition.video;next.error={code:2};next.emit('error');h.flushTimer();}assert.equal(h.api.snap().activePath,paths[2]);assert.equal(h.api.snap().playing,true);
});
test('new user selection invalidates an older scheduled retry',()=>{
 const h=harness();h.api.playClip(paths[0]);let v=h.api.snap().transition.video;v.error={code:2};v.emit('error');assert.equal(h.timers.size,1);h.api.playClip(paths[2]);h.flushTimer();assert.equal(h.api.snap().activePath,paths[2]);
});
test('mute and playback rate persist over a double-buffer switch',()=>{
 const h=harness();h.el('speed').value='4';h.api.playClip(paths[0]);h.ready();h.el('mute-replay').onclick();h.finish();const second=h.ready();assert.equal(second.muted,true);assert.equal(second.playbackRate,4);
});
test('no background audio can play from a prefetched clip',()=>{
 const h=harness();h.api.playClip(paths[0]);const first=h.ready();const idle=[...h.api.snap().slots.keys()].find(v=>v!==first);h.ready(idle);assert.equal(idle.paused,true);assert.equal(idle.muted,true);
});
test('20x stepped playback advances once while the next file is loading',()=>{
 const h=harness();h.api.playClip(paths[0]);const first=h.api.snap().transition.video;Object.defineProperty(first,'playbackRate',{set(){throw new Error('unsupported');}});h.el('speed').value='20';h.ready(first,1);const tick=h.intervals.find(x=>x.ms===100).fn;h.clock.now+=100;tick();h.clock.now+=100;tick();assert.equal(h.api.snap().activePath,paths[1]);assert.ok(h.api.snap().transition);
});

test('pausing cancels a retry without preventing retries after resume',()=>{
 const h=harness();h.api.playClip(paths[0]);let v=h.api.snap().transition.video;v.error={code:2};v.emit('error');h.api.stopReplay();assert.equal(h.timers.size,0);h.el('toggle-replay').onclick();v=h.api.snap().transition.video;v.error={code:2};v.emit('error');assert.equal(h.timers.size,1);
});
