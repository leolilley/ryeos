import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

const mock = `
export class UnknownDeliveryError extends Error {}
export class HttpResponseError extends Error { constructor(status) { super('HTTP refusal'); this.status=status; } }
export const encodeJsonBody = value => JSON.stringify(value, (_key,value) => typeof value==='bigint' ? value.toString() : value);
export const encodeSeatPayloadDigest = encodeJsonBody;
export const decodeJsonBody = text => JSON.parse(text, (_key,value) => typeof value === 'number' && Number.isInteger(value) ? BigInt(value) : value);
export const errorMessage = error => String(error);
export const postJson = (url,body) => globalThis.seatHarness.post(url,body);
export const getJson = () => globalThis.seatHarness.current();
export const postEncodedJson = async (url,body) => {
  const h=globalThis.seatHarness;
  if(url.endsWith('/open')||url.endsWith('/close')) {
    h.controls.push(body);
    if(h.controlLost>0){h.controlLost--;throw new UnknownDeliveryError('control delivery unknown');}
    const request=JSON.parse(body);
    const result=await h.post(url,request);
    if(request.action==='claim') { h.lastClaimRequest=request; h.lastClaimResult=result; }
    return result;
  }
  h.encoded.push(body);
  if(h.auth){const status=h.auth;h.auth=0;throw new HttpResponseError(status);}
  if(h.timeout>0){h.timeout--;throw new HttpResponseError(408);}
  if(h.rateLimitAfterCommit>0){h.rateLimitAfterCommit--;h.append(JSON.parse(body));throw new HttpResponseError(429);}
  if(h.rateLimit>0){h.rateLimit--;throw new HttpResponseError(429);}
  if(h.serverError>0){h.serverError--;throw new HttpResponseError(503);}
  if(h.lost>0){h.lost--;throw new UnknownDeliveryError('lost');}
  return h.append(JSON.parse(body));
};
`;
const subjectModule = ts.transpileModule(await readFile(new URL("../browser/runtime/attachment-subject.ts", import.meta.url), "utf8"), {compilerOptions:{module:ts.ModuleKind.ESNext,target:ts.ScriptTarget.ES2022}}).outputText;
const threadTailModule = ts.transpileModule(await readFile(new URL("../browser/runtime/thread-tail.ts", import.meta.url), "utf8"), {compilerOptions:{module:ts.ModuleKind.ESNext,target:ts.ScriptTarget.ES2022}}).outputText;
const streamModule = ts.transpileModule((await readFile(new URL("../browser/runtime/seat-history-stream.ts", import.meta.url), "utf8"))
  .replace('from "./transport"', `from ${JSON.stringify(`data:text/javascript;base64,${Buffer.from(mock).toString("base64")}`)}`),
  {compilerOptions:{module:ts.ModuleKind.ESNext,target:ts.ScriptTarget.ES2022}}).outputText;
const source = (await readFile(new URL("../browser/runtime/session.ts", import.meta.url), "utf8"))
  .replace('from "./transport"', `from ${JSON.stringify(`data:text/javascript;base64,${Buffer.from(mock).toString("base64")}`)}`)
  .replace('from "./attachment-subject"', `from ${JSON.stringify(`data:text/javascript;base64,${Buffer.from(subjectModule).toString("base64")}`)}`)
  .replace('from "./thread-tail"', `from ${JSON.stringify(`data:text/javascript;base64,${Buffer.from(threadTailModule).toString("base64")}`)}`)
  .replace('from "./seat-history-stream"', `from ${JSON.stringify(`data:text/javascript;base64,${Buffer.from(streamModule).toString("base64")}`)}`);
const compiled = ts.transpileModule(source, {compilerOptions:{module:ts.ModuleKind.ESNext,target:ts.ScriptTarget.ES2022}}).outputText;
const {createSessionRuntime,classifySeatTouchResponse,classifySeatTouchError} = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);
const {HttpResponseError} = await import(`data:text/javascript;base64,${Buffer.from(mock).toString("base64")}`);
const root={binding_attachment_id:"root",binding_generation:1n,binding_digest:"d".repeat(64)};
const coord=id=>({presentation_id:id,thread_id:`thread-${id}`,chain_root_id:`chain-${id}`});
const pause=()=>new Promise(resolve=>setImmediate(resolve));
// The runtime's only genuinely asynchronous step outside the harness is
// crypto.subtle.digest, which completes on the libuv thread pool and so has no
// fixed relation to the event-loop turn count. Every digest is counted so a
// test can wait for real quiescence instead of guessing a number of turns.
let digestsInFlight=0;
const subtle=new Proxy(webcrypto.subtle,{get(target,key){
 if(key!=="digest")return Reflect.get(target,key).bind(target);
 return async (...args)=>{digestsInFlight++;try{return await target.digest(...args);}finally{digestsInFlight--;}};
}});
const trackedCrypto=new Proxy(webcrypto,{get(target,key){if(key==="subtle")return subtle;const value=Reflect.get(target,key);return typeof value==="function"?value.bind(target):value;}});
// Wait for an observable condition; failure is a deadline, never a turn budget.
async function until(done,what="asynchronous owner transition"){
 const deadline=Date.now()+20_000;
 while(!done()){if(Date.now()>deadline)assert.fail(`timed out waiting for ${what}`);await pause();}
}
// Wait until every started digest has finished and the continuations it
// released have run.
async function idle(){do{await pause();await pause();}while(digestsInFlight>0);}
// Transport tests stub the Rust bridge; commitment semantics are covered in Rust.
const derivedAppendId="11111111-1111-8111-8111-111111111111";
const derivedControlId="22222222-2222-8222-8222-222222222222";
const initializationPredecessor={kind:"initialization",event_hash:"d".repeat(64)};
const controlPredecessor={kind:"control",event_hash:"c".repeat(64)};
function ack(body){
 const events=body.events.map((event,index)=>{const event_hash=(index+1).toString(16).padStart(64,"0");
  const previous=index===0?"f".repeat(64):index.toString(16).padStart(64,"0");
  return {event_hash,chain_root_id:`chain-${body.presentation_id}`,chain_seq:String(40+index),
   thread_id:body.thread_id,thread_seq:String(3+index),event_type:event.event_type,durability:"durable",
   ts:"2026-10-06T00:00:00Z",prev_chain_event_hash:previous,prev_thread_event_hash:previous,
   payload:{seq:String(event.engine_seq),payload:event.payload}};});
 return {producer_incarnation:body.producer_incarnation,operation_id:body.operation_id,
  first_engine_seq:String(body.first_engine_seq),last_engine_seq:String(body.last_engine_seq),
  event_count:body.event_count,appended:body.event_count,payload_digest:body.payload_digest,
  chain_seq:String(40+events.length),receipt_thread_seq:String(3+events.length),
  receipt_event_hash:"e".repeat(64),events_authority:"authoritative",events};
}
function harness(){
 globalThis.window=globalThis;
 Object.defineProperty(globalThis,"crypto",{configurable:true,value:trackedCrypto});
 const storage=new Map();
 globalThis.localStorage={getItem:key=>storage.get(key)??null,setItem:(key,value)=>storage.set(key,value),removeItem:key=>storage.delete(key)};
 const h={storage,calls:[],encoded:[],controls:[],identityInputs:[],controlIdentityInputs:[],producers:new Map(),controlLost:0,lost:0,timeout:0,rateLimit:0,rateLimitAfterCommit:0,serverError:0,auth:0,next:0,receipt:null,committedOperation:null,committedBody:null,committedCount:0,notFound:false,historyFrames:null,historyCalls:[],lastClaimRequest:null,lastClaimResult:null,
 current:()=>({session_id:"session",binding_attachments:[{...root,project_path:null,subject:{kind:"projectless"}}],presentations:[{...coord("A"),root_binding:root},{...coord("B"),root_binding:root}]}),
 append(body){
  if(this.committedOperation===body.operation_id){
   assert.equal(JSON.stringify(body),this.committedBody,"idempotent retry changed its committed body");
   return this.receipt;
  }
  this.committedOperation=body.operation_id;
  this.committedBody=JSON.stringify(body);
  this.committedCount++;
  this.receipt=ack(body);
  return this.receipt;
 },
 async post(url,body){
  this.calls.push({url,body});
  if(url.endsWith("/open")&&body.action==="prepare")return {...coord(++this.next===1?"A":"B"),root_binding:root};
  if(url.endsWith("/open")&&body.action==="observe")return {...coord(body.presentation_id),revision:0,root_binding:root,
   control_predecessor:this.producers.has(body.presentation_id)?controlPredecessor:initializationPredecessor,
   producer:this.producers.get(body.presentation_id)??{kind:"unclaimed"}};
  if(url.endsWith("/open")&&body.action==="claim"){
   this.producers.set(body.presentation_id,{kind:"active",producer_incarnation:"a".repeat(64)});
   this.lastClaimRequest=body;
   this.lastClaimResult={...coord(body.presentation_id),operation_id:body.operation_id,
    producer_incarnation:"a".repeat(64),next_engine_seq:"0",state:"active"};
   return this.lastClaimResult;
  }
  if(url.endsWith("/replay")&&body.receipt){
   if(body.receipt.thread_id!==coord(body.presentation_id).thread_id)throw new Error("receipt thread coordinate mismatch");
   return {receipt_status:this.notFound?"not_found":"found",receipt:this.notFound?null:this.receipt};
  }
  if(url.endsWith("/replay"))return {events:[],next_cursor:null};
  if(url.endsWith("/close")){
   this.producers.set(body.presentation_id,{kind:"relinquished",producer_incarnation:body.producer_incarnation});
   return {...coord(body.presentation_id),...body,state:"relinquished",next_engine_seq:"0"};
  }
  return {};
 }};
 globalThis.fetch=async (url,init)=>{
  if(!url.endsWith("/ui/api/session/seat/history"))throw new Error(`unexpected fetch ${url}`);
  const request=JSON.parse(init.body);h.historyCalls.push(request);
  const c=coord(request.presentation_id),claim=h.lastClaimRequest,result=h.lastClaimResult;
  const initialized={event_hash:"d".repeat(64),chain_root_id:c.chain_root_id,thread_id:c.thread_id,thread_seq:1,prev_thread_event_hash:"0".repeat(64),chain_seq:1,event_type:"ui_seat.presentation_initialized.v1",payload:{}};
  const control={event_hash:"c".repeat(64),chain_root_id:c.chain_root_id,thread_id:c.thread_id,thread_seq:2,prev_thread_event_hash:initialized.event_hash,chain_seq:2,event_type:"ui_seat.producer.v3",payload:{schema_version:"ryeos.ui.seat.producer.v3",operation_id:result?.operation_id,producer_incarnation:result?.producer_incarnation,next_engine_seq:result?.next_engine_seq,state:result?.state,request:claim}};
  const cframes=[
   {event:"ui_seat.replay.begin.v1",data:{schema:1,...c}},
   {event:"ui_seat.replay.events.v1",data:{schema:1,events:[initialized,control]}},
   {event:"ui_seat.replay.complete.v1",data:{schema:1,tip_hash:control.event_hash,thread_seq:2,event_count:2}},
  ];
  const frames=h.historyFrames??cframes;
  const body=new ReadableStream({start(controller){
   (h.historyControllers??=[]).push(controller);
   for(const frame of frames)controller.enqueue(new TextEncoder().encode(`event: ${frame.event}\ndata: ${JSON.stringify(frame.data)}\n\n`));
  }});
  return new Response(body,{headers:{"Content-Type":"text/event-stream"}});
 };
 globalThis.seatHarness=h;return h;
}
function runtime(extra={},events=[]){
 let admitted=false;
 const r=createSessionRuntime({sessionId:"session",eventsUrl:null,rootAttachment:root,maxSeatAppendBytes:65536n,
 deriveSeatAppendIdentity(identity){globalThis.seatHarness.identityInputs.push(identity);return derivedAppendId;},
 deriveSeatControlIdentity(request){globalThis.seatHarness.controlIdentityInputs.push(request);return derivedControlId;},
 seatEvents:()=>admitted?events:[],commitEvent(){},
 // Test observation marker only; production invokes the combined Rust ACK
 // owner, not this legacy event shape or an inferred same-coordinate replay.
 replayAcknowledgedSeatEvents(producer,captured,canonical){extra.commitEvent?.({type:"seat_append_acknowledged",producer,events:captured,canonical});},
 async beginSeatReplay(){},async beginSeatReproof(_coordinate,requireOwner){requireOwner();},
 async completeSeatReproof(_coordinate,_completion,requireOwner){requireOwner();return {acknowledgedEventCount:events.length,publish(){requireOwner();}};},
 replaySeatEvents(){},replaySeatTransferProof(){},replaySeatMoveFreezeCancellationProof(){},abortSeatReplay(){},...extra});
 const attach=r.attachSeat.bind(r),attempts=new Map();
 r.attachSeat=()=>{const original=attach();if(!attempts.has(original))attempts.set(original,original.then(()=>{
  admitted=true;r.observe({});
 }));return attempts.get(original);};return r;
}
async function retained(h){
 const events=[{engine_seq:"0",event_type:"seat.facet",payload:{draft:"retained"}}];
 const bytes=new TextEncoder().encode(JSON.stringify(events));
 const digest=[...new Uint8Array(await crypto.subtle.digest("SHA-256",bytes))].map(byte=>byte.toString(16).padStart(2,"0")).join("");
 const body={presentation_id:"A",thread_id:"thread-A",producer_incarnation:"a".repeat(64),operation_id:derivedAppendId,
 first_engine_seq:"0",last_engine_seq:"0",event_count:1,payload_digest:digest,events};
 h.receipt=ack(body);
 h.storage.set("ryeos.ui.seat.pending.v2:session:A",JSON.stringify({schema_version:"ryeos.ui.seat.pending-append.v3",
 session_id:"session",...coord("A"),...body,encoded_request:JSON.stringify(body)}));
}
function deferred(){let resolve;const promise=new Promise(done=>{resolve=done;});return {promise,resolve};}
function historyWake(h,index,data={schema:1}){
 h.historyControllers[index].enqueue(new TextEncoder().encode(`event: ui_seat.replay.reproof_required.v1\ndata: ${JSON.stringify(data)}\n\n`));
}
async function eventually(predicate){await until(predicate);}

