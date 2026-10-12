import assert from "node:assert/strict";
import test from "node:test";
import { readFileSync } from "node:fs";
import { preparedDocumentUrl, createPresentationDocumentHandoff } from "../browser/runtime/presentation-document.js";

const id="11111111-1111-4111-8111-111111111111";
function dom() {
  const elements=[];
  const document={createElement(tag){const listeners=new Map();const node={tag,children:[],listeners,append(...children){this.children.push(...children);},setAttribute(key,value){this[key]=value;},addEventListener(key,handler){listeners.set(key,handler);},removeEventListener(key){listeners.delete(key);},remove(){this.removed=true;}};elements.push(node);return node;}};
  return {document,parent:document.createElement("body"),elements};
}
test("prepared document URL contains only cookie-authenticated non-bearer selection on current origin",()=>{
  const url=preparedDocumentUrl(new URL("http://localhost:7411/ui/launch?token=secret&session_id=credential&presentation_id=old#draft"),id);
  assert.equal(url.href,`http://localhost:7411/ui/?presentation_id=${id}`);
  assert.throws(()=>preparedDocumentUrl(new URL("http://localhost:7411"),"credential"));
});
test("prepare completes before explicit noopener link; clicking does not consume it",async()=>{
  const {document,parent,elements}=dom();let finish;
  const frame=createPresentationDocumentHandoff(document,parent,new URL("http://localhost:7411"),()=>new Promise(resolve=>{finish=resolve;}));
  await Promise.resolve();
  const link=elements.find(node=>node.tag==="a");
  assert.equal(link.hidden,true);
  finish(id);await frame.ready;
  assert.equal(link.href,`http://localhost:7411/ui/?presentation_id=${id}`);
  assert.equal(link.target,"_blank");assert.equal(link.rel,"noopener noreferrer");
  assert.equal(link.hidden,false);assert.equal(link.listeners.has("click"),false);
  assert.equal(parent.children[0].removed,undefined);
});
test("failed preparation retries same callback and disposal joins late outcome without cancellation",async()=>{
  const {document,parent,elements}=dom();let calls=0;let finish;
  const retained={creation_request_id:id};const seen=[];
  const frame=createPresentationDocumentHandoff(document,parent,new URL("http://localhost:7411"),async()=>{seen.push(retained);if(++calls===1)throw Error("unknown delivery");return new Promise(resolve=>{finish=resolve;});});
  await frame.ready;
  const retry=elements.find(node=>node.textContent==="Retry exact preparation");
  assert.equal(retry.hidden,false);retry.listeners.get("click")();await Promise.resolve();
  frame.dispose();let settled=false;const joined=frame.settle().then(()=>{settled=true;});
  await Promise.resolve();assert.equal(settled,false);
  finish(id);await joined;
  assert.equal(settled,true);assert.equal(seen[0],seen[1]);
  assert.equal(parent.children[0].removed,true);
  assert.equal(elements.find(node=>node.tag==="a").hidden,true);
});
test("browser document path prepares but never claims parent-side or leaks session into URL",()=>{
  const boot=readFileSync(new URL("../browser/runtime/boot.ts",import.meta.url),"utf8");
  const branch=boot.slice(boot.indexOf('if (effect.kind.type === "open_new_presentation_document")'),boot.indexOf('return runEffect(effect'));
  assert.match(branch,/action:\s*"prepare"/);assert.match(branch,/postEncodedJson\("\/ui\/api\/session\/seat\/open",encoded\)/);
  assert.doesNotMatch(branch,/action:\s*"claim"|window\.open|freshDocumentUrl|fresh_session_id/);
  assert.match(boot,/\.\.\.documentSettlements/);
});
test("acknowledged selected document exposes its prepared identity without another prepare or claim",()=>{
  const boot=readFileSync(new URL("../browser/runtime/boot.ts",import.meta.url),"utf8");
  const branch=boot.slice(boot.indexOf('if (effect.kind.type === "show_prepared_presentation_document")'),boot.indexOf('if (effect.kind.type === "open_new_presentation_document")'));
  assert.match(branch,/createPresentationDocumentHandoff/);
  assert.match(branch,/async \(\) => presentationId/);
  assert.doesNotMatch(branch,/postEncodedJson|window\.open|session_id|action:\s*"(?:prepare|claim)"/);
});
