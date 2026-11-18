<!-- ryeos:signed:2026-11-18T01:18:54Z:07d9345c098787cfb6e041bf167bb130b234df38ecefe654e2abe8b10ca2411c:4gQ4N7W6ll/7xguzSmJImXxqYPMqptzNrkJ6VXYjLlPKYLYf4eyIZjcTZCj+fsqELPyUnPqeGa6TLNtfiHMMCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/ryeos-ui"
name: "timeline-v1"
title: "RyeOS Timeline Contract v1"
description: "The timeline response: entries, ids, paging and upserts, the status lane, and what a client does with them; versioned apart from the frame contract"
entry_type: reference
version: "1.0.0"
```

# RyeOS Timeline Contract v1

Contract version: `ryeos.timeline.v1`

The timeline is a chain's durable events folded into drawable items by a signed
definition. This document is the contract between the service that folds and
every client that draws: what the response holds, how items are identified,
how a client keeps a window current, and what a client does with what it does
not recognise. The frame contract (`frame-v2`) references this document for
the `timeline` widget and does not restate it, so the timeline versions on its
own.

The response carries its version as `timeline_contract`. A client accepts
exactly the version it implements. A response that names another version, or
none, is refused: the client shows a failed source with the reason and draws
nothing from it. No client reads an older shape.

The version changes when a field of the response or of an item is added,
removed or changes meaning, or when an id rule or an upsert rule changes.
Adding an entry type changes the entry-type table below and is not a version
change, because a client draws an unrecognised type by its name (see
"Unknown entry types"). A test fails when the version or the tables in this
document differ from the code.

## Request

The fold is served under two authorisations that return the same response:

- `service:threads/timeline`: the caller owns the chain's root thread.
- `service:ui/ryeos-ui/timeline/thread`: the caller is a seat session that
  owns the chain's root thread.

A chain the caller does not own, and a thread that is not a chain root, are
indistinguishable from a missing chain (`NotFound`).

Parameters (unknown parameters are refused):

| Parameter | Meaning |
|---|---|
| `chain_root_id` | required, non-empty after trimming |
| `limit` | top-level items wanted: an integer or numeric string from 1 to 2000; default 200 |
| `definition` | the definition ref, an unsuffixed `config:` ref; default `config:ryeos/ui/timeline/thread` |
| `since_chain_seq` | a non-negative integer or numeric string; asks for an upsert (see "Sequence and upserts") |

## Response fields

One JSON object. `Present` says when a field is there: `always`, or `upsert`
(only when the request carried `since_chain_seq`).

| Field | Present | Meaning |
|---|---|---|
| `timeline_contract` | always | the contract version string, `ryeos.timeline.v1` |
| `chain_root_id` | always | the chain the response describes |
| `definition_ref` | always | the definition ref the fold used |
| `definition_digest` | always | sha256 hex of the verified definition content, serialized as JSON |
| `highest_chain_seq` | always | the highest `chain_seq` folded; 0 for a chain with no events |
| `older` | always | number of top-level items before the window |
| `next_limit` | always | the `limit` that would reveal more, or null |
| `summary` | always | the chain header: status and chain-wide usage |
| `undeclared` | always | event types the definition has no rule for, with counts |
| `notice_noun` | always | the noun a client counts undeclared events under, or null |
| `lane` | always | what the thread is doing now, or null |
| `items` | always | the window's items, or the changed ones in an upsert |
| `order` | upsert | the ids of every item in the window, in order |

`summary` is `{status, input_tokens, output_tokens, spend_usd, turns}`:
`status` is the root thread's current status; the rest are chain-wide totals
(`turns` counts completed turns). It is computed on every request and never
cached with the fold, so the status is current.

## Items

`items` holds items, each a JSON object whose `item` field names its shape:
`entry`, `cluster` or `recap`. Fields that can be absent are present as null.

### Entry item

An entry is one drawable unit.

| Field | Meaning |
|---|---|
| `item` | `entry` |
| `id` | the entry's id (see "Ids") |
| `type` | the entry type (see "Entry types") |
| `state` | the entry's latest state, or null; an open set the definition names |
| `tone` | `neutral`, `accent`, `good`, `warn` or `danger`, or null |
| `thread` | the thread the entry's event came from |
| `parent` | the id of the entry this one nests under, or null |
| `exchange` | the id of the entry that opened the exchange this entry belongs to, or null before the first exchange |
| `counts_as` | the vocabulary noun key the entry counts under in a cluster headline, or null |
| `slots` | an object of named text, name to string; the names are the definition's |
| `seq` | `[first, last]` `chain_seq` of the events behind the entry |
| `ts` | `[first, last]` timestamps of those events |

### Cluster item

A cluster is a run of finished operations, or of notices, shown as one group.

| Field | Meaning |
|---|---|
| `item` | `cluster` |
| `id` | the cluster's id |
| `headline` | the counted nouns, for example `3 pages · 1 operation` |
| `members` | the member entries, each a bare entry (the fields above without `item`) |

### Recap item

A recap is the work of a finished exchange folded under one line.

| Field | Meaning |
|---|---|
| `item` | `recap` |
| `id` | the recap's id |
| `exchange` | the id of the exchange the recap belongs to, or an empty string |
| `work` | the number of operations among the members, not events |
| `duration_s` | whole seconds from the earliest to the latest member timestamp, or null |
| `members` | the folded work: items (`entry` and `cluster`, each carrying `item`) in order |
| `seq` | the highest `chain_seq` among the members and the entries that finished the exchange |

## Entry types

The set is closed: a rule of the definition can create exactly these.
`cluster` and `recap` are produced by passes over entries, never by a rule.

| Type | Id prefix | Meaning |
|---|---|---|
| `message` | `msg` | input the model is given by an operator or launch; may open an exchange |
| `prose` | `pro` | model text; adjacent flow emissions of one thread merge into one entry |
| `thought` | `tho` | model reasoning |
| `activity` | `act` | a unit of work with a lifecycle (a tool call, a graph step) |
| `link` | `lnk` | work in another thread, with the child's state when it has settled |
| `marker` | `mrk` | a boundary or system event; notices are markers |
| `outcome` | `out` | how a thread ended |
| `ask` | `ask` | an approval or question: pending, then answered or expired |

What a type means beyond this table, its states, its tones and which slots
it fills are the definition's: it is signed data, not part of this contract.
A client interprets only what this document names.

## Group items

| Item | Id prefix | Id |
|---|---|---|
| `cluster` | `cl` | `cl:` followed by the id of its first member |
| `recap` | `rc` | `rc:` followed by the id of the first entry it folds |

A cluster is two or more consecutive finished activities (with a `counts_as`
noun and a state the definition lists as a group state), or two or more
consecutive markers (counted under the definition's notice noun, when it names
one, and not the marker that opens an exchange), within one exchange and one
parent, none of which escapes. An entry escapes, and never
hides in a cluster or a recap, when its state or tone is listed by the
definition. A lone qualifying entry stays an entry. The headline counts nouns
in the definition's vocabulary order (singular for one, plural otherwise),
then unknown nouns by key, joined by ` · `.

A recap folds the work of a finished exchange. An exchange is finished when
it has an answer (a `prose` entry) and something ended it after the answer: a
later exchange began, or its own `outcome` followed the answer. An exchange
still receiving events, or one with no answer, is not folded. Each unbroken
stretch of work that precedes the exchange's last answer becomes one recap;
visible entries between stretches stay where they were, so an exchange can
hold several recaps. Work is an activity, link or thought, or a cluster of
activities, that does not escape. A stretch whose time range cannot be
measured is left unfolded.

## Ids

- A keyed entry (the rule names key paths) has the id `<prefix>:<k1>/<k2>/...`,
  each key read from the event and rendered as text. Every event that yields
  the same id updates the same entry: slots merge, a state or tone that is
  present replaces the earlier one, and `seq[1]` and `ts[1]` advance.
- An entry with no key, or whose key has a missing part, has the id
  `<prefix>:<chain_seq>` of its event; when the rule emits once per element of
  an array and has no key, `<prefix>:<chain_seq>/<element index>`.
- A merged flow entry keeps the id of the first event that formed it.
- Ids derive only from durable event content and the definition, so folding
  the same events with the same definition gives the same ids, and an entry's
  id does not change when later events are appended.
- Cluster and recap ids derive from a member, so a group is identified by
  where it starts. Group membership is not stable: an entry that was a
  top-level item can join a cluster, and a member that fails leaves its
  group. The entry keeps its id throughout; the group items come and go.
- When an event adds a spine edge (threads linked into the viewed thread's
  story) that puts earlier events on the spine, earlier entries are
  re-derived. Ids stay deterministic, and the next upsert resends the whole
  window (see below).

## Sequence and upserts

`seq` and `ts` of an entry are the first and last events behind it. The
engine derives an item's last sequence: an entry's `seq[1]`, a cluster's
highest member `seq[1]`, a recap's `seq`. An item whose content changed has a
larger last sequence than it had before the change. A recap's `seq` includes
the entries that finished its exchange, so it reads as changed from the
moment it forms; an entry changes when its presentation subject arrives.

A request without `since_chain_seq` returns the whole window: `items` is the
window in order and there is no `order`.

A request with `since_chain_seq` returns an upsert for a client that holds
the window as of that sequence:

- `order` lists the ids of every item in the window, in order;
- `items` holds every window item whose last sequence is greater than the
  given value, and all of them when earlier entries were re-derived after it.

A client builds its window from `order`: each id takes the item sent in
`items`, or else the item it already holds. An item it holds that is not in
`order` is dropped (it slid out, or was absorbed into a group). When an id in
`order` is in neither `items` nor the held window, or the held window was
folded by a different `definition_digest`, the client cannot be brought
current from this update and refetches the whole window without
`since_chain_seq`. The shipped client sends `highest_chain_seq` of the window
it holds as `since_chain_seq`, and also refetches whole after a change of
`limit`.

## Paging

The window is a suffix of the chain's top-level items (entries, clusters and
recaps, after the group passes): the last `limit` of them. `older` counts the
items before it. An item is never split, so a cluster or recap is wholly in or
out of the window.

`next_limit` is `limit` widened by 200 and capped at 2000, and null when
nothing is older than the window or `limit` is already as wide as it may be.
A client pages backward by requesting the whole window again with that
limit; there is no cursor and no way to request a range of older items. A
client shows `older` and, where `next_limit` is present, offers to widen.

## Fold state keyed by id

A client keeps the state the operator chose (which groups are open, which
entries are expanded, where the point is) by id, so it survives upserts and
the arrival of events:

- a cluster and a recap are folded until the operator opens them; their fold
  key is the item id;
- an exchange's entries start open; its fold key is the exchange id;
- the notice that counts undeclared events starts folded; its fold key is
  `notice:system_events`;
- an entry's expansion and its detail source are keyed by its item id;
- a group item without an id folds by position.

The operator's departures from the defaults are stored by key, so a fold
choice made on `cl:act:7` still applies when an upsert resends that cluster.

## Lane, summary, notice noun and undeclared

`lane` is what the thread is doing now: `{label, entry}` or null when idle.
The definition's lane table lists rows in priority order; the first row that
matches wins. An entry row (`type`, `state`, `label`) matches the most recent
entry of that type whose latest state is that state, and `entry` is that
entry's id (it can lie outside the window). A status row (`status`, `label`)
matches when the chain's status equals it, and `entry` is null. A chain whose
root status is terminal other than `continued` (`completed`, `failed`,
`cancelled`, `killed`, `timed_out`) has no lane, whatever its entries say.

`undeclared` maps each event type that no rule of the definition handles to
how many such events the chain holds, over the whole chain, not the window. An
event type with a rule that deliberately drops it is declared and not counted.
`notice_noun` is the vocabulary noun `{key, one, many}` the definition's
notice setting names, or null. A client draws one folded notice, headed by
the total count and the singular or plural noun, only when there is at least
one undeclared event and a noun; it never supplies the wording itself.

## Definition ref and digest

The definition is a signed config item. `definition_ref` is the ref the fold
used. `definition_digest` is the sha256 hex of the verified item content
serialized as JSON, so a changed definition has a different digest. The
service keeps one resumable fold per chain and digest, and a changed
definition never reuses a fold made with another. A client treats a change of
`definition_digest` as a change of everything it holds and refetches the
window whole.

## Errors and states

The service refuses with these kinds: `BadRequest` (an unknown parameter, an
empty `chain_root_id`, a `limit` or `since_chain_seq` out of range or
malformed, an invalid definition ref, a ref that is not an unsuffixed
`config:` ref), `NotFound` (see "Request"), `Conflict` (the definition is not
installed), and `Internal` (the definition is untrusted or invalid, events
that cannot be ordered, a failure reading usage totals). There is no partial
or coverage field: a response is the fold of every event up to
`highest_chain_seq`.

A client draws the timeline's own state in the feed's place:

| State | When |
|---|---|
| `loading` | no response yet; the source is waiting |
| `unresolved` | a required parameter, such as the chain, is not resolved |
| `failed` | the fetch failed, or the response was refused; the reason is shown and the fetch can be retried; entries from an earlier response stay drawn, marked stale |
| `refreshing` | a response is held and a newer fetch is in flight |
| `empty` | a response is held and the window has no entries |

A response that names another `timeline_contract`, or none, is a `failed`
source with the code `timeline_contract_mismatch`; the window held before it
is dropped, and the next fetch retries.

## Unknown entry types

A client draws every type this document names. An entry whose `type` it does
not recognise is drawn as a single line whose text is the type name (or
`item` when the type is empty), followed by the entry's `headline` and
`detail` slots when present, with its tone and state: its label, not nothing
and not an error. An item whose `item` field is none of `entry`, `cluster`
and `recap` is skipped.

## How the shipped client draws entries

By type, from the slots an entry carries (a missing slot falls back as
shown; text is the first line, cut at 120 characters unless stated):

| Type | Drawn as |
|---|---|
| `message` | a separator: the `body`, else `headline`, else `message`, cut at 80 characters |
| `prose` | a block of the `body`, else `headline` |
| `thought` | a neutral block: `✻` and the `body`, else `headline` |
| `activity` | a step: the `headline`, else `tool`, else `activity`, with `tool` (only when a headline is shown), `detail` and `badge` beside it; pending while the state is `running` or `retrying` |
| `link` | a line `↘` and the `headline`, else `child`, with `target` beside it; activating it drills into the thread named by `target` |
| `outcome` | a line: the state, else `headline`, else `outcome`; a `reason` slot follows it and activating the line inspects the entry |
| `marker` | a line: `headline`, else `detail`, else `body`, else `marker` |
| `ask` | a line: `headline`, else `body`, else `detail`, else `ask`; accent unless the entry has a tone |

An entry with a `retry_input` slot also offers that input back as a
pre-filled continuation of its own thread. The state is drawn in plain words
(underscores as spaces) and is part of the accessible label, so a label reads
`fetch, failed: 404`. A tone name outside the five is neutral; an entry with
no tone reads accent while `running`, warn while `retrying`, else neutral.
Nesting depth follows `parent`, and a cluster's or recap's members sit one
level below it. A text copy, a screen reader and a terminal all draw the same
label.

The window opens with the chain header from `summary` (status, tokens in and
out, cost, turns) and, when `older` is above zero, a line counting the older
items.

## Bounds and sanitising

- `limit` is at most 2000 and defaults to 200; `next_limit` never exceeds
  2000.
- The fold reads the chain's events 500 at a time, and keeps its fold in a
  cache of 64 chains.
- A presentation subject (the `presentation` block a tool declares) is read
  from the capsule pinned when the call was admitted, kept only when it is a
  mapping of at most 8 KiB serialized, and cached by capsule hash.
- Text a subject fills into a slot is cleaned once, in the engine: control
  characters other than newline and tab and bidirectional controls are
  removed, the text is trimmed, and it is cut at the cap the definition gives
  that slot, with an elision mark. The engine does not clean or cap any other
  slot text: it is the event's text as the durable log holds it.
- All slot text is untrusted. A client draws it as inert text: it never
  interprets escape sequences or markup in it. The terminal client replaces
  control characters before printing.
- Clients draw summary lines from the first line of a slot, cut at 120
  characters (a message separator at 80).

## Raw events

The events behind an item are not embedded. An item's `seq` names the
inclusive range; the raw-events service
(`service:ui/ryeos-ui/timeline/raw`, parameters `chain_root_id`,
`from_chain_seq`, `to_chain_seq`) returns at most 200 events of the range, in
order, with `truncated` set when the range held more. It is a separate
response and does not carry `timeline_contract`.