// Authenticated transport simulation only: these tests do not mint a Rust
// completed candidate or qualify server CAS proofs. AUTHORED, NOT RUN.
for(const phase of ["begin","complete"]){
 test(`disposal during reproof ${phase} authorization cannot stage or publish`,async()=>{
  const h=harness(),gate=deferred();let entered=false,staged=0,published=0;
  const r=runtime({
   async beginSeatReproof(_coordinate,requireOwner){
    if(phase==="begin"){entered=true;await gate.promise;}
    requireOwner();staged++;
   },
   async completeSeatReproof(_coordinate,_completion,requireOwner){
    if(phase==="complete"){entered=true;await gate.promise;}
    requireOwner();return {acknowledgedEventCount:0,publish(){requireOwner();published++;}};
   },
  });
  try{
   await r.attachSeat();r.completeEngineBootstrap();historyWake(h,0);
   await eventually(()=>entered);r.close();gate.resolve();await r.settle();
   assert.equal(staged,phase==="begin"?0:1);assert.equal(published,0);
   assert.equal(r.currentProducer(),null);assert.equal(r.currentReplayCompletion(),null);
   assert.equal(h.calls.filter(c=>c.body.action==="claim").length,1);
  }finally{gate.resolve();r.close();await r.settle();}
 });
}
test("promoted longer acknowledged journal never reappends historical rows",async()=>{
 const h=harness(),events=[];let published=0;
 const r=runtime({async completeSeatReproof(_coordinate,_completion,requireOwner){
  requireOwner();events.push({seq:40n,event_type:"seat.facet",payload:{draft:"historical"}},
   {seq:41n,event_type:"seat.facet",payload:{draft:"also historical"}});
  return {acknowledgedEventCount:events.length,publish(){requireOwner();published++;}};
 }},events);
 try{
  await r.attachSeat();r.completeEngineBootstrap();historyWake(h,0);
  await eventually(()=>published===1);await idle();
  assert.equal(h.encoded.length,0);assert.equal(r.canAcceptMutations(),true);
  assert.equal(h.calls.filter(c=>c.body.action==="claim").length,1);
 }finally{r.close();await r.settle();}
});
test("malformed reproof wake fences rather than opening another history",async()=>{
 const h=harness();let begun=0;
 const r=runtime({async beginSeatReproof(){begun++;}});
 try{
  await r.attachSeat();r.completeEngineBootstrap();historyWake(h,0,{schema:1,authority:true});
  await eventually(()=>!r.canAcceptMutations());
  assert.equal(begun,0);assert.equal(h.historyCalls.length,1);
 }finally{r.close();await r.settle();}
});
test("wake during completion schedules another same-producer proof without publishing an unchecked suffix",async()=>{
 const h=harness(),gate=deferred();let completing=0,published=0;
 const r=runtime({async completeSeatReproof(_coordinate,_completion,requireOwner){
  completing++;if(completing===1)await gate.promise;requireOwner();
  return {acknowledgedEventCount:0,publish(){requireOwner();published++;}};
 }});
 try{
  await r.attachSeat();r.completeEngineBootstrap();historyWake(h,0);
  await eventually(()=>completing===1);historyWake(h,1);await idle();gate.resolve();
  await eventually(()=>published===2);assert.equal(h.historyCalls.length,3);
  assert.equal(h.encoded.length,0);assert.equal(h.calls.filter(c=>c.body.action==="claim").length,1);
 }finally{gate.resolve();r.close();await r.settle();}
});

test("reproof wake cannot discard an unresolved append interval",async()=>{
 const h=harness(),events=[],r=runtime({},events);
 const timeout=globalThis.setTimeout;
 try{
  await r.attachSeat();r.completeEngineBootstrap();h.lost=5;
  globalThis.setTimeout=callback=>{queueMicrotask(callback);return 0;};
  events.push({seq:0n,event_type:"seat.facet",payload:{draft:"unresolved"}});r.observe({});
  await eventually(()=>h.encoded.length===5);await idle();
  globalThis.setTimeout=timeout;
  const retainedBytes=[...h.storage.entries()];assert.equal(retainedBytes.length,1);
  historyWake(h,0);
  await eventually(()=>h.calls.some(call=>call.url.endsWith("/replay")&&call.body.receipt));
  await idle();assert.equal(r.canAcceptMutations(),false);
  assert.deepEqual([...h.storage.entries()],retainedBytes);
  assert.ok(h.encoded.every(body=>body===h.encoded[0]));
  assert.equal(h.historyCalls.length,1,"unresolved append must precede a new proof scan");
  assert.equal(h.calls.filter(c=>c.body.action==="claim").length,1);
 }finally{globalThis.setTimeout=timeout;r.close();await r.settle();}
});

test("replay completion is available only after authenticated handoff and is copied", async()=>{
 harness();
 const r=runtime();
 assert.equal(r.currentReplayCompletion(),null);
 try {
  await r.attachSeat();
  const completion=r.currentReplayCompletion();
  assert.equal(completion.thread_seq,2n);
  assert.equal(completion.event_count,2n);
  completion.tip_hash="forged";
  assert.notEqual(r.currentReplayCompletion().tip_hash,"forged");
 } finally {r.close();}
 assert.equal(r.currentReplayCompletion(),null);
});

