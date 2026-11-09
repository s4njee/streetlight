// Verify the exact bridge embedded in the client UI without a browser or native calls.
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
const html=readFileSync(new URL('../web/client/index.html',import.meta.url),'utf8');
const code=html.slice(html.indexOf('window.streetlight ='),html.indexOf('const sl = window.streetlight;'));
const context={window:{}};
vm.runInNewContext(code,context);
const bridge=context.window.streetlight;
let called=0;
bridge.once('pairPin',()=>called++);
bridge.__event('pairPin',{}); bridge.__event('pairPin',{});
assert.equal(called,1);
for(let attempt=0; attempt<500; attempt++) {
  const cancel=bridge.once('pairPin',()=>called++);
  cancel();
}
bridge.__event('pairPin',{});
assert.equal(called,1,'cancelled pairing listeners must not accumulate');
const off=bridge.on('pairPin',()=>called++);
bridge.__event('pairPin',{}); off(); bridge.__event('pairPin',{});
assert.equal(called,2);
console.log('PASS: one-shot and cancelled pairing listeners');
