import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const source = await readFile(new URL("../browser/runtime/seat-history-stream.ts", import.meta.url), "utf8");
const session = await readFile(new URL("../browser/runtime/session.ts", import.meta.url), "utf8");
const boot = await readFile(new URL("../browser/runtime/boot.ts", import.meta.url), "utf8");

test("same-producer reproof settles original appends and publishes only after readiness", () => {
  const refresh = session.slice(session.indexOf("async function reproveCurrentSeat"), session.indexOf("function appendMatches"));
  assert.match(refresh, /if \(seatSyncPromise\) await seatSyncPromise/);
  assert.match(refresh, /await reconcileRetainedAppend\(\)/);
  assert.match(refresh, /consumeSeatHistory\(coordinate, claim,/);
  assert.doesNotMatch(refresh, /invokeSeat\("open"|clearRetainedAppend\(/);
  assert.match(refresh, /seatObserved = promotedCore\.acknowledgedEventCount/);
  assert.ok(refresh.indexOf("seatObserved =") < refresh.indexOf("promotedCore.publish()"));
  assert.ok(refresh.indexOf("seatReady = true") < refresh.indexOf("promotedCore.publish()"));
  const promotion = boot.slice(boot.indexOf("async completeSeatReproof"), boot.indexOf("replaySeatEvents:", boot.indexOf("async completeSeatReproof")));
  assert.match(promotion, /await reproofSession\(presentation\)/);
  assert.ok(promotion.indexOf("requireOwner();") < promotion.indexOf("ryeos_commit_seat_reproof"));
  assert.match(promotion, /ryeos_commit_seat_reproof\(completion\)/);
  assert.match(promotion, /acknowledgedPrefix = wasm\.ryeos_acknowledged_seat_prefix_len\(\)/);
  assert.match(promotion, /acknowledgedPrefix > BigInt\(Number\.MAX_SAFE_INTEGER\)/);
  assert.match(promotion, /acknowledgedEventCount = Number\(acknowledgedPrefix\)/);
  assert.match(promotion, /publish: \(\) =>/);
  assert.doesNotMatch(promotion, /mountRyeOsRenderer|createCommitRuntime|ryeos_start/);
});

test("document settlement joins refresh work before successor attachment", () => {
  assert.match(session, /await Promise\.allSettled\(\[attaching, reproofing, seatSyncPromise, relinquishing\]\)/);
  assert.match(boot, /closingSession\?\.settle\(\)/);
});

test("obsolete reproof failure cannot fence or abort a replacement owner", () => {
  const owner = session.slice(session.indexOf("function receiveHistoryFailure"), session.indexOf("function appendMatches"));
  assert.match(owner, /!closed && taskEpoch === seatEpoch && taskProducer === seatProducer/);
  assert.match(owner, /if \(epoch === seatEpoch && producer === seatProducer\) options\.abortSeatReplay\(\)/);
});

test("append reconciliation can await the exact in-flight sync without restarting it", () => {
  const sync = session.slice(session.indexOf("async function syncSeat():"), session.indexOf("async function syncSeatOnce():"));
  assert.match(sync, /if \(seatSyncPromise\) return seatSyncPromise/);
  assert.match(sync, /await pending/);
  assert.match(sync, /if \(seatSyncPromise === pending\) seatSyncPromise = null/);
  assert.match(sync, /nextEngineSeq > frontier && pendingSeatEvents\.length > 0/);
  assert.ok(sync.indexOf("seatSyncPromise = null") < sync.indexOf("void syncSeat()"));
});

test("original claim witness stays separate from the append frontier", () => {
  assert.match(session, /acknowledgedClaim = Object\.freeze\(\{\s*operation_id: operationId,\s*producer_incarnation: producer,\s*next_engine_seq: engineFrontier/);
  assert.match(session, /consumeSeatHistory\(presentation, acknowledgedClaim,/);
  assert.match(session, /nextEngineSeq = expected\.lastEngineSeq \+ 1n/);
  assert.doesNotMatch(session, /acknowledgedClaim\.next_engine_seq\s*=/);
});

test("reproof wake is closed and post-completion only, never a provisional proof", () => {
  const marker = 'frame.event === "ui_seat.replay.reproof_required.v1"';
  const index = source.indexOf(marker);
  assert.ok(index > source.indexOf("if (settled)"));
  assert.ok(index < source.indexOf('frame.event === "ui_seat.replay.begin.v1"'));
  assert.equal(source.indexOf(marker, index + marker.length), -1);
  const branch = source.slice(index, source.indexOf('frame.event === "ui_seat.replay.invalidated.v1"', index));
  assert.match(branch, /frame\.data\.schema === 1n/);
  assert.match(branch, /Object\.keys\(frame\.data\)\.length === 1/);
  assert.match(branch, /fail\(new SeatHistoryReproofRequiredError/);
  assert.doesNotMatch(branch, /stageTransferProof\(|stage\(|resolve\(/);
});

test("history callback runs only after the old reader has been stopped", () => {
  const fail = source.slice(source.indexOf("const fail ="), source.indexOf("const onAbort ="));
  assert.ok(fail.indexOf("stop();") < fail.indexOf("onInvalidated(error)"));
});