test("read-only producer observation narrows exact membership without claiming",async()=>{
 const h=harness(),r=runtime({presentation:coord("A")});
 const post=h.post.bind(h);
 h.post=async(url,body)=>{
  if(body.action!=="observe")return post(url,body);
  h.calls.push({url,body});
  return {...coord("A"),revision:"9007199254740993",root_binding:root,control_predecessor:controlPredecessor,producer:{kind:"active",producer_incarnation:"b".repeat(64)}};
 };
 try{
  const copy=r.preparedPresentation();copy.presentation_id="forged";
  const observed=await r.observeProducer();
  assert.equal(observed.presentation_id,"A");
  assert.equal(observed.revision,9007199254740993n);
  assert.equal(observed.producer.producer_incarnation,"b".repeat(64));
  assert.equal(r.currentProducer(),null);
  assert.equal(r.canAcceptMutations(),false);
  assert.deepEqual(h.calls,[{url:"/ui/api/session/seat/open",body:{action:"observe",...coord("A")}}]);
 }finally{r.close();}
});
test("producer observation refuses contradictory coordinates and noncanonical states",async()=>{
 const h=harness(),r=runtime({presentation:coord("A")});
 let response;
 h.post=async()=>response;
 try{
  const baseline={...coord("A"),revision:0,root_binding:root,control_predecessor:controlPredecessor,producer:{kind:"active",producer_incarnation:"a".repeat(64)}};
  for(const field of ["presentation_id","thread_id","chain_root_id"]){
   response={...baseline,[field]:"foreign"};
   await assert.rejects(r.observeProducer(),/coordinate/);
  }
  response={...baseline,root_binding:{...root,binding_generation:2n}};
  await assert.rejects(r.observeProducer(),/root binding/);
  for(const revision of [undefined,-1,0.5,Number.MAX_SAFE_INTEGER+1,"01"]){
   response={...baseline,revision};
   await assert.rejects(r.observeProducer(),/revision/);
  }
  for(const producer of [{kind:["active"],producer_incarnation:"a".repeat(64)},
   {kind:"future"},{kind:"active",producer_incarnation:"A".repeat(64)},
   {kind:"unclaimed",producer_incarnation:"a".repeat(64)},
   {kind:"active",producer_incarnation:"a".repeat(64),unexpected:true}]){
   response={...baseline,producer};
   await assert.rejects(r.observeProducer(),/observation/);
  }
  assert.equal(r.currentProducer(),null);
 }finally{r.close();}
});
test("late producer observation cannot revive a disposed document",async()=>{
 harness();const r=runtime({presentation:coord("A")});let resolve;
 globalThis.seatHarness.post=()=>new Promise(done=>{resolve=done;});
 const pending=r.observeProducer();r.close();
 resolve({...coord("A"),revision:0,root_binding:root,control_predecessor:initializationPredecessor,producer:{kind:"unclaimed"}});
 await assert.rejects(pending,/disposed/);
 assert.equal(r.currentProducer(),null);
});
test("fresh preparation and claims use distinct presentation coordinates",async()=>{
 const h=harness(),a=runtime(),b=runtime();
 try{await a.attachSeat();await b.attachSeat();assert.equal(a.currentProducer().presentation_id,"A");
 assert.equal(b.currentProducer().presentation_id,"B");
 assert.ok(h.calls.filter(c=>c.body.action==="claim").every(c=>c.body.claim.kind==="fresh"));
 }finally{a.close();b.close();}
});
test("claim refuses noncanonical producers and unsafe or exhausted engine frontiers before replay",async()=>{
 const invalid=[
  ...[null,"","a".repeat(63),"A".repeat(64),"g".repeat(64)].map(producer_incarnation=>({producer_incarnation})),
  ...[undefined,null,-1,"-1","01","+0",Number.MAX_SAFE_INTEGER+1,
   "18446744073709551615","18446744073709551616"].map(next_engine_seq=>({next_engine_seq})),
 ];
 for(const fields of invalid){
  const h=harness(),post=h.post.bind(h),replayed=[];
  h.post=async(url,body)=>{const response=await post(url,body);return body.action==="claim"?{...response,...fields}:response;};
  const r=runtime({replaySeatEvents:events=>replayed.push(events)});
  try{await assert.rejects(r.attachSeat(),/producer incarnation|producer_incarnation|next_engine_seq|sequence is exhausted/);
   assert.equal(r.currentProducer(),null);assert.equal(r.currentEngineFrontier(),null);
   assert.equal(replayed.length,0);assert.equal(h.calls.filter(call=>call.url.endsWith("/replay")).length,0);
  }finally{r.close();}
 }
});
test("claim preserves a large exact u64 frontier without Number conversion",async()=>{
 const h=harness(),post=h.post.bind(h);
 h.post=async(url,body)=>{const response=await post(url,body);return body.action==="claim"
  ?{...response,next_engine_seq:"18446744073709551614"}:response;};
 const r=runtime();
 try{await r.attachSeat();assert.equal(r.currentEngineFrontier(),18446744073709551614n);}
 finally{r.close();}
});
test("engine exhaustion commits the fitting prefix before refusing the remaining event",async()=>{
 const h=harness(),post=h.post.bind(h),reported=[];
 h.post=async(url,body)=>{const response=await post(url,body);return body.action==="claim"
  ?{...response,next_engine_seq:"18446744073709551613"}:response;};
 const events=[0,1,2].map(seq=>({seq:BigInt(seq),event_type:"seat.facet",payload:{draft:`edit-${seq}`}}));
 const r=runtime({commitEvent:event=>reported.push(event)},events);
 try{
  await r.attachSeat();
  await until(()=>reported.some(event=>event.error?.error?.includes("exhausted")));
  assert.equal(h.encoded.length,1);
  const append=JSON.parse(h.encoded[0]);
  assert.equal(append.first_engine_seq,"18446744073709551613");
  assert.equal(append.last_engine_seq,"18446744073709551614");
  assert.equal(append.event_count,2);
  assert.equal(h.identityInputs[0].first_engine_seq,18446744073709551613n);
  assert.equal(h.identityInputs[0].last_engine_seq,18446744073709551614n);
  assert.ok(reported.some(event=>event.type==="seat_append_acknowledged"&&event.events.length===2));
  assert.equal(r.currentEngineFrontier(),null,"the remaining event fences further mutation");
  assert.ok(reported.some(event=>event.error?.error?.includes("exhausted")));
 }finally{r.close();}
});
test("incomplete fixed-anchor stream retries bounded fresh proofs and never promotes a partial reconstruction",async()=>{
 const h=harness(),staged=[],aborted=[];
 h.historyFrames=[{event:"ui_seat.replay.begin.v1",data:{schema:1,...coord("A")}}];
 globalThis.fetch=async(_url,init)=>{h.historyCalls.push(JSON.parse(init.body));const body=new ReadableStream({start(controller){controller.enqueue(new TextEncoder().encode(`event: ui_seat.replay.begin.v1\ndata: ${JSON.stringify(h.historyFrames[0].data)}\n\n`));controller.close();}});return new Response(body,{headers:{"Content-Type":"text/event-stream"}});};
 const r=runtime({replaySeatEvents:events=>staged.push(...events),abortSeatReplay:()=>aborted.push(true)});
 try{await assert.rejects(r.attachSeat(),/ended/);assert.equal(staged.length,0);assert.equal(aborted.length,3);
  assert.equal(h.historyCalls.length,3);assert.equal(h.calls.filter(call=>call.body.action==="claim").length,1);
  assert.equal(r.currentProducer(),null);}
 finally{r.close();}
});
test("an advanced cold anchor restarts one provisional proof without rotating the claimed producer",async()=>{
 const h=harness(),completeFetch=globalThis.fetch,staged=[],aborted=[];
 let calls=0;
 globalThis.fetch=async(url,init)=>{
  if(++calls>1)return completeFetch(url,init);
  h.historyCalls.push(JSON.parse(init.body));
  const body=new ReadableStream({start(controller){
   controller.enqueue(new TextEncoder().encode(`event: ui_seat.replay.begin.v1\ndata: ${JSON.stringify({schema:1,...coord("A")})}\n\n`));
   controller.close();
  }});
  return new Response(body,{headers:{"Content-Type":"text/event-stream"}});
 };
 const r=runtime({replaySeatEvents:events=>staged.push(...events),abortSeatReplay:()=>aborted.push(true)});
 try{await r.attachSeat();assert.equal(h.historyCalls.length,2);assert.equal(aborted.length,1);
  assert.equal(staged.length,2);assert.equal(h.calls.filter(call=>call.body.action==="claim").length,1);
  assert.equal(r.currentProducer().presentation_id,"A");}
 finally{r.close();}
});
test("fixed-anchor stream uses the exact claimed seat and no JSON cold-page route",async()=>{
 const h=harness(),published=[];
 const r=runtime({replaySeatEvents:events=>published.push(...events)});
 try{await r.attachSeat();assert.equal(published.length,2);assert.deepEqual(h.historyCalls,[{presentation_id:"A",chain_root_id:"chain-A"}]);
  assert.equal(h.calls.some(call=>call.url.endsWith("/replay")&&!call.body.receipt),false);
 }finally{r.close();}
});
test("replacement reconstructs the complete prior control history before its claim",async()=>{
 const h=harness(),first=runtime();let replacement;
 try{
  await first.attachSeat();
  const priorRequest=h.lastClaimRequest,priorResult=h.lastClaimResult;
  const post=h.post.bind(h);
  h.post=async(url,body)=>{
   const result=await post(url,body);
   return body.action==="claim" ? {...result,producer_incarnation:"b".repeat(64)} : result;
  };
  const restored=[];
  globalThis.fetch=async(_url,init)=>{
   h.historyCalls.push(JSON.parse(init.body));
   const c=coord("A"),latestRequest=h.lastClaimRequest,latestResult=h.lastClaimResult;
   const initialized={event_hash:"d".repeat(64),chain_root_id:c.chain_root_id,thread_id:c.thread_id,
    thread_seq:1,prev_thread_event_hash:"0".repeat(64),chain_seq:1,event_type:"ui_seat.presentation_initialized.v1",payload:{}};
   const prior={event_hash:"c".repeat(64),chain_root_id:c.chain_root_id,thread_id:c.thread_id,
    thread_seq:2,prev_thread_event_hash:initialized.event_hash,chain_seq:2,event_type:"ui_seat.producer.v3",
    payload:{schema_version:"ryeos.ui.seat.producer.v3",operation_id:priorResult.operation_id,
     producer_incarnation:priorResult.producer_incarnation,next_engine_seq:priorResult.next_engine_seq,
     state:"active",request:priorRequest}};
   const latest={event_hash:"e".repeat(64),chain_root_id:c.chain_root_id,thread_id:c.thread_id,
    thread_seq:3,prev_thread_event_hash:prior.event_hash,chain_seq:3,event_type:"ui_seat.producer.v3",
    payload:{schema_version:"ryeos.ui.seat.producer.v3",operation_id:latestResult.operation_id,
     producer_incarnation:latestResult.producer_incarnation,next_engine_seq:latestResult.next_engine_seq,
     state:"active",request:latestRequest}};
   const frames=[{event:"ui_seat.replay.begin.v1",data:{schema:1,...c}},
    {event:"ui_seat.replay.events.v1",data:{schema:1,events:[initialized,prior,latest]}},
    {event:"ui_seat.replay.complete.v1",data:{schema:1,tip_hash:latest.event_hash,thread_seq:3,event_count:3}}];
   const body=new ReadableStream({start(controller){for(const frame of frames)
    controller.enqueue(new TextEncoder().encode(`event: ${frame.event}\ndata: ${JSON.stringify(frame.data)}\n\n`));}});
   return new Response(body,{headers:{"Content-Type":"text/event-stream"}});
  };
  replacement=runtime({presentation:coord("A"),claim:{kind:"takeover",
   expected_producer_incarnation:"a".repeat(64),predecessor:controlPredecessor},
   deriveSeatControlIdentity(){return "33333333-3333-8333-8333-333333333333";},
   replaySeatEvents:events=>restored.push(...events)});
  await replacement.attachSeat();
  assert.deepEqual(restored.map(event=>event.event_hash),["d".repeat(64),"c".repeat(64),"e".repeat(64)]);
  assert.equal(h.lastClaimRequest.predecessor.event_hash,"c".repeat(64));
  assert.notEqual(h.lastClaimRequest.operation_id,priorRequest.operation_id);
  assert.notEqual(h.lastClaimResult.producer_incarnation,priorResult.producer_incarnation);
  assert.equal(restored.at(-1).payload.operation_id,h.lastClaimResult.operation_id);
  assert.equal(restored.at(-1).payload.producer_incarnation,h.lastClaimResult.producer_incarnation);
 }finally{first.close();replacement?.close();}
});
test("explicit reconnect creates an unattached predecessor-fenced replacement",async()=>{
 const h=harness(),r=runtime();let next;
 try{await r.attachSeat();const calls=h.calls.length;
  next=await r.reconnect({kind:"takeover",expected_producer_incarnation:"b".repeat(64),predecessor:controlPredecessor});
  assert.equal(h.calls.length,calls);assert.equal(next.currentProducer(),null);
  assert.equal(r.currentProducer().presentation_id,"A");
  await next.attachSeat();const claim=h.calls.filter(c=>c.body.action==="claim").at(-1).body;
  assert.deepEqual(claim.claim,{kind:"takeover",expected_producer_incarnation:"b".repeat(64)});
  assert.equal(claim.presentation_id,"A");
 }finally{r.close();next?.close();}
});
test("resume retains its exact predecessor and independent choice prepares a distinct presentation",async()=>{
 const h=harness(),r=runtime();let resumed,duplicate;
 try{await r.attachSeat();await r.relinquish();
  resumed=await r.reconnect({kind:"resume",expected_producer_incarnation:"a".repeat(64),predecessor:controlPredecessor});
  await resumed.attachSeat();assert.equal(h.calls.filter(c=>c.body.action==="claim").at(-1).body.claim.kind,"resume");
  duplicate=await resumed.reconnect({kind:"independent"});assert.equal(duplicate.currentProducer(),null);
  await duplicate.attachSeat();assert.equal(duplicate.currentProducer().presentation_id,"B");
  assert.equal(h.calls.filter(c=>c.body.action==="claim").at(-1).body.claim.kind,"fresh");
 }finally{r.close();resumed?.close();duplicate?.close();}
});
test("authenticated missing old receipt resends exact retained bytes before reconnect",async()=>{
 const h=harness(),r=runtime({presentation:coord("A")});
 await retained(h);h.notFound=true;
 const order=[];const append=h.append.bind(h),post=h.post.bind(h);
 h.append=body=>{order.push("append");return append(body);};
 h.post=async(url,body)=>{if(body.action==="claim")order.push("claim");return post(url,body);};
 try{const record=JSON.parse([...h.storage.values()][0]);
  const next=await r.reconnect({kind:"takeover",expected_producer_incarnation:"a".repeat(64),predecessor:controlPredecessor});
  assert.equal(next.currentProducer(),null);
  assert.deepEqual(h.encoded,[record.encoded_request]);
  assert.equal(h.storage.size,0);
  assert.equal(h.calls.filter(c=>c.body.action==="claim"||c.body.action==="prepare").length,0);
  await next.attachSeat();
  assert.deepEqual(order,["append","claim"]);
  assert.equal(h.calls.filter(c=>c.body.action==="claim").length,1);
  next.close();
 }finally{r.close();}
});
test("concurrent delayed claim refuses provisional startup mutations without renumbering",async()=>{
 const h=harness();let release,started;const sent=new Promise(resolve=>{started=resolve;});
 const post=h.post.bind(h);h.post=async(url,body)=>{
  if(body.action==="claim"){started();await new Promise(resolve=>{release=resolve;});}
  return post(url,body);
 };
 const events=[],r=runtime({seatEvents:()=>events},events);
 try{const first=r.attachSeat();assert.equal(r.attachSeat(),first);await sent;
  assert.equal(r.currentProducer(),null);assert.equal(h.encoded.length,0);
  events.push({seq:1n,event_type:"seat.facet",payload:{draft:"during claim"}});
  release();await assert.rejects(first,/provisional seat events/);
  assert.equal(h.calls.filter(c=>c.body.action==="claim").length,1);
  assert.equal(h.encoded.length,0);assert.equal(events[0].seq,1n);
 }finally{r.close();}
});
test("exhausted unknown prepare retries the same request within one runtime only",async()=>{
 const h=harness();h.controlLost=5;const r=runtime();const timeout=globalThis.setTimeout;
 globalThis.setTimeout=callback=>{queueMicrotask(callback);return 0;};
 try{await assert.rejects(r.attachSeat(),/control delivery unknown/);assert.equal(r.currentProducer(),null);
  assert.equal(h.controls.length,5);assert.ok(h.controls.every(body=>body===h.controls[0]));
  await r.attachSeat();assert.equal(h.controls[5],h.controls[0]);
  const sibling=runtime();try{await sibling.attachSeat();
   const preparations=h.controls.map(JSON.parse).filter(body=>body.action==="prepare");
   assert.notEqual(preparations.at(-1).creation_request_id,preparations[0].creation_request_id);
  }finally{sibling.close();}
 }finally{r.close();globalThis.setTimeout=timeout;}
});
test("exhausted unknown claim never substitutes another operation",async()=>{
 const h=harness(),post=h.post.bind(h);
 // Lose delivery before the mock server response, retaining the exact claim bytes.
 h.post=async(url,body)=>{if(body.action==="prepare"){const prepared=await post(url,body);h.controlLost=5;return prepared;}return post(url,body);};
 const r=runtime();const timeout=globalThis.setTimeout;globalThis.setTimeout=callback=>{queueMicrotask(callback);return 0;};
 try{await assert.rejects(r.attachSeat(),/control delivery unknown/);assert.equal(r.currentProducer(),null);
  const claims=h.controls.filter(body=>JSON.parse(body).action==="claim");assert.equal(claims.length,5);
  await r.attachSeat();const retried=h.controls.filter(body=>JSON.parse(body).action==="claim");
  assert.equal(retried.length,6);assert.ok(retried.every(body=>body===claims[0]));
  assert.equal(h.calls.filter(c=>c.body.action==="prepare").length,1);
  const frozen=JSON.parse(claims[0]);assert.equal(frozen.schema,2);
  assert.deepEqual(frozen.predecessor,initializationPredecessor);assert.equal(frozen.operation_id,derivedControlId);
  assert.match(frozen.intent_nonce,/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  assert.equal(h.controlIdentityInputs.length,1);assert.equal(h.controlIdentityInputs[0].operation_id,"");
 }finally{r.close();globalThis.setTimeout=timeout;}
});
test("replacement keeps the selected control predecessor without re-observing into a new intent",async()=>{
 const h=harness(),selected={kind:"control",event_hash:"e".repeat(64)};
 const r=runtime({presentation:coord("A"),claim:{kind:"takeover",expected_producer_incarnation:"a".repeat(64),predecessor:selected}});
 try{await r.attachSeat();const claim=h.calls.find(call=>call.body.action==="claim").body;
  assert.deepEqual(claim.predecessor,selected);assert.equal(claim.chain_root_id,"chain-A");
  assert.equal(h.calls.filter(call=>call.body.action==="observe").length,0);
 }finally{r.close();}
});
test("fresh claim refuses an observation without authenticated predecessor before deriving or sending",async()=>{
 const h=harness(),post=h.post.bind(h);
 h.post=(url,body)=>body.action==="observe"?Promise.resolve({...coord(body.presentation_id),revision:0,root_binding:root,producer:{kind:"unclaimed"}}):post(url,body);
 const r=runtime();try{await assert.rejects(r.attachSeat(),/exact control predecessor/);
  assert.equal(h.controlIdentityInputs.length,0);assert.equal(h.controls.filter(body=>JSON.parse(body).action==="claim").length,0);
 }finally{r.close();}
});
test("independent documents create distinct control intent nonces",async()=>{
 const h=harness(),a=runtime(),b=runtime();try{await a.attachSeat();await b.attachSeat();
  assert.equal(h.controlIdentityInputs.length,2);
  assert.notEqual(h.controlIdentityInputs[0].intent_nonce,h.controlIdentityInputs[1].intent_nonce);
 }finally{a.close();b.close();}
});
test("exhausted unknown append preserves its exact interval and blocks replacement",async()=>{
 const h=harness();h.lost=5;const r=runtime({},[{seq:0n,event_type:"seat.facet",payload:{draft:"unknown"}}]);
 const timeout=globalThis.setTimeout;globalThis.setTimeout=callback=>{queueMicrotask(callback);return 0;};
 try{await r.attachSeat();await until(()=>h.encoded.length>=5);await idle();
  assert.equal(h.encoded.length,5);assert.ok(h.encoded.every(body=>body===h.encoded[0]));assert.equal(h.storage.size,1);
  assert.equal(r.currentProducer(),null);assert.equal(r.canAcceptMutations(),false);
  r.close();h.notFound=true;const replacement=runtime({presentation:coord("A"),claim:{kind:"takeover",expected_producer_incarnation:"a".repeat(64),predecessor:controlPredecessor}});
  try{await replacement.attachSeat();assert.equal(h.storage.size,0);
   assert.equal(h.encoded[5],h.encoded[0]);
   assert.equal(h.calls.filter(c=>c.body.action==="claim").length,2);
  }finally{replacement.close();}
 }finally{r.close();globalThis.setTimeout=timeout;}
});
async function unknownCurrent(h,r) {
 const timeout=globalThis.setTimeout;globalThis.setTimeout=callback=>{queueMicrotask(callback);return 0;};h.lost=5;
 try{await r.attachSeat();await until(()=>h.encoded.length>=5);await idle();}
 finally{globalThis.setTimeout=timeout;}
 assert.equal(r.currentProducer(),null);
 h.receipt=ack(JSON.parse(h.encoded[0]));
}
test("found current receipt advances only proven prefix and resumes the same writer",async()=>{
 const committed=[];
 const h=harness(),events=[{seq:71n,event_type:"seat.facet",payload:{draft:"accepted"}}],r=runtime({commitEvent:event=>committed.push(event)},events);
 try{await unknownCurrent(h,r);r.completeEngineBootstrap();
  events.push({seq:1n,event_type:"seat.facet",payload:{draft:"suffix"}});
  await r.reconcilePendingAppend();await until(()=>h.encoded.length>=6);
  const localAck=committed.find(event=>event.type==="seat_append_acknowledged");
  assert.deepEqual(localAck.events,[events[0]],"receipt acknowledges captured local prefix, not remapped wire sequence");
  assert.equal(localAck.events[0].seq,71n);
  assert.equal(localAck.canonical.length,1);
  assert.notEqual(BigInt(localAck.canonical[0].chain_seq),71n,"original native coordinate is not grafted onto the provisional local sequence");
  assert.match(localAck.canonical[0].event_hash,/^[0-9a-f]{64}$/);
  assert.equal(localAck.producer.presentation_id,"A");
  assert.equal(h.calls.filter(c=>c.body.action==="claim").length,1);
  const suffix=JSON.parse(h.encoded[5]);assert.equal(suffix.first_engine_seq,"1");
  assert.deepEqual(suffix.events.map(e=>e.payload.draft),["suffix"]);
  assert.equal(r.currentProducer().producer_incarnation,"a".repeat(64));
 }finally{r.close();}
});
test("copied historical receipt cannot consume a current producer queue",async()=>{
 const committed=[];
 const h=harness(),r=runtime({commitEvent:event=>committed.push(event)},[{seq:0n,event_type:"seat.facet",payload:{draft:"current"}}]);
 try{await unknownCurrent(h,r);await retained(h);const record=[...h.storage.values()][0];
  await assert.rejects(r.reconcilePendingAppend(),/current producer's pending interval/);
  assert.equal([...h.storage.values()][0],record);assert.equal(r.currentProducer(),null);
  assert.equal(committed.some(event=>event.type==="seat_append_acknowledged"),false);
 }finally{r.close();}
});

test("successful append acknowledges exact local events rather than wire positions",async()=>{
 const h=harness(),committed=[];
 const local={seq:91n,event_type:"seat.facet",payload:{draft:"local"}};
 const r=runtime({commitEvent:event=>committed.push(event)},[local]);
 try{await r.attachSeat();await until(()=>committed.some(event=>event.type==="seat_append_acknowledged"));
  assert.equal(JSON.parse(h.encoded[0]).events[0].engine_seq,"0");
  const acknowledgement=committed.find(event=>event.type==="seat_append_acknowledged");
  assert.deepEqual(acknowledgement.events,[local]);
  assert.equal(acknowledgement.producer.producer_incarnation,"a".repeat(64));
 }finally{r.close();}
});

test("verified frame before HTTP ACK never folds twice and keeps exact pending recovery",async()=>{
 const h=harness(),committed=[],unavailable=[];
 let streamController,deliver,started;
 const sent=new Promise(resolve=>{started=resolve;});
 const emit=(event,data)=>streamController.enqueue(new TextEncoder().encode(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`));
 globalThis.fetch=async()=>new Response(new ReadableStream({start(controller){
  streamController=controller;
  const c=coord("A"),zero="0".repeat(64),a="d".repeat(64),b="c".repeat(64);
  emit("ui_seat.replay.begin.v1",{schema:1,...c});
  emit("ui_seat.replay.events.v1",{schema:1,events:[{event_hash:a,chain_root_id:c.chain_root_id,
   thread_id:c.thread_id,thread_seq:1,prev_thread_event_hash:zero,chain_seq:1,
   event_type:"ui_seat.presentation_initialized.v1",payload:{}},{event_hash:b,chain_root_id:c.chain_root_id,
   thread_id:c.thread_id,thread_seq:2,prev_thread_event_hash:a,chain_seq:2,
   event_type:"ui_seat.producer.v3",payload:{schema_version:"ryeos.ui.seat.producer.v3",
   operation_id:h.lastClaimResult.operation_id,producer_incarnation:h.lastClaimResult.producer_incarnation,
   next_engine_seq:h.lastClaimResult.next_engine_seq,state:"active",request:h.lastClaimRequest}}]});
  emit("ui_seat.replay.complete.v1",{schema:1,tip_hash:b,thread_seq:2,event_count:2});
 }}),{headers:{"Content-Type":"text/event-stream"}});
 h.append=body=>new Promise(resolve=>{deliver=()=>resolve(ack(body));started();});
 const local={seq:91n,event_type:"seat.facet",payload:{draft:"local"}};
 const r=runtime({commitEvent:event=>committed.push(event),onUnavailable:failure=>unavailable.push(failure)},[local]);
 try{
  await r.attachSeat();await sent;
  const pending=JSON.parse([...h.storage.values()][0]);
  const verified={schema:1,operation_id:pending.operation_id,producer_incarnation:pending.producer_incarnation,
   first_engine_seq:pending.first_engine_seq,last_engine_seq:pending.last_engine_seq,
   event_count:pending.event_count,payload_digest:pending.payload_digest,
   event_hash:"e".repeat(64),thread_seq:4};
  emit("ui_seat.replay.append_verified.v1",verified);
  emit("ui_seat.replay.append_verified.v1",verified);
  await idle();
  assert.equal(h.storage.size,1);
  assert.equal(committed.filter(event=>event.type==="seat_append_acknowledged").length,0);
  deliver();
  await until(()=>committed.some(event=>event.type==="seat_append_acknowledged"));
  assert.equal(committed.filter(event=>event.type==="seat_append_acknowledged").length,1);
  assert.equal(unavailable.length,0);
 }finally{r.close();}
});

test("stream-before-ACK receipt hash disagreement fences the pending append",async()=>{
 const h=harness(),committed=[],unavailable=[];
 let streamController,deliver,started;
 const sent=new Promise(resolve=>{started=resolve;});
 const emit=(event,data)=>streamController.enqueue(new TextEncoder().encode(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`));
 globalThis.fetch=async()=>new Response(new ReadableStream({start(controller){
  streamController=controller;
  const c=coord("A"),zero="0".repeat(64),a="d".repeat(64),b="c".repeat(64);
  emit("ui_seat.replay.begin.v1",{schema:1,...c});
  emit("ui_seat.replay.events.v1",{schema:1,events:[
   {event_hash:a,chain_root_id:c.chain_root_id,thread_id:c.thread_id,thread_seq:1,
    prev_thread_event_hash:zero,chain_seq:1,event_type:"ui_seat.presentation_initialized.v1",payload:{}},
   {event_hash:b,chain_root_id:c.chain_root_id,thread_id:c.thread_id,thread_seq:2,
    prev_thread_event_hash:a,chain_seq:2,event_type:"ui_seat.producer.v3",
    payload:{schema_version:"ryeos.ui.seat.producer.v3",operation_id:h.lastClaimResult.operation_id,
     producer_incarnation:h.lastClaimResult.producer_incarnation,next_engine_seq:h.lastClaimResult.next_engine_seq,
     state:"active",request:h.lastClaimRequest}}
  ]});
  emit("ui_seat.replay.complete.v1",{schema:1,tip_hash:b,thread_seq:2,event_count:2});
 }}),{headers:{"Content-Type":"text/event-stream"}});
 h.append=body=>new Promise(resolve=>{deliver=()=>resolve({...ack(body),receipt_event_hash:"f".repeat(64)});started();});
 const local={seq:91n,event_type:"seat.facet",payload:{draft:"local"}};
 const r=runtime({commitEvent:event=>committed.push(event),onUnavailable:failure=>unavailable.push(failure)},[local]);
 try{
  await r.attachSeat();await sent;
  const pending=JSON.parse([...h.storage.values()][0]);
  emit("ui_seat.replay.append_verified.v1",{schema:1,operation_id:pending.operation_id,
   producer_incarnation:pending.producer_incarnation,first_engine_seq:pending.first_engine_seq,
   last_engine_seq:pending.last_engine_seq,event_count:pending.event_count,
   payload_digest:pending.payload_digest,event_hash:"e".repeat(64),thread_seq:4});
  await idle();deliver();
  await until(()=>unavailable.length>0);
  assert.equal(committed.some(event=>event.type==="seat_append_acknowledged"),false);
  assert.equal(h.storage.size,1);
  assert.ok(unavailable.length>0);
 }finally{r.close();}
});

test("ACK-before-stream receipt position disagreement invalidates the live handoff",async()=>{
 const h=harness(),committed=[],unavailable=[];
 let streamController;
 const emit=(event,data)=>streamController.enqueue(new TextEncoder().encode(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`));
 globalThis.fetch=async()=>new Response(new ReadableStream({start(controller){
  streamController=controller;
  const c=coord("A"),zero="0".repeat(64),a="d".repeat(64),b="c".repeat(64);
  emit("ui_seat.replay.begin.v1",{schema:1,...c});
  emit("ui_seat.replay.events.v1",{schema:1,events:[
   {event_hash:a,chain_root_id:c.chain_root_id,thread_id:c.thread_id,thread_seq:1,
    prev_thread_event_hash:zero,chain_seq:1,event_type:"ui_seat.presentation_initialized.v1",payload:{}},
   {event_hash:b,chain_root_id:c.chain_root_id,thread_id:c.thread_id,thread_seq:2,
    prev_thread_event_hash:a,chain_seq:2,event_type:"ui_seat.producer.v3",
    payload:{schema_version:"ryeos.ui.seat.producer.v3",operation_id:h.lastClaimResult.operation_id,
     producer_incarnation:h.lastClaimResult.producer_incarnation,next_engine_seq:h.lastClaimResult.next_engine_seq,
     state:"active",request:h.lastClaimRequest}}
  ]});
  emit("ui_seat.replay.complete.v1",{schema:1,tip_hash:b,thread_seq:2,event_count:2});
 }}),{headers:{"Content-Type":"text/event-stream"}});
 const local={seq:92n,event_type:"seat.facet",payload:{draft:"local"}};
 const r=runtime({commitEvent:event=>committed.push(event),onUnavailable:failure=>unavailable.push(failure)},[local]);
 try{
  await r.attachSeat();
  await until(()=>committed.some(event=>event.type==="seat_append_acknowledged"));
  assert.equal(committed.filter(event=>event.type==="seat_append_acknowledged").length,1);
  const receipt=h.receipt;
  emit("ui_seat.replay.append_verified.v1",{schema:1,operation_id:receipt.operation_id,
   producer_incarnation:receipt.producer_incarnation,first_engine_seq:receipt.first_engine_seq,
   last_engine_seq:receipt.last_engine_seq,event_count:receipt.event_count,
   payload_digest:receipt.payload_digest,event_hash:receipt.receipt_event_hash,thread_seq:99});
  await until(()=>unavailable.length>0);
  assert.ok(unavailable.length>0);
 }finally{r.close();}
});

test("live acknowledgement permits distinct canonical chain and thread predecessors for its first event",async()=>{
 const h=harness(),committed=[],append=h.append.bind(h);
 h.append=body=>{const receipt=append(body);receipt.events[0].prev_thread_event_hash="e".repeat(64);return receipt;};
 const local={seq:92n,event_type:"seat.facet",payload:{draft:"distinct predecessors"}};
 const r=runtime({commitEvent:event=>committed.push(event)},[local]);
 try{await r.attachSeat();await until(()=>committed.some(event=>event.type==="seat_append_acknowledged"));
  assert.deepEqual(committed.find(event=>event.type==="seat_append_acknowledged").events,[local]);
  assert.equal(h.storage.size,0);
 }finally{r.close();}
});

test("unknown append notifies document owner without claiming writer death",async()=>{
 const h=harness(),unavailable=[];
 const r=runtime({onUnavailable:failure=>unavailable.push(failure)},[{seq:0n,event_type:"seat.facet",payload:{draft:"pending"}}]);
 try{await unknownCurrent(h,r);
  assert.equal(unavailable.length,1);
  assert.equal(unavailable[0].kind,"append_unknown");
  assert.equal(h.calls.filter(call=>call.body.action==="claim").length,1);
  assert.equal(r.canAcceptMutations(),false);
 }finally{r.close();}
});
test("malformed successful append acknowledgement remains unknown, not refused",async()=>{
 const h=harness(),unavailable=[],committed=[];
 const append=h.append.bind(h);
 h.append=body=>{const receipt=append(body);delete receipt.receipt_thread_seq;return receipt;};
 const timeout=globalThis.setTimeout;
 globalThis.setTimeout=callback=>{queueMicrotask(callback);return 0;};
 const r=runtime({onUnavailable:failure=>unavailable.push(failure),commitEvent:event=>committed.push(event)},
  [{seq:0n,event_type:"seat.facet",payload:{draft:"retained"}}]);
 try{
  await r.attachSeat();
  await until(()=>unavailable.length>0);
  assert.equal(h.encoded.length,5);
  assert.ok(h.encoded.every(encoded=>encoded===h.encoded[0]),"only exact retained bytes are retried");
  assert.equal(h.storage.size,1);
  assert.equal(unavailable[0]?.kind,"append_unknown");
  assert.equal(committed.some(event=>event.type==="seat_append_acknowledged"),false);
  assert.equal(r.canAcceptMutations(),false);
 }finally{r.close();globalThis.setTimeout=timeout;}
});
test("HTTP 408 append timeout preserves unknown outcome and exact retry bytes",async()=>{
 const h=harness(),unavailable=[],committed=[];
 h.timeout=5;
 const timeout=globalThis.setTimeout;
 globalThis.setTimeout=callback=>{queueMicrotask(callback);return 0;};
 const r=runtime({onUnavailable:failure=>unavailable.push(failure),commitEvent:event=>committed.push(event)},
  [{seq:0n,event_type:"seat.facet",payload:{draft:"retained"}}]);
 try{
  await r.attachSeat();
  await until(()=>unavailable.length>0);
  assert.equal(h.encoded.length,5);
  assert.ok(h.encoded.every(encoded=>encoded===h.encoded[0]));
  assert.equal(unavailable[0]?.kind,"append_unknown");
  assert.equal(h.storage.size,1);
  assert.equal(committed.some(event=>event.type==="seat_append_acknowledged"),false);
  assert.equal(h.calls.filter(call=>call.body.action==="claim").length,1);
  assert.equal(r.canAcceptMutations(),false);
 }finally{r.close();globalThis.setTimeout=timeout;}
});
test("HTTP 429 append response preserves unknown outcome and exact retry bytes",async()=>{
 const h=harness(),unavailable=[],committed=[];
 h.rateLimit=5;
 const timeout=globalThis.setTimeout;
 globalThis.setTimeout=callback=>{queueMicrotask(callback);return 0;};
 const r=runtime({onUnavailable:failure=>unavailable.push(failure),commitEvent:event=>committed.push(event)},
  [{seq:0n,event_type:"seat.facet",payload:{draft:"retained"}}]);
 try{
  await r.attachSeat();
  await until(()=>unavailable.length>0);
  assert.equal(h.encoded.length,5);
  assert.ok(h.encoded.every(encoded=>encoded===h.encoded[0]));
  assert.equal(unavailable[0]?.kind,"append_unknown");
  assert.equal(h.storage.size,1);
  assert.equal(committed.some(event=>event.type==="seat_append_acknowledged"),false);
  assert.equal(h.calls.filter(call=>call.body.action==="claim").length,1);
  assert.equal(r.canAcceptMutations(),false);
 }finally{r.close();globalThis.setTimeout=timeout;}
});
test("HTTP 429 after commit reconciles one exact append on retry",async()=>{
 const h=harness(),committed=[],unavailable=[];
 h.rateLimitAfterCommit=1;
 const r=runtime({commitEvent:event=>committed.push(event),onUnavailable:failure=>unavailable.push(failure)},
  [{seq:0n,event_type:"seat.facet",payload:{draft:"committed before 429"}}]);
 try{
  await r.attachSeat();
  await until(()=>committed.some(event=>event.type==="seat_append_acknowledged"));
  assert.equal(h.encoded.length,2);
  assert.equal(h.encoded[0],h.encoded[1]);
  assert.equal(h.committedCount,1);
  assert.equal(h.storage.size,0);
  assert.equal(committed.filter(event=>event.type==="seat_append_acknowledged").length,1);
  assert.equal(unavailable.length,0);
 }finally{r.close();}
});
test("exhausted HTTP 5xx append retries cannot become a refusal",async()=>{
 const h=harness(),unavailable=[],committed=[];
 h.serverError=5;
 const timeout=globalThis.setTimeout;
 globalThis.setTimeout=callback=>{queueMicrotask(callback);return 0;};
 const r=runtime({onUnavailable:failure=>unavailable.push(failure),commitEvent:event=>committed.push(event)},
  [{seq:0n,event_type:"seat.facet",payload:{draft:"retained"}}]);
 try{
  await r.attachSeat();
  await until(()=>unavailable.length>0);
  assert.equal(h.encoded.length,5);
  assert.ok(h.encoded.every(encoded=>encoded===h.encoded[0]));
  assert.equal(unavailable[0]?.kind,"append_unknown");
  assert.equal(h.storage.size,1);
  assert.equal(committed.some(event=>event.type==="seat_append_acknowledged"),false);
  assert.equal(h.calls.filter(call=>call.body.action==="claim").length,1);
  assert.equal(r.canAcceptMutations(),false);
 }finally{r.close();globalThis.setTimeout=timeout;}
});
test("a changed queued prefix cannot be retired by an otherwise valid receipt",async()=>{
 const h=harness(),events=[{seq:0n,event_type:"seat.facet",payload:{draft:"original"}}],r=runtime({},events);
 try{await unknownCurrent(h,r);const record=[...h.storage.values()][0];events[0].payload.draft="changed";
  await assert.rejects(r.reconcilePendingAppend(),/exact queued prefix/);
  assert.equal([...h.storage.values()][0],record);assert.equal(r.currentProducer(),null);
 }finally{r.close();}
});
test("authentication reconciliation rejects a membership with another root",async()=>{
 const h=harness();h.auth=401;
 h.current=()=>({session_id:"session",binding_attachments:[{...root,project_path:null,subject:{kind:"projectless"}}],presentations:[{...coord("A"),root_binding:{...root,binding_digest:"f".repeat(64)}}]});
 const r=runtime({},[{seq:0n,event_type:"seat.facet",payload:{draft:"text"}}]);
 try{await r.attachSeat();await until(()=>h.encoded.length>0);await idle();
  assert.equal(h.encoded.length,1);assert.equal(h.storage.size,1);
 }finally{r.close();}
});
test("storage refusal fences new mutations before any append is sent",async()=>{
 const h=harness(),events=[],r=runtime({},events);
 try{await r.attachSeat();r.completeEngineBootstrap();assert.equal(r.canAcceptMutations(),true);
  globalThis.localStorage.setItem=()=>{throw new Error("storage unavailable");};
  events.push({seq:0n,event_type:"seat.facet",payload:{draft:"not durable"}});
  r.observe({});await until(()=>r.currentProducer()===null);
  assert.equal(r.currentProducer(),null);assert.equal(r.canAcceptMutations(),false);assert.equal(h.encoded.length,0);
 }finally{r.close();}
});
test("current producer retries unknown delivery with byte-identical append",async()=>{
 const h=harness();h.lost=1;const r=runtime({},[{seq:0n,event_type:"seat.facet",payload:{draft:"text"}}]);
 try{await r.attachSeat();await until(()=>h.encoded.length>=2);await idle();
 assert.equal(h.encoded.length,2);assert.equal(h.encoded[0],h.encoded[1]);assert.equal(h.storage.size,0);
 assert.equal(JSON.parse(h.encoded[0]).presentation_id,"A");}finally{r.close();}
});
for(const status of [401,403])test(`authentication ${status} reconciles presentation membership`,async()=>{
 const h=harness();h.auth=status;const r=runtime({},[{seq:0n,event_type:"seat.facet",payload:{draft:"text"}}]);
 try{await r.attachSeat();await until(()=>h.encoded.length>=2);await idle();assert.equal(h.encoded.length,2);
 assert.equal(h.encoded[0],h.encoded[1]);assert.equal(h.storage.size,0);}finally{r.close();}
});
test("authentication reconciliation refuses missing mandatory subject without resending",async()=>{
 const h=harness();h.auth=403;
 h.current=()=>({session_id:"session",binding_attachments:[root],presentations:[{...coord("A"),root_binding:root}]});
 const r=runtime({},[{seq:0n,event_type:"seat.facet",payload:{draft:"retained"}}]);
 try{await r.attachSeat();await until(()=>r.currentProducer()===null);
  assert.equal(h.encoded.length,1);assert.equal(h.storage.size,1);assert.equal(r.currentProducer(),null);
 }finally{r.close();}
});
test("replacement reconciles receipt before claim without resending",async()=>{
 const h=harness();await retained(h);
 const r=runtime({presentation:coord("A"),claim:{kind:"resume",expected_producer_incarnation:"a".repeat(64),predecessor:controlPredecessor}});
 try{await r.attachSeat();assert.equal(h.calls[0].body.receipt.thread_id,coord("A").thread_id);assert.equal(h.calls[1].body.action,"claim");
 assert.equal(h.encoded.length,0);assert.equal(h.storage.size,0);}finally{r.close();}
});
test("retained acknowledgement permits distinct canonical chain and thread predecessors for its first event",async()=>{
 const h=harness();await retained(h);h.receipt.events[0].prev_thread_event_hash="e".repeat(64);
 const r=runtime({presentation:coord("A"),claim:{kind:"resume",expected_producer_incarnation:"a".repeat(64),predecessor:controlPredecessor}});
 try{await r.attachSeat();assert.equal(h.storage.size,0);
  assert.equal(h.calls.filter(call=>call.body.action==="claim").length,1);
 }finally{r.close();}
});
test("retained reconciliation refuses substituted authoritative event evidence without clearing recovery",async()=>{
 const mutations=[
  receipt=>{receipt.events_authority="projection";},
  receipt=>{receipt.events[0].event_hash="A".repeat(64);},
  receipt=>{receipt.events[0].chain_root_id="foreign";},
  receipt=>{receipt.events[0].thread_id="foreign";},
  receipt=>{receipt.events[0].payload.payload={draft:"substituted"};},
  receipt=>{receipt.events[0].prev_chain_event_hash="invalid";},
  receipt=>{receipt.chain_seq="99";},
  receipt=>{receipt.receipt_event_hash="invalid";},
  receipt=>{receipt.receipt_thread_seq="99";},
 ];
 for(const mutate of mutations){
  const h=harness();await retained(h);mutate(h.receipt);
  const record=[...h.storage.values()][0],r=runtime({presentation:coord("A")});
  try{await assert.rejects(r.attachSeat(),/authoritative|event interval|event links|receipt hash|receipt is not adjacent/);
   assert.equal([...h.storage.values()][0],record);
   assert.equal(h.calls.some(call=>call.body.action==="claim"),false);
  }finally{r.close();}
 }
});
test("fresh claim stages server initialization outside its local producer interval",async()=>{
 const h=harness(),restored=[];
 const r=runtime({replaySeatEvents:events=>restored.push(...events)});
 try{await r.attachSeat();assert.equal(restored.length,2);assert.equal(h.encoded.length,0);
  assert.equal(h.historyCalls[0].chain_root_id,"chain-A");
 }finally{r.close();}
});
test("nonempty provisional constructor output refuses before creating authority",async()=>{
 const h=harness(),events=[{seq:17n,event_type:"seat.facet",payload:{draft:"provisional"}}];
 const r=runtime({seatEvents:()=>events});try{await assert.rejects(r.attachSeat(),/provisional seat events/);
 assert.equal(h.calls.length,0);assert.equal(events[0].seq,17n);
 }finally{r.close();}
});
test("malformed missing receipt blocks replacement and preserves recovery",async()=>{
 const h=harness();await retained(h);h.notFound=true;
 const post=h.post.bind(h);h.post=(url,body)=>url.endsWith("/replay")&&body.receipt
  ?{receipt_status:"not_found",receipt:{forged:true}}:post(url,body);
 const r=runtime({presentation:coord("A"),claim:{kind:"takeover",expected_producer_incarnation:"a".repeat(64),predecessor:controlPredecessor}});
 try{await assert.rejects(r.attachSeat(),/receipt is not confirmed/);
 assert.equal(h.calls.filter(c=>c.body.action==="claim").length,0);assert.equal(h.storage.size,1);
 assert.equal(h.encoded.length,0);}finally{r.close();}
});
test("receipt lookup with extra fields cannot clear or resend retained recovery",async()=>{
 for(const status of ["found","not_found"]){
  const h=harness();await retained(h);const record=[...h.storage.values()][0];
  const post=h.post.bind(h);h.post=(url,body)=>url.endsWith("/replay")&&body.receipt
   ?{receipt_status:status,receipt:status==="found"?h.receipt:null,unexpected:true}:post(url,body);
  const r=runtime({presentation:coord("A")});
  try{await assert.rejects(r.attachSeat(),/unsupported response/);
   assert.equal([...h.storage.values()][0],record);
   assert.equal(h.encoded.length,0);
   assert.equal(h.calls.filter(c=>c.body.action==="claim").length,0);
  }finally{r.close();}
 }
});
test("sibling presentation never consumes another pending record",async()=>{
 const h=harness();await retained(h);const r=runtime({presentation:coord("B")});
 try{await r.attachSeat();assert.equal(h.storage.size,1);assert.equal(h.calls.filter(c=>c.body.receipt).length,0);}
 finally{r.close();}
});
test("invalidation during relinquishment observation cannot restore writer permission",async()=>{
 for(const rejectObservation of [true,false]){
  const h=harness(),events=[],unavailable=[];
  let controller,release,observing;
  const observed=new Promise(resolve=>{observing=resolve;});
  const emit=(event,data)=>controller.enqueue(new TextEncoder().encode(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`));
  globalThis.fetch=async()=>new Response(new ReadableStream({start(owner){
   controller=owner;const c=coord("A"),a="d".repeat(64),b="c".repeat(64);
   emit("ui_seat.replay.begin.v1",{schema:1,...c});
   emit("ui_seat.replay.events.v1",{schema:1,events:[{event_hash:a,chain_root_id:c.chain_root_id,
    thread_id:c.thread_id,thread_seq:1,prev_thread_event_hash:"0".repeat(64),chain_seq:1,
    event_type:"ui_seat.presentation_initialized.v1",payload:{}},{event_hash:b,chain_root_id:c.chain_root_id,
    thread_id:c.thread_id,thread_seq:2,prev_thread_event_hash:a,chain_seq:2,
    event_type:"ui_seat.producer.v3",payload:{schema_version:"ryeos.ui.seat.producer.v3",
    operation_id:h.lastClaimResult.operation_id,producer_incarnation:h.lastClaimResult.producer_incarnation,
    next_engine_seq:h.lastClaimResult.next_engine_seq,state:"active",request:h.lastClaimRequest}}]});
   emit("ui_seat.replay.complete.v1",{schema:1,tip_hash:b,thread_seq:2,event_count:2});
  }}),{headers:{"Content-Type":"text/event-stream"}});
  const r=runtime({seatEvents:()=>events,onUnavailable:failure=>unavailable.push(failure)},events);
  try{
   await r.attachSeat();r.completeEngineBootstrap();assert.equal(r.canAcceptMutations(),true);
   const post=h.post.bind(h);
   h.post=(url,body)=>body.action==="observe" ? new Promise((resolve,reject)=>{
    release=()=>rejectObservation?reject(new Error("observation failed")):resolve(post(url,body));observing();
   }) : post(url,body);
   const pending=r.relinquish();await observed;
   if(!rejectObservation)events.push({seq:9n,event_type:"seat.facet",payload:{draft:"during observation"}});
   emit("ui_seat.replay.invalidated.v1",{schema:1});await idle();release();
   await assert.rejects(pending,rejectObservation?/observation failed/:/invalidated/);
   assert.equal(r.canAcceptMutations(),false);
   assert.equal(r.currentProducer(),null);
   assert.equal(h.calls.filter(call=>call.url.endsWith("/close")).length,0);
   assert.equal(unavailable.length,1);
  }finally{r.close();}
 }
});
test("mechanical close never relinquishes; explicit relinquishment is acknowledged",async()=>{
 const h=harness(),a=runtime(),b=runtime();await a.attachSeat();a.close();
 assert.equal(h.calls.filter(c=>c.url.endsWith("/close")).length,0);
 await b.attachSeat();await b.relinquish();const close=h.calls.find(c=>c.url.endsWith("/close")).body;
 assert.equal(close.action,"relinquish");assert.equal(close.presentation_id,"B");
 assert.equal(close.producer_incarnation,"a".repeat(64));assert.equal(b.currentProducer(),null);
});
test("late heartbeat refusal from a disposed producer cannot publish failure",async()=>{
 const h=harness(),interval=globalThis.setInterval;let heartbeat,rejectTouch;
 globalThis.setInterval=callback=>{heartbeat=callback;return 0;};
 const post=h.post.bind(h);h.post=(url,body)=>url.endsWith("/touch")
  ?new Promise((_resolve,reject)=>{rejectTouch=reject;}):post(url,body);
 const committed=[];const r=runtime({commitEvent:event=>committed.push(event)});
 try{await r.attachSeat();heartbeat();r.close();const count=committed.length;
  rejectTouch(new Error("late heartbeat"));await idle();assert.equal(committed.length,count);
 }finally{r.close();globalThis.setInterval=interval;}
});
test("touch renewal requires exact coordinates, touched true, and no extra fields",()=>{
 const exact={presentation_id:"A",thread_id:"thread-A",touched:true};
 assert.equal(classifySeatTouchResponse(exact,"A","thread-A"),"renewed");
 assert.equal(classifySeatTouchResponse({result:exact},"A","thread-A"),"renewed");
 for(const response of [{...exact,touched:false},{...exact,thread_id:"thread-B"},
  {...exact,presentation_id:"B"},{...exact,extra:1},{result:{...exact,extra:1}},null]){
  assert.equal(classifySeatTouchResponse(response,"A","thread-A"),"retry");
 }
 for(const status of [403,404,409,410])assert.equal(classifySeatTouchError(new HttpResponseError(status)),"refused");
 for(const status of [400,429,500])assert.equal(classifySeatTouchError(new HttpResponseError(status)),"retry");
 assert.equal(classifySeatTouchError(new Error("offline")),"retry");
});
test("definitive touch refusal fences only the current producer and overlapping ticks send once",async()=>{
 const h=harness(),interval=globalThis.setInterval;let heartbeat,rejectTouch,touches=0;
 globalThis.setInterval=callback=>{heartbeat=callback;return 0;};
 const post=h.post.bind(h);h.post=(url,body)=>url.endsWith("/touch")
  ?(touches++,new Promise((_resolve,reject)=>{rejectTouch=reject;})):post(url,body);
 const unavailable=[];const r=runtime({onUnavailable:value=>unavailable.push(value)});
 try{await r.attachSeat();heartbeat();heartbeat();assert.equal(touches,1);
  rejectTouch(new HttpResponseError(404));await idle();
  assert.equal(r.currentProducer(),null);assert.equal(r.canAcceptMutations(),false);
  assert.equal(unavailable.length,1);assert.equal(unavailable[0].kind,"writer_refused");
  heartbeat();assert.equal(touches,1);
 }finally{r.close();globalThis.setInterval=interval;}
});
test("blocked reconnect cannot discard an uncaptured local seat suffix",async()=>{
 const h=harness(),interval=globalThis.setInterval,events=[];let heartbeat;
 globalThis.setInterval=callback=>{heartbeat=callback;return 0;};
 const post=h.post.bind(h);h.post=(url,body)=>url.endsWith("/touch")
  ? Promise.reject(new HttpResponseError(404)) : post(url,body);
 const r=runtime({seatEvents:()=>events},events);
 try{
  await r.attachSeat();r.completeEngineBootstrap();heartbeat();await idle();
  assert.equal(r.canAcceptMutations(),false);
  events.push({seq:7n,event_type:"seat.facet",payload:{draft:"uncaptured"}});
  await assert.rejects(r.reconnect({kind:"independent"}),/uncaptured seat mutations/);
  assert.equal(h.storage.size,0);
  assert.equal(events.length,1);
  assert.equal(h.calls.filter(call=>call.body.action==="claim").length,1);
 }finally{r.close();globalThis.setInterval=interval;}
});
test("malformed success and transient touch failure retry without fencing the producer",async()=>{
 const h=harness(),interval=globalThis.setInterval;let heartbeat,touches=0;
 globalThis.setInterval=callback=>{heartbeat=callback;return 0;};
 const post=h.post.bind(h);h.post=(url,body)=>{
  if(!url.endsWith("/touch"))return post(url,body);
  touches++;
  if(touches===1)return {presentation_id:body.presentation_id,thread_id:body.thread_id,touched:true,extra:true};
  if(touches===2)throw new Error("temporary transport failure");
  return {presentation_id:body.presentation_id,thread_id:body.thread_id,touched:true};
 };
 const unavailable=[];const r=runtime({onUnavailable:value=>unavailable.push(value)});
 try{await r.attachSeat();r.completeEngineBootstrap();
  for(let expected=1;expected<=3;expected++){
   heartbeat();await idle();assert.equal(touches,expected);
   assert.ok(r.currentProducer());assert.equal(r.canAcceptMutations(),true);
  }
  assert.equal(unavailable.length,0);
 }finally{r.close();globalThis.setInterval=interval;}
});
test("close during admitted append preserves exact recovery after late success",async()=>{
 const h=harness();let deliver,started;const sent=new Promise(resolve=>{started=resolve;});
 h.append=body=>new Promise(resolve=>{deliver=()=>resolve(ack(body));started();});
 const r=runtime({},[{seq:0n,event_type:"seat.facet",payload:{draft:"text"}}]);
 await r.attachSeat();await sent;assert.equal(h.storage.size,1);r.close();deliver();await idle();
 assert.equal(h.storage.size,1);assert.equal(h.calls.filter(c=>c.url.endsWith("/close")).length,0);
});
test("bounded append batches fit route bytes and advance contiguous intervals",async()=>{
 const h=harness();const r=runtime({},[0,1].map(seq=>({seq:BigInt(seq),event_type:"seat.facet",payload:{draft:"x".repeat(40000)}})));
 try{await r.attachSeat();await until(()=>h.encoded.length>=2);
 assert.equal(h.encoded.length,2);assert.ok(h.encoded.every(body=>Buffer.byteLength(body)<=65536));
 const [first,second]=h.encoded.map(JSON.parse);assert.equal(first.first_engine_seq,"0");assert.equal(second.first_engine_seq,"1");
 assert.equal(h.identityInputs.length,2);
 for(const [index,input] of h.identityInputs.entries()){
  const body=JSON.parse(h.encoded[index]);assert.equal(input.first_engine_seq,BigInt(index));
  assert.equal(input.event_count,1n);assert.equal(input.chain_root_id,"chain-A");
  assert.equal(input.payload_digest,body.payload_digest);assert.equal(body.operation_id,derivedAppendId);
 }
 }finally{r.close();}
});
test("prepared producer position cannot be rebased by the browser batch adapter",async()=>{
 const h=harness(),committed=[];
 const r=runtime({commitEvent:event=>committed.push(event)},[{seq:901n,event_type:"seat.presentation_operation.prepared",
  payload:{schema:2,prepared_engine_seq:1n}}]);
 try{await r.attachSeat();await idle();assert.equal(h.encoded.length,0);
  assert.equal(h.identityInputs.length,0);assert.ok(committed.some(event=>event.error?.error.includes("reserved producer position")));
 }finally{r.close();}
});
test("predecessor random append identity is refused without deleting recovery evidence",async()=>{
 const h=harness();await retained(h);const key="ryeos.ui.seat.pending.v2:session:A";
 const predecessor=JSON.parse(h.storage.get(key));predecessor.schema_version="ryeos.ui.seat.pending-append.v2";
 h.storage.set(key,JSON.stringify(predecessor));const r=runtime({presentation:coord("A")});
 try{await assert.rejects(r.attachSeat(),/invalid schema/);assert.equal(h.storage.size,1);
  assert.equal(h.calls.filter(call=>call.body.action==="claim").length,0);
 }finally{r.close();}
});
test("signed append route bound controls batching instead of a browser default",async()=>{
 const h=harness();const r=runtime({maxSeatAppendBytes:2000n},[0,1].map(seq=>({seq:BigInt(seq),event_type:"seat.facet",payload:{draft:"x".repeat(1100)}})));
 try{await r.attachSeat();await until(()=>h.encoded.length>=2);
 assert.equal(h.encoded.length,2);assert.ok(h.encoded.every(body=>Buffer.byteLength(body)<=2000));
 }finally{r.close();}
});
test("missing or unsafe signed append bounds refuse runtime construction",()=>{
 harness();for(const maxSeatAppendBytes of [undefined,0n,-1n,9007199254740992n,65536]){
  assert.throws(()=>runtime({maxSeatAppendBytes}),/signed seat append byte bound is invalid/);
 }
});
test("a predecessor adapter without the shared identity bridge is refused",()=>{
 harness();assert.throws(()=>runtime({deriveSeatAppendIdentity:undefined}),/shared Rust seat append identity bridge/);
 assert.throws(()=>runtime({deriveSeatControlIdentity:undefined}),/shared Rust seat control identity bridge/);
});
test("unknown relinquishment retries one immutable predecessor nonce and control ID",async()=>{
 const h=harness(),r=runtime();const timeout=globalThis.setTimeout;
 globalThis.setTimeout=callback=>{queueMicrotask(callback);return 0;};
 try{await r.attachSeat();h.controlLost=5;await assert.rejects(r.relinquish(),/control delivery unknown/);
  const first=h.controls.filter(body=>JSON.parse(body).action==="relinquish");assert.equal(first.length,5);
  const observations=h.calls.filter(call=>call.body.action==="observe").length;
  await r.relinquish();const retry=h.controls.filter(body=>JSON.parse(body).action==="relinquish");
  assert.equal(retry.length,6);assert.ok(retry.every(body=>body===first[0]));
  assert.equal(h.calls.filter(call=>call.body.action==="observe").length,observations);
  assert.equal(h.controlIdentityInputs.filter(request=>request.action==="relinquish").length,1);
  assert.deepEqual(JSON.parse(first[0]).predecessor,controlPredecessor);
 }finally{r.close();globalThis.setTimeout=timeout;}
});
test("disposal during hashing cannot retain or initiate append",async()=>{
 const h=harness();let release,started;const hashing=new Promise(resolve=>{started=resolve;});
 Object.defineProperty(globalThis,"crypto",{configurable:true,value:{randomUUID:()=>webcrypto.randomUUID(),subtle:{
 digest:()=>new Promise(resolve=>{release=()=>resolve(new ArrayBuffer(32));started();})}}});
 const r=runtime({},[{seq:0n,event_type:"seat.facet",payload:{draft:"text"}}]);
 try{await r.attachSeat();await hashing;r.close();release();await idle();assert.equal(h.encoded.length,0);assert.equal(h.storage.size,0);}
 finally{r.close();Object.defineProperty(globalThis,"crypto",{configurable:true,value:trackedCrypto});}
});
