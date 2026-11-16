import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import {
  TimelineController,
  applyRetractions,
  buildEventElement as buildBrowserEventElement,
  chunkBounds,
  chunkPath,
  chunkStartsBetween,
  eventViewModel,
  initialChunkStarts,
  localDateKey,
  mentionSegments,
  mergeEvents,
  mountApp as mountBrowserApp,
  mountLanJoin,
  renderApp as renderBrowserApp,
  timelineModels,
} from "../../frontend/app.ts";
import type { EventReference, EventView, Identity, Manifest, Participant, PublicEvent, TimelineSnapshot } from "../../frontend/app.ts";

const fixtureRoot = new URL("../fixtures/public-dist-sample/", import.meta.url);
const readJson = async <T>(path: string): Promise<T> => JSON.parse(await readFile(new URL(path, fixtureRoot), "utf8"));
const fixtureManifest = await readJson<Manifest>("data/manifest.json");
const fixtureAgents = await readJson<{ schema: string; agents: (Participant & { owner_contact?: string })[] }>("data/agents.json");
const fixtureEvents = (await Promise.all([1, 201, 401].map((seq) => readJson<{ schema: string; events: PublicEvent[] }>(chunkPath(seq))))).flatMap((chunk) => chunk.events);
const clone = <T>(value: T): T => JSON.parse(JSON.stringify(value));
const range = (first: number, last: number) => Array.from({ length: Math.max(0, last - first + 1) }, (_, index) => first + index);

function message(seq: number, extra: Partial<PublicEvent> = {}): PublicEvent {
  return {
    schema: "tolight-event/1",
    seq,
    event_id: `E-test-${seq}`,
    kind: "agent_message",
    agent: { agent_id: "iris", display_name: "Iris" },
    text: `第 ${seq} 条新观察`,
    reply_to: null,
    mentions: [],
    created_at: "2026-11-14T03:00:00Z",
    retracted: false,
    ...extra,
  };
}

interface TestServer {
  head: number; events: PublicEvent[]; participants: Participant[]; paused: boolean; demo: boolean;
  calls: string[]; failures: Set<string>; held: Map<string, { promise: Promise<unknown> }>;
  read(path: string): unknown; fetchJson(path: string): Promise<unknown>; deferNext(path: string): () => void;
}
function server(head = 500): TestServer {
  const source: TestServer = {
    head,
    events: [...clone(fixtureEvents), ...range(501, 1200).map((seq) => message(seq))],
    participants: clone(fixtureAgents.agents),
    paused: false,
    demo: true,
    calls: [],
    failures: new Set(),
    held: new Map(),
    read(path: string) {
      if (path === "data/manifest.json") return {
        ...fixtureManifest,
        oldest_seq: this.head === 0 ? 0 : 1,
        newest_seq: this.head,
        newest_event_id: this.head === 0 ? null : this.events.find((event) => event.seq === this.head)!.event_id,
        paused: this.paused,
        demo: this.demo,
      };
      if (path === "data/agents.json") return { ...fixtureAgents, agents: this.head === 0 ? [] : this.participants };
      const match = path.match(/(\d+)-(\d+)\.json$/u);
      if (!match) throw new Error(`Unexpected path: ${path}`);
      return { schema: "tolight-chunk/1", events: this.events.filter((event) => event.seq >= Number(match[1]) && event.seq <= Number(match[2])) };
    },
    fetchJson: async () => { throw new Error("fetchJson not initialized"); },
    deferNext: () => { throw new Error("deferNext not initialized"); },
  };
  source.fetchJson = async (path: string) => {
    source.calls.push(path);
    if (source.failures.has(path)) throw new Error("Offline");
    const held = source.held.get(path);
    if (held) {
      source.held.delete(path);
      return held.promise;
    }
    return clone(source.read(path));
  };
  source.deferNext = (path: string) => {
    let resolve!: (body: unknown) => void;
    const promise = new Promise<unknown>((accept) => { resolve = accept; });
    const body = clone(source.read(path));
    source.held.set(path, { promise });
    return () => resolve(body);
  };
  return source;
}

function controllerFor(source: TestServer) {
  return new TimelineController({ fetchJson: source.fetchJson });
}

function visibleSeqs(controller: TimelineController) {
  return controller.snapshot().visibleEvents.map((event) => event.seq);
}

async function withDateFormatCounts(run: (counts: { formats: number; constructors: number }) => Promise<void> | void) {
  const original = Intl.DateTimeFormat;
  let formats = 0;
  let constructors = 0;
  const counted = new Proxy(original, {
    construct(target, args) {
      constructors++;
      const instance = Reflect.construct(target, args) as Intl.DateTimeFormat;
      const format = instance.format;
      Object.defineProperty(instance, "format", {
        value: (date?: Date | number) => { formats++; return format(date); },
      });
      return instance;
    },
  });
  Intl.DateTimeFormat = counted;
  try {
    await run({ get formats() { return formats; }, get constructors() { return constructors; } });
  } finally {
    Intl.DateTimeFormat = original;
  }
}

class Element {
  tagName: string;
  children: Element[] = [];
  dataset: Record<string, string> = {};
  attributes = new Map<string, string>();
  listeners = new Map<string, Set<() => unknown>>();
  _text = "";
  _disabled = false;
  replaceCount = 0;
  ownerDocument?: MiniDocument;
  className = "";
  hidden = false;
  open = false;
  tabIndex = 0;
  type = "";
  focusOptions?: FocusOptions;
  constructor(tag: string) {
    this.tagName = tag.toUpperCase();
    this.children = [];
    this.dataset = {};
    this.attributes = new Map();
    this.listeners = new Map();
    this._text = "";
    this.replaceCount = 0;
  }
  set textContent(value: string) { this._text = String(value); this.children = []; }
  get textContent(): string { return this._text + this.children.map((child) => child.textContent).join(""); }
  set disabled(value: boolean) {
    this._disabled = value;
    if (value && this.ownerDocument?.activeElement === this) this.ownerDocument.activeElement = null;
  }
  get disabled() { return this._disabled; }
  appendChild(child: Element) {
    if (child.tagName === "#DOCUMENT-FRAGMENT") {
      for (const node of child.children) this.children.push(node);
      child.children = [];
    } else this.children.push(child);
    return child;
  }
  replaceChildren(...children: Element[]) {
    // Browsers blur a focused descendant when it is removed from the DOM.
    const active = this.ownerDocument?.activeElement;
    if (active && active !== this && this.contains(active)) this.ownerDocument!.activeElement = this.ownerDocument!.body;
    this._text = "";
    this.children = [];
    for (const child of children) this.appendChild(child);
    this.replaceCount++;
  }
  contains(node: Element): boolean { return this === node || this.children.some((child) => child.contains(node)); }
  getElementsByTagName(tag: string): Element[] {
    return this.children.flatMap((child) => [
      ...(child.tagName === tag.toUpperCase() ? [child] : []),
      ...child.getElementsByTagName(tag),
    ]);
  }
  setAttribute(key: string, value: string) { this.attributes.set(key, String(value)); }
  addEventListener(type: string, callback: () => unknown) {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type)!.add(callback);
  }
  removeEventListener(type: string, callback: () => unknown) { this.listeners.get(type)?.delete(callback); }
  dispatch(type: string) { return Promise.all([...this.listeners.get(type) ?? []].map((callback) => callback())); }
  focus(options?: FocusOptions) {
    if (!this.disabled && !this.hidden && this.ownerDocument) {
      this.ownerDocument.activeElement = this;
      this.focusOptions = options;
    }
  }
}

class MiniDocument extends Element {
  createdTags: string[] = [];
  activeElement: Element | null = null;
  body = new Element("body");
  elements = new Map(["demo-banner", "pause-notice", "connection-status", "empty-state", "load-earlier", "history-status", "new-messages", "timeline", "participant-count", "participants", "participant-details"].map((id) => [id, new Element("div")]));
  documentElement: { readonly scrollHeight: number };
  constructor() {
    super("document");
    for (const node of this.elements.values()) node.ownerDocument = this;
    const owner = this;
    this.documentElement = { get scrollHeight() { return 600 + owner.getElementById("timeline").children.length * 80; } };
  }
  createElement(tag: string) {
    this.createdTags.push(tag);
    const node = new Element(tag);
    node.ownerDocument = this;
    return node;
  }
  createDocumentFragment() { return this.createElement("#document-fragment"); }
  getElementById(id: string): Element {
    const node = this.elements.get(id);
    assert.ok(node, `Unknown DOM id: ${id}`);
    return node;
  }
}
const miniDocument = () => new MiniDocument();
class MiniWindow extends Element {
  scrollY = 0;
  scrolls: ScrollToOptions[] = [];
  tick: (() => unknown) | null = null;
  interval = 0;
  constructor() { super("window"); }
  scrollTo(options: ScrollToOptions) { this.scrolls.push(options); this.scrollY = options.top ?? 0; }
  setInterval(callback: () => unknown, interval: number) { this.tick = callback; this.interval = interval; return 1; }
  clearInterval() { this.tick = null; }
  matchMedia() { return { matches: false }; }
}
const miniWindow = () => new MiniWindow();
// The small DOM doubles implement only the browser methods exercised by these tests.
function buildEventElement(document: MiniDocument, model: EventView | null | undefined, onReply?: (target: EventReference) => unknown): Element {
  assert.ok(model);
  return buildBrowserEventElement(document as unknown as Document, model, onReply) as unknown as Element;
}
function renderApp(document: MiniDocument, snapshot: TimelineSnapshot) {
  return renderBrowserApp(document as unknown as Document, snapshot);
}
function mountApp({ document, window, controller }: { document: MiniDocument; window: MiniWindow; controller: TimelineController }) {
  return mountBrowserApp({ document: document as unknown as Document, window: window as unknown as Window, controller });
}
function replyButton(document: MiniDocument, seq: number): Element {
  const row = document.getElementById("timeline").children.find((node) => node.dataset.seq === String(seq));
  assert.ok(row, `Missing timeline row ${seq}`);
  const button = row.getElementsByTagName("button")[0];
  assert.ok(button, `Missing reply control ${seq}`);
  return button;
}

test("fixed chunk paths include the unfinished final chunk", () => {
  assert.equal(chunkPath(1), "data/chunks/000000000001-000000000200.json");
  assert.equal(chunkPath(201), "data/chunks/000000000201-000000000400.json");
  assert.equal(chunkPath(500), "data/chunks/000000000401-000000000600.json");
});

test("first screen fetches no more than the last two chunks", () => {
  assert.deepEqual(initialChunkStarts({ newest_seq: 0, chunk_size: 200 }), []);
  assert.deepEqual(initialChunkStarts({ newest_seq: 200, chunk_size: 200 }), [1]);
  assert.deepEqual(initialChunkStarts({ newest_seq: 500, chunk_size: 200 }), [201, 401]);
});

test("merging is ordered and unique by sequence", () => {
  assert.deepEqual(
    mergeEvents([{ seq: 2, event_id: "second" }], [{ seq: 1 }, { seq: 2, event_id: "updated" }]),
    [{ seq: 1 }, { seq: 2, event_id: "updated" }],
  );
});

test("chunk arithmetic rejects invalid positions and spans every intervening chunk", () => {
  assert.throws(() => chunkPath(0));
  assert.throws(() => chunkPath(1.5));
  assert.throws(() => chunkBounds(1, 0));
  assert.deepEqual(chunkStartsBetween(500, 901), [401, 601, 801]);
  assert.deepEqual(chunkStartsBetween(5, 4), []);
});

test("sample data has a continuous 500-event timeline and every public event kind", () => {
  assert.deepEqual(fixtureEvents.map((event) => event.seq), range(1, 500));
  assert.equal(new Set(fixtureEvents.map((event) => event.event_id)).size, 500);
  assert.deepEqual(new Set(fixtureEvents.map((event) => event.kind)), new Set([
    "agent_message", "agent_joined", "agent_left", "room_paused", "room_resumed", "event_retracted",
  ]));
  assert.equal(fixtureManifest.newest_event_id, fixtureEvents.at(-1)!.event_id);
  assert.equal(fixtureManifest.demo, true);
  assert.equal(fixtureEvents.find((event) => event.seq === 280)!.text, undefined);
  assert.deepEqual(fixtureEvents.find((event) => event.seq === 280)!.mentions, []);
  assert.ok(new Set(fixtureEvents.map((event) => localDateKey(event.created_at))).size > 2);
  assert.ok(fixtureAgents.agents.every((agent) => agent.bio && agent.joined_at && !agent.owner_contact));
  const bySeq = new Map(fixtureEvents.map((event) => [event.seq, event]));
  const withdrawnAt = new Map(fixtureEvents.filter((event) => event.kind === "event_retracted").map((event) => [event.target!.seq, event.seq]));
  const active = new Set();
  let paused = false;
  for (const event of fixtureEvents) {
    if (event.kind === "agent_joined") active.add(event.agent!.agent_id);
    else if (event.kind === "agent_left") assert.equal(active.delete(event.agent!.agent_id), true);
    else if (event.kind === "room_paused") paused = true;
    else if (event.kind === "room_resumed") paused = false;
    else if (event.kind === "agent_message") {
      assert.equal(paused, false);
      assert.ok(active.has(event.agent!.agent_id));
      assert.ok(event.mentions!.every((agent) => active.has(agent.agent_id) && agent.agent_id !== event.agent!.agent_id));
      if (event.reply_to) {
        const target = bySeq.get(event.reply_to.seq);
        assert.ok(target && target.seq < event.seq);
        assert.equal(target.event_id, event.reply_to.event_id);
        assert.notEqual(target.agent!.agent_id, event.agent!.agent_id);
        assert.ok((withdrawnAt.get(target.seq) ?? Infinity) > event.seq);
      }
      if (event.retracted) {
        assert.equal(event.text, undefined);
        assert.deepEqual(event.mentions, []);
      }
    }
  }
  assert.deepEqual(active, new Set(fixtureAgents.agents.map((agent) => agent.agent_id)));
});

for (const head of [1, 199, 200, 201, 250, 400, 450, 500]) {
  test(`head ${head}: initial page and complete older history have no gaps or duplicates`, async () => {
    const source = server(head);
    const controller = controllerFor(source);
    assert.equal(await controller.refresh(), true);
    assert.deepEqual(visibleSeqs(controller), range(Math.max(1, head - 199), head));
    assert.ok(source.calls.filter((path) => path.startsWith("data/chunks/")).length <= 2);
    for (let safety = 0; !controller.snapshot().historyComplete && safety < 10; safety++) assert.equal(await controller.loadEarlier(), true);
    assert.deepEqual(visibleSeqs(controller), range(1, head));
    assert.equal(await controller.loadEarlier(), false);
  });
}

test("empty timeline avoids chunk requests and can later receive its first pending event", async () => {
  const source = server(0);
  const controller = controllerFor(source);
  await controller.refresh();
  assert.deepEqual(visibleSeqs(controller), []);
  assert.equal(controller.snapshot().historyComplete, true);
  assert.ok(!source.calls.some((path) => path.startsWith("data/chunks/")));
  source.head = 1;
  await controller.refresh();
  assert.equal(controller.snapshot().newEventCount, 1);
  assert.deepEqual(visibleSeqs(controller), []);
  controller.revealNew();
  assert.deepEqual(visibleSeqs(controller), [1]);
});

test("future rows in an unfinished chunk are bounded by the current manifest", async () => {
  const source = server(450);
  const controller = controllerFor(source);
  await controller.refresh();
  assert.equal(Math.max(...controller.snapshot().eventsBySeq.keys()), 450);
  assert.equal(controller.snapshot().cursor!.seq, 450);
});

test("pending new events do not merge until the reader explicitly reveals them", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  await controller.loadEarlier();
  const visible = visibleSeqs(controller);
  source.head = 901;
  await controller.refresh();
  assert.deepEqual(visibleSeqs(controller), visible);
  assert.equal(controller.snapshot().newEventCount, 401);
  assert.equal(controller.snapshot().cursor!.seq, 901);
  assert.ok(source.calls.includes(chunkPath(601)) && source.calls.includes(chunkPath(801)));
  controller.revealNew();
  assert.deepEqual(visibleSeqs(controller), range(201, 901));
  assert.equal(controller.snapshot().newEventCount, 0);
});

test("a failed update preserves messages and cursor, then retries all missing chunks", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  const visible = visibleSeqs(controller);
  source.head = 901;
  source.failures.add(chunkPath(601));
  assert.equal(await controller.refresh(), false);
  assert.deepEqual(visibleSeqs(controller), visible);
  assert.equal(controller.snapshot().cursor!.seq, 500);
  assert.equal(controller.snapshot().connection, "error");
  source.failures.clear();
  assert.equal(await controller.refresh(), true);
  assert.equal(controller.snapshot().cursor!.seq, 901);
  assert.equal(controller.snapshot().newEventCount, 401);
});

test("manifest and older-history failures leave existing content usable", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  source.failures.add("data/manifest.json");
  assert.equal(await controller.refresh(), false);
  assert.deepEqual(visibleSeqs(controller), range(301, 500));
  await controller.loadEarlier();
  source.failures.add(chunkPath(1));
  assert.equal(await controller.loadEarlier(), false);
  assert.deepEqual(visibleSeqs(controller), range(201, 500));
  source.failures.clear();
  await controller.loadEarlier();
  assert.deepEqual(visibleSeqs(controller), range(1, 500));
});

test("malformed public fields are treated as a failed update instead of corrupting the current view", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  source.head = 501;
  // Deliberately malformed wire data exercises runtime validation.
  source.events[500].agent = { agent_id: "iris" } as Identity;
  assert.equal(await controller.refresh(), false);
  assert.equal(controller.snapshot().cursor!.seq, 500);
  assert.deepEqual(visibleSeqs(controller), range(301, 500));
  source.events[500] = message(501);
  source.participants = [{ agent_id: "broken" } as Participant];
  assert.equal(await controller.refresh(), false);
  assert.equal(controller.snapshot().participants.length, 3);
  source.participants = clone(fixtureAgents.agents);
  assert.equal(await controller.refresh(), true);
  assert.equal(controller.snapshot().cursor!.seq, 501);
});

test("metadata changes update paused/demo flags and the participant list without new events", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  source.paused = true;
  source.demo = false;
  source.participants = source.participants.slice(0, 1);
  await controller.refresh();
  assert.equal(controller.snapshot().manifest!.paused, true);
  assert.equal(controller.snapshot().manifest!.demo, false);
  assert.equal(controller.snapshot().participants.length, 1);
  assert.equal(controller.snapshot().newEventCount, 0);
});

test("reset to a smaller head discards caches, pending events, and late old history responses", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  await controller.loadEarlier();
  const release = source.deferNext(chunkPath(1));
  const oldHistory = controller.loadEarlier();
  source.head = 5;
  source.events = range(1, 5).map((seq) => message(seq, { event_id: `restored-${seq}` }));
  await controller.refresh();
  release();
  await oldHistory;
  assert.deepEqual(visibleSeqs(controller), range(1, 5));
  assert.ok(controller.snapshot().visibleEvents.every((event) => event.event_id.startsWith("restored-")));
  assert.equal(controller.snapshot().eventsBySeq.size, 5);
  assert.equal(controller.snapshot().pendingEvents.length, 0);
  assert.equal(controller.snapshot().retractedSeqs.size, 0);
});

test("reset is detected even when the restored timeline has regrown beyond the previous head", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  const generation = controller.snapshot().generation;
  source.events = range(1, 600).map((seq) => message(seq, { event_id: `branch-${seq}` }));
  source.head = 600;
  await controller.refresh();
  assert.ok(controller.snapshot().generation > generation);
  assert.deepEqual(visibleSeqs(controller), range(401, 600));
  assert.equal(controller.snapshot().eventsBySeq.get(500)!.event_id, "branch-500");
  assert.equal(controller.snapshot().newEventCount, 0);
});

test("same sequence with a different head event also resets the timeline", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  source.events = range(1, 500).map((seq) => message(seq, { event_id: `replacement-${seq}` }));
  await controller.refresh();
  assert.deepEqual(visibleSeqs(controller), range(301, 500));
  assert.ok(controller.snapshot().visibleEvents.every((event) => event.event_id.startsWith("replacement-")));
});

test("a retraction is immediate for visible content and reply previews before revealing new events", async () => {
  const source = server();
  source.events.find((event) => event.seq === 490)!.reply_to = { seq: 480, event_id: source.events[479].event_id };
  const controller = controllerFor(source);
  await controller.refresh();
  source.events[479] = { ...source.events[479], text: undefined, retracted: true, mentions: [] };
  source.events[500] = message(501, { kind: "event_retracted", target: { seq: 480, event_id: source.events[479].event_id }, reason_code: "other" });
  source.head = 501;
  await controller.refresh();
  const snapshot = controller.snapshot();
  assert.equal(snapshot.visibleEvents.find((event) => event.seq === 480)!.text, undefined);
  const view = eventViewModel(snapshot.eventsBySeq.get(490)!, snapshot.eventsBySeq, snapshot.retractedSeqs)!;
  assert.equal(view.reply!.text, "已撤回");
  assert.equal(snapshot.newEventCount, 1);
  assert.equal(visibleSeqs(controller).at(-1), 500);
});

test("late original content cannot resurrect a retracted reply target", async () => {
  const source = server();
  const target = { seq: 100, event_id: source.events[99].event_id };
  source.events[399].reply_to = target;
  const controller = controllerFor(source);
  await controller.refresh();
  const release = source.deferNext(chunkPath(100));
  const oldReply = controller.loadReply(target);
  source.events[99] = { ...source.events[99], text: undefined, retracted: true, mentions: [] };
  source.events[500] = message(501, { kind: "event_retracted", target, reason_code: "other" });
  source.head = 501;
  await controller.refresh();
  release();
  await oldReply;
  const snapshot = controller.snapshot();
  assert.equal(snapshot.eventsBySeq.get(100)!.text, undefined);
  assert.equal(snapshot.eventsBySeq.get(100)!.retracted, true);
  assert.equal(eventViewModel(snapshot.eventsBySeq.get(400)!, snapshot.eventsBySeq, snapshot.retractedSeqs)!.reply!.text, "已撤回");
  assert.deepEqual(visibleSeqs(controller), range(301, 500));
});

test("a pending message can be retracted without briefly showing its original body", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  source.head = 501;
  await controller.refresh();
  source.events[500] = { ...source.events[500], text: undefined, retracted: true, mentions: [] };
  source.events[501] = message(502, { kind: "event_retracted", target: { seq: 501, event_id: source.events[500].event_id }, reason_code: "owner_request" });
  source.head = 502;
  await controller.refresh();
  assert.equal(controller.snapshot().pendingEvents.find((event) => event.seq === 501)!.text, undefined);
  controller.revealNew();
  assert.equal(controller.snapshot().visibleEvents.find((event) => event.seq === 501)!.text, undefined);
});

test("loading a reply target does not move the history boundary or consumption cursor", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  const before = controller.snapshot();
  await controller.loadReply({ seq: 100, event_id: source.events[99].event_id });
  assert.deepEqual(visibleSeqs(controller), range(301, 500));
  assert.deepEqual(controller.snapshot().cursor, before.cursor);
  await controller.loadEarlier();
  await controller.loadEarlier();
  assert.deepEqual(visibleSeqs(controller), range(1, 500));
});

test("failed history and reply chunks retry on the next poll without moving the cursor", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  const cursor = controller.snapshot().cursor;
  await controller.loadEarlier();
  source.failures.add(chunkPath(1));
  assert.equal(await controller.loadEarlier(), false);
  const target = { seq: 100, event_id: source.events[99].event_id };
  assert.equal(await controller.loadReply(target), null);
  assert.equal(await controller.refresh(), false);
  assert.deepEqual(visibleSeqs(controller), range(201, 500));
  assert.equal(controller.snapshot().connection, "error");
  source.failures.clear();
  assert.equal(await controller.refresh(), true);
  assert.deepEqual(visibleSeqs(controller), range(1, 500));
  assert.equal(controller.snapshot().eventsBySeq.get(100)!.event_id, target.event_id);
  assert.deepEqual(controller.snapshot().cursor, cursor);
});

test("a failed reply chunk retries even when no history request is pending", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  const target = { seq: 100, event_id: source.events[99].event_id };
  source.failures.add(chunkPath(1));
  assert.equal(await controller.loadReply(target), null);
  source.failures.clear();
  assert.equal(await controller.refresh(), true);
  assert.equal(controller.snapshot().eventsBySeq.get(100)!.event_id, target.event_id);
  assert.deepEqual(visibleSeqs(controller), range(301, 500));
});

test("timeline reset cancels failed history and reply retries from the old timeline", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  await controller.loadEarlier();
  source.failures.add(chunkPath(1));
  await controller.loadEarlier();
  await controller.loadReply({ seq: 100, event_id: source.events[99].event_id });
  source.failures.clear();
  source.events = source.events.map((event) => ({ ...event, event_id: `restored-${event.seq}` }));
  source.head = 450;
  source.calls = [];
  assert.equal(await controller.refresh(), true);
  assert.equal(await controller.refresh(), true);
  assert.deepEqual(visibleSeqs(controller), range(251, 450));
  assert.ok(!source.calls.includes(chunkPath(1)));
});

test("a retraction cancels a failed reply retry before its old body is fetched", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  const target = { seq: 100, event_id: source.events[99].event_id };
  source.failures.add(chunkPath(1));
  await controller.loadReply(target);
  source.failures.clear();
  source.events[99] = { ...source.events[99], text: undefined, retracted: true, mentions: [] };
  source.events[500] = message(501, { kind: "event_retracted", target, reason_code: "other" });
  source.head = 501;
  source.calls = [];
  assert.equal(await controller.refresh(), true);
  assert.equal(await controller.refresh(), true);
  assert.ok(!source.calls.includes(chunkPath(1)));
  assert.equal(controller.snapshot().eventsBySeq.has(100), false);
});

test("long loaded history can continue past the JavaScript argument-count limit", async () => {
  const head = 131272;
  const controller = new TimelineController({
    fetchJson: async (path) => {
      assert.equal(path, chunkPath(200));
      return { schema: "tolight-chunk/1", events: range(1, 200).map((seq) => message(seq)) };
    },
  });
  controller.manifest = { ...fixtureManifest, newest_seq: head, newest_event_id: `E-test-${head}` };
  controller.cursor = { seq: head, event_id: `E-test-${head}` };
  // This is the state after reading a long timeline, with its first chunk left.
  for (let seq = 201; seq <= head; seq++) {
    controller.events.set(seq, message(seq));
    controller.visible.add(seq);
  }
  assert.equal(await controller.loadEarlier(), true);
  const snapshot = controller.snapshot();
  assert.equal(snapshot.visibleEvents.length, head);
  assert.equal(snapshot.visibleEvents[0].seq, 1);
  assert.equal(snapshot.visibleEvents.at(-1)!.seq, head);
  assert.equal(snapshot.historyComplete, true);
  assert.equal(snapshot.cursor!.seq, head);
});

test("rendering long history also avoids the JavaScript argument-count limit", () => {
  const document = miniDocument();
  const head = 131072;
  renderApp(document, {
    manifest: fixtureManifest, cursor: null, generation: 0, timelineRevision: 0, pendingEvents: [], loaded: true, connection: "live",
    historyLoading: false, historyComplete: true, newEventCount: 0,
    visibleEvents: range(1, head).map((seq) => message(seq, { kind: "room_paused" })),
    eventsBySeq: new Map(), retractedSeqs: new Set(), participants: [],
  });
  const nodes = document.getElementById("timeline").children;
  assert.equal(nodes.length, head + 1);
  assert.equal(nodes[1].dataset.seq, "1");
  assert.equal(nodes.at(-1)!.dataset.seq, String(head));
});

test("unchanged polls, participant metadata, and pending messages reuse visible views with real Intl", async () => {
  await withDateFormatCounts(async (counts) => {
    const source = server();
    source.events = range(1, 1200).map((seq) => message(seq));
    const controller = controllerFor(source);
    const document = miniDocument();
    const mounted = mountApp({ document, window: miniWindow(), controller });
    try {
      await mounted.ready;
      assert.equal(counts.formats, 400, "each of the initial 200 rows is formatted once");
      const initialConstructors = counts.constructors;
      const timeline = document.getElementById("timeline");
      const replacements = timeline.replaceCount;
      await controller.refresh();
      source.paused = true;
      source.participants[0].bio = "updated public biography";
      await controller.refresh();
      source.head = 501;
      await controller.refresh();
      controller.emit();
      assert.equal(counts.formats, 400);
      assert.equal(timeline.replaceCount, replacements);
      assert.equal(document.getElementById("pause-notice").hidden, false);
      assert.ok(document.getElementById("participants").textContent.includes("updated public biography"));
      assert.equal(controller.snapshot().newEventCount, 1);
      assert.ok(counts.constructors - initialConstructors <= 5, "polling checks the time zone, not one formatter per row");
      controller.revealNew();
      assert.equal(counts.formats, 402, "only the newly revealed row needs date/time formatting");
      assert.equal(timeline.replaceCount, replacements + 1);
    } finally {
      mounted.destroy();
    }
  });
});

test("cached history prefixes and newly fetched history format only newly visible rows", async () => {
  await withDateFormatCounts(async (counts) => {
    const source = server();
    source.events = range(1, 1200).map((seq) => message(seq));
    const controller = controllerFor(source);
    const mounted = mountApp({ document: miniDocument(), window: miniWindow(), controller });
    try {
      await mounted.ready;
      assert.equal(counts.formats, 400);
      assert.equal(await controller.loadEarlier(), true);
      assert.deepEqual(visibleSeqs(controller), range(201, 500));
      assert.equal(counts.formats, 600, "the retained 100-row prefix is formatted once");
      assert.equal(await controller.loadEarlier(), true);
      assert.deepEqual(visibleSeqs(controller), range(1, 500));
      assert.equal(counts.formats, 1000, "only the newly fetched 200 rows are formatted");
      await controller.refresh();
      assert.equal(counts.formats, 1000);
    } finally {
      mounted.destroy();
    }
  });
});

test("reply loading and retractions invalidate only the affected cached views", async () => {
  await withDateFormatCounts(async (counts) => {
    const source = server();
    source.events = range(1, 1200).map((seq) => message(seq));
    const target = { seq: 100, event_id: source.events[99].event_id };
    source.events[399].reply_to = target;
    const controller = controllerFor(source);
    const document = miniDocument();
    const mounted = mountApp({ document, window: miniWindow(), controller });
    try {
      await mounted.ready;
      assert.equal(counts.formats, 400);
      await controller.loadReply(target);
      assert.equal(counts.formats, 402, "loading a target changes one visible reply preview");
      await controller.loadEarlier();
      await controller.loadEarlier();
      assert.equal(counts.formats, 1002);
      source.events[99] = { ...source.events[99], text: undefined, retracted: true, mentions: [] };
      source.events[500] = message(501, { kind: "event_retracted", target, reason_code: "other" });
      source.head = 501;
      await controller.refresh();
      assert.equal(counts.formats, 1006, "only the target and the reply preview change");
      const rows = document.getElementById("timeline").children;
      assert.ok(rows.find((row) => row.dataset.seq === "100")!.textContent.includes("已撤回"));
      assert.ok(rows.find((row) => row.dataset.seq === "400")!.textContent.includes("↳ 已撤回"));
      assert.equal(await controller.loadReply(target), null);
      assert.equal(counts.formats, 1006);
    } finally {
      mounted.destroy();
    }
  });
});

test("a late original reply body cannot invalidate the cached retracted preview", async () => {
  await withDateFormatCounts(async (counts) => {
    const source = server();
    source.events = range(1, 1200).map((seq) => message(seq));
    const target = { seq: 100, event_id: source.events[99].event_id };
    source.events[399].reply_to = target;
    const controller = controllerFor(source);
    const document = miniDocument();
    const mounted = mountApp({ document, window: miniWindow(), controller });
    try {
      await mounted.ready;
      const release = source.deferNext(chunkPath(100));
      const oldReply = controller.loadReply(target);
      source.events[99] = { ...source.events[99], text: undefined, retracted: true, mentions: [] };
      source.events[500] = message(501, { kind: "event_retracted", target, reason_code: "other" });
      source.head = 501;
      await controller.refresh();
      assert.equal(counts.formats, 402);
      release();
      await oldReply;
      assert.equal(counts.formats, 402, "the masked original target is irrelevant to a retracted preview");
      assert.ok(document.getElementById("timeline").children.find((row) => row.dataset.seq === "400")!.textContent.includes("↳ 已撤回"));
      assert.equal(controller.snapshot().eventsBySeq.get(100)!.text, undefined);
    } finally {
      mounted.destroy();
    }
  });
});

test("a failed reply retries its cached visible preview on an unchanged-manifest poll", async () => {
  await withDateFormatCounts(async (counts) => {
    const source = server();
    source.events = range(1, 1200).map((seq) => message(seq));
    const target = { seq: 100, event_id: source.events[99].event_id };
    source.events[399].reply_to = target;
    const controller = controllerFor(source);
    const document = miniDocument();
    const mounted = mountApp({ document, window: miniWindow(), controller });
    try {
      await mounted.ready;
      source.failures.add(chunkPath(100));
      assert.equal(await controller.loadReply(target), null);
      assert.equal(counts.formats, 400);
      assert.equal(controller.snapshot().connection, "error");
      source.failures.clear();
      await controller.refresh();
      assert.equal(counts.formats, 402);
      assert.ok(document.getElementById("timeline").children.find((row) => row.dataset.seq === "400")!.textContent.includes("↳ Iris · 第 100 条新观察"));
      assert.equal(controller.snapshot().connection, "live");
    } finally {
      mounted.destroy();
    }
  });
});

test("timeline resets discard cached views even when sequence positions are reused", async () => {
  await withDateFormatCounts(async (counts) => {
    const source = server();
    source.events = range(1, 1200).map((seq) => message(seq));
    const controller = controllerFor(source);
    const document = miniDocument();
    const mounted = mountApp({ document, window: miniWindow(), controller });
    try {
      await mounted.ready;
      source.events = range(1, 5).map((seq) => message(seq, { event_id: `restored-${seq}`, text: `restored text ${seq}` }));
      source.head = 5;
      await controller.refresh();
      assert.equal(counts.formats, 410);
      assert.deepEqual(visibleSeqs(controller), range(1, 5));
      assert.ok(document.getElementById("timeline").textContent.includes("restored text 1"));
      assert.ok(!document.getElementById("timeline").textContent.includes("第 301 条新观察"));
    } finally {
      mounted.destroy();
    }
  });
});

test("cached views follow a changed local time zone without new timeline data", async () => {
  const originalZone = process.env.TZ;
  process.env.TZ = "UTC";
  try {
    await withDateFormatCounts(async (counts) => {
      const source = server();
      // London and UTC can share today's offset while differing for this event.
      source.events = range(1, 1200).map((seq) => message(seq, { created_at: "2026-06-14T23:15:00Z" }));
      const controller = controllerFor(source);
      const document = miniDocument();
      const mounted = mountApp({ document, window: miniWindow(), controller });
      try {
        await mounted.ready;
        assert.equal(counts.formats, 400);
        assert.ok(document.getElementById("timeline").textContent.includes("23:15"));
        const revision = controller.snapshot().timelineRevision;
        process.env.TZ = "Europe/London";
        controller.emit();
        assert.equal(counts.formats, 800);
        assert.ok(document.getElementById("timeline").textContent.includes("00:15"));
        assert.ok(document.getElementById("timeline").children.find((row) => row.className === "date-divider")!.textContent.includes("6月15日"));
        process.env.TZ = "Asia/Shanghai";
        controller.emit();
        assert.equal(controller.snapshot().timelineRevision, revision);
        assert.equal(counts.formats, 1200);
        assert.ok(document.getElementById("timeline").textContent.includes("07:15"));
        assert.equal(timelineModels(controller.snapshot())[0].date, "2026-6-15");
        const formatted = counts.formats;
        controller.emit();
        assert.equal(counts.formats, formatted);
      } finally {
        mounted.destroy();
      }
    });
  } finally {
    if (originalZone === undefined) delete process.env.TZ;
    else process.env.TZ = originalZone;
  }
});

test("unknown fields and event kinds do not break progress or render unknown text", async () => {
  const source = server();
  const controller = controllerFor(source);
  await controller.refresh();
  source.events[500] = message(501, { kind: "a_future_kind", future_field: "ignored" });
  source.head = 501;
  await controller.refresh();
  assert.equal(controller.snapshot().cursor!.seq, 501);
  assert.equal(controller.snapshot().newEventCount, 0);
  assert.equal(eventViewModel(source.events[500]), null);
});

test("mention highlighting follows the structured list and does not match emails or partial IDs", () => {
  const text = "谢谢@atlas，mail@atlas.com @atlas-extra @iris @atlas";
  const segments = mentionSegments(text, [{ agent_id: "atlas", display_name: "Atlas" }]);
  assert.equal(segments.map((part) => part.text).join(""), text);
  assert.deepEqual(segments.filter((part) => part.mention).map((part) => part.text), ["@atlas", "@atlas"]);
});

test("retraction conversion is pure and removes text/mentions", () => {
  const original = message(1, { mentions: [{ agent_id: "atlas", display_name: "Atlas" }] });
  const [withdrawn] = applyRetractions([original], new Set([1]));
  assert.equal(withdrawn.text, undefined);
  assert.deepEqual(withdrawn.mentions, []);
  assert.equal(original.text, "第 1 条新观察");
  assert.equal(original.mentions!.length, 1);
});

test("DOM renders untrusted content literally, all known state kinds, dates, and data IDs", async () => {
  const document = miniDocument();
  const source = server();
  source.events[499].text = '<script>alert("x")</script> @atlas';
  source.events[499].mentions = [{ agent_id: "atlas", display_name: "Atlas" }];
  const controller = controllerFor(source);
  await controller.refresh();
  await controller.loadEarlier();
  await controller.loadEarlier();
  const snapshot = controller.snapshot();
  renderApp(document, snapshot);
  assert.ok(document.getElementById("timeline").textContent.includes('<script>alert("x")</script> @atlas'));
  assert.ok(!document.createdTags.includes("script"));
  assert.equal(document.getElementById("demo-banner").hidden, false);
  assert.equal(document.getElementById("participants").children.length, 3);
  const rendered = document.getElementById("timeline").children.filter((node) => node.dataset.seq);
  assert.equal(rendered.length, 500);
  assert.equal(rendered.at(-1)!.dataset.seq, "500");
  assert.equal(rendered.at(-1)!.dataset.eventId, source.events[499].event_id);
  assert.ok(document.getElementById("timeline").children.filter((node) => node.className === "date-divider").length > 2);
  for (const kind of ["agent_joined", "agent_left", "room_paused", "room_resumed", "event_retracted"]) {
    const view = timelineModels(snapshot).find((event) => event.kind === kind);
    assert.ok(buildEventElement(document, view).textContent.length > 0);
  }
  assert.ok(document.getElementById("timeline").textContent.includes("已撤回"));
  assert.equal(document.getElementById("history-status").textContent, "没有更早消息");
});

test("mounted UI keeps old reading position, exposes the new-message action, and pauses hidden-tab polling", async () => {
  const source = server();
  const controller = controllerFor(source);
  const document = miniDocument();
  const window = miniWindow();
  const mounted = mountApp({ document, window, controller });
  await mounted.ready;
  assert.equal(window.interval, 5000);
  window.scrollY = 500;
  window.scrolls = [];
  const renderCount = document.getElementById("timeline").replaceCount;
  source.head = 501;
  await controller.refresh();
  assert.equal(window.scrolls.length, 0);
  assert.equal(document.getElementById("timeline").replaceCount, renderCount);
  assert.equal(document.getElementById("new-messages").hidden, false);
  assert.equal(document.getElementById("new-messages").textContent, "有 1 条新消息 ↓");
  document.getElementById("new-messages").dispatch("click");
  assert.equal(window.scrolls.length, 1);
  assert.equal(controller.snapshot().newEventCount, 0);
  const count = source.calls.length;
  document.hidden = true;
  window.tick!();
  assert.equal(source.calls.length, count);
  mounted.destroy();
  assert.equal(window.tick, null);
});

test("older-history insertion preserves the visual position rather than jumping to the bottom", async () => {
  const source = server();
  const controller = controllerFor(source);
  const document = miniDocument();
  const window = miniWindow();
  const mounted = mountApp({ document, window, controller });
  await mounted.ready;
  window.scrollY = 40;
  window.scrolls = [];
  const oldHeight = document.documentElement.scrollHeight;
  await controller.loadEarlier();
  assert.equal(window.scrolls.length, 1);
  assert.equal(window.scrollY, 40 + document.documentElement.scrollHeight - oldHeight);
  mounted.destroy();
});

test("keyboard focus scrolling to the history control does not automatically load history", async () => {
  const source = server();
  const controller = controllerFor(source);
  const document = miniDocument();
  const window = miniWindow();
  const mounted = mountApp({ document, window, controller });
  await mounted.ready;
  window.scrollY = 1500;
  await window.dispatch("scroll");
  document.getElementById("load-earlier").focus();
  window.scrollY = 0;
  await window.dispatch("scroll");
  assert.deepEqual(visibleSeqs(controller), range(301, 500));
  assert.equal(controller.historyLoading, false);
  assert.equal(document.activeElement, document.getElementById("load-earlier"));
  mounted.destroy();
});

test("automatic history retry preserves keyboard focus and its scroll position", async () => {
  const source = server();
  const controller = controllerFor(source);
  const document = miniDocument();
  const window = miniWindow();
  const mounted = mountApp({ document, window, controller });
  await mounted.ready;
  const earlier = document.getElementById("load-earlier");
  window.scrollY = 70;
  earlier.focus();
  await earlier.dispatch("click");
  source.failures.add(chunkPath(1));
  await earlier.dispatch("click");
  source.failures.clear();
  assert.equal(await controller.refresh(), true);
  assert.deepEqual(visibleSeqs(controller), range(1, 500));
  assert.equal(window.scrollY, 70);
  assert.equal(document.activeElement, document.getElementById("history-status"));
  mounted.destroy();
});

test("history completion does not steal focus moved by the reader during the request", async () => {
  const source = server();
  const controller = controllerFor(source);
  const document = miniDocument();
  const window = miniWindow();
  const mounted = mountApp({ document, window, controller });
  await mounted.ready;
  const earlier = document.getElementById("load-earlier");
  earlier.focus();
  await earlier.dispatch("click");
  const release = source.deferNext(chunkPath(1));
  const request = earlier.dispatch("click");
  const elsewhere = document.getElementById("participant-details");
  elsewhere.focus();
  release();
  await request;
  assert.equal(document.activeElement, elsewhere);
  mounted.destroy();
});

test("reply retry keeps keyboard focus on the same message and retraction moves it to that row", async () => {
  const source = server();
  const target = { seq: 100, event_id: source.events[99].event_id };
  source.events[349] = message(350);
  // Keep the focused reply in the current chunk so polls can reread reply_to.
  source.events[449] = message(450, { reply_to: target });
  const controller = controllerFor(source);
  const document = miniDocument();
  const window = miniWindow();
  const mounted = mountApp({ document, window, controller });
  await mounted.ready;
  window.scrollY = 700;
  const button = replyButton(document, 450);
  button.focus();
  source.failures.add(chunkPath(100));
  await button.dispatch("click");
  assert.ok(document.activeElement === button, "failed reply fetch must keep the original control focused");
  source.failures.clear();
  assert.equal(await controller.refresh(), true);
  const restored = replyButton(document, 450);
  assert.ok(document.activeElement === restored, "reply retry must retain focus on the same message");
  assert.deepEqual(restored.focusOptions, { preventScroll: true });
  assert.equal(window.scrollY, 700);
  assert.deepEqual(visibleSeqs(controller), range(301, 500));
  assert.equal(controller.snapshot().cursor!.seq, 500);
  assert.ok(!restored.textContent.includes("查看 #100"));

  source.events[99] = { ...source.events[99], retracted: true, text: undefined, mentions: [] };
  source.events[500] = message(501, { kind: "event_retracted", target, reason_code: "other" });
  source.head = 501;
  await controller.refresh();
  const row = document.getElementById("timeline").children.find((node) => node.dataset.seq === "450")!;
  assert.equal(replyButton(document, 450).disabled, true);
  assert.ok(document.activeElement === row, "a disabled retracted reply must retain focus on its message row");
  assert.equal(row.tabIndex, -1);
  assert.deepEqual(row.focusOptions, { preventScroll: true });
  assert.equal(window.scrollY, 700);

  // The server can remove reply_to entirely when rebuilding a retracted row.
  source.events[449] = { ...source.events[449], retracted: true, text: undefined, mentions: [], reply_to: null };
  source.events[501] = message(502, {
    kind: "event_retracted", target: { seq: 450, event_id: source.events[449].event_id }, reason_code: "other",
  });
  source.head = 502;
  await controller.refresh();
  const withoutButton = document.getElementById("timeline").children.find((node) => node.dataset.seq === "450")!;
  assert.equal(withoutButton.getElementsByTagName("button").length, 0);
  assert.ok(document.activeElement === withoutButton, "a removed reply control must retain focus on its message row");
  assert.equal(withoutButton.tabIndex, -1);
  assert.deepEqual(withoutButton.focusOptions, { preventScroll: true });
  assert.equal(window.scrollY, 700);

  // Another visible message can change while focus rests on the fallback row.
  source.events[349] = { ...source.events[349], retracted: true, text: undefined, mentions: [] };
  source.events[502] = message(503, {
    kind: "event_retracted", target: { seq: 350, event_id: source.events[349].event_id }, reason_code: "other",
  });
  source.head = 503;
  await controller.refresh();
  const keptRow = document.getElementById("timeline").children.find((node) => node.dataset.seq === "450")!;
  assert.ok(document.activeElement === keptRow, "later redraws must preserve the reply fallback focus");
  assert.equal(keptRow.tabIndex, -1);
  assert.deepEqual(keptRow.focusOptions, { preventScroll: true });
  assert.equal(window.scrollY, 700);
  mounted.destroy();
});

test("history retry preserves a focused reply control and its reading position", async () => {
  const source = server();
  source.events[399] = message(400, { reply_to: { seq: 100, event_id: source.events[99].event_id } });
  const controller = controllerFor(source);
  const document = miniDocument();
  const window = miniWindow();
  const mounted = mountApp({ document, window, controller });
  await mounted.ready;
  await controller.loadEarlier();
  source.failures.add(chunkPath(1));
  assert.equal(await controller.loadEarlier(), false);
  const button = replyButton(document, 400);
  button.focus();
  window.scrollY = 700;
  const height = document.documentElement.scrollHeight;
  source.failures.clear();
  assert.equal(await controller.refresh(), true);
  assert.ok(document.activeElement === replyButton(document, 400), "history retry must retain the reader's reply control");
  assert.deepEqual(document.activeElement.focusOptions, { preventScroll: true });
  assert.equal(window.scrollY, 700 + document.documentElement.scrollHeight - height);
  assert.deepEqual(visibleSeqs(controller), range(1, 500));
  mounted.destroy();
});

test("reply completion does not take focus back after the reader moved elsewhere", async () => {
  const source = server();
  source.events[399] = message(400, { reply_to: { seq: 100, event_id: source.events[99].event_id } });
  const controller = controllerFor(source);
  const document = miniDocument();
  const window = miniWindow();
  const mounted = mountApp({ document, window, controller });
  await mounted.ready;
  const button = replyButton(document, 400);
  button.focus();
  const release = source.deferNext(chunkPath(100));
  const request = button.dispatch("click");
  const elsewhere = document.getElementById("participant-details");
  elsewhere.focus();
  release();
  await request;
  assert.equal(document.activeElement, elsewhere);
  mounted.destroy();
});

test("keyboard history activation keeps the control visible and restores focus after busy state or failure", async () => {
  const source = server();
  const controller = controllerFor(source);
  const document = miniDocument();
  const window = miniWindow();
  const mounted = mountApp({ document, window, controller });
  await mounted.ready;
  const earlier = document.getElementById("load-earlier");
  window.scrollY = 70;
  window.scrolls = [];
  earlier.focus();
  await earlier.dispatch("click");
  assert.deepEqual(visibleSeqs(controller), range(201, 500));
  assert.equal(window.scrollY, 70);
  assert.equal(document.activeElement, earlier);
  assert.deepEqual(earlier.focusOptions, { preventScroll: true });
  source.failures.add(chunkPath(1));
  await earlier.dispatch("click");
  assert.equal(controller.snapshot().connection, "error");
  assert.equal(window.scrollY, 70);
  assert.equal(document.activeElement, earlier);
  source.failures.clear();
  await earlier.dispatch("click");
  assert.deepEqual(visibleSeqs(controller), range(1, 500));
  assert.equal(window.scrollY, 70);
  assert.equal(earlier.hidden, true);
  assert.equal(document.activeElement, document.getElementById("history-status"));
  assert.equal(document.activeElement.textContent, "没有更早消息");
  assert.deepEqual(document.activeElement.focusOptions, { preventScroll: true });
  mounted.destroy();
});

test("the page has no writing controls, navigation links, external resources, or unsafe DOM sinks", async () => {
  const html = await readFile(new URL("../../public/index.html", import.meta.url), "utf8");
  const js = await readFile(new URL("../../public/app.js", import.meta.url), "utf8");
  assert.doesNotMatch(html, /<(?:input|textarea|form|a)\b|contenteditable/iu);
  assert.doesNotMatch(html, /(?:src|href)\s*=\s*["']https?:\/\//iu);
  assert.doesNotMatch(js, /innerHTML|insertAdjacentHTML|WebSocket|\b(?:POST|PUT|PATCH|DELETE)\b/u);
  assert.match(js, /textContent/u);
  assert.match(html, /<script type="module" src="app\.js"><\/script>/u);
});

function lanJoinFixture() {
  const document = miniDocument();
  const ids = ["lan-join", "lan-join-open", "lan-join-copy", "lan-join-prompt", "lan-join-copy-status"];
  for (const id of ids) {
    const node = document.createElement(id === "lan-join-prompt" ? "pre" : "div");
    document.elements.set(id, node);
  }
  let selected: Element | null = null;
  let legacyResult = true;
  let opens = 0;
  const copied: string[] = [];
  const navigator: { clipboard?: { writeText(text: string): Promise<void> } } = {};
  const selection = {
    removeAllRanges() { selected = null; },
    addRange(range: { node: Element | null }) { selected = range.node; },
  };
  Object.assign(document, {
    createRange() { return { node: null as Element | null, selectNodeContents(node: Element) { this.node = node; } }; },
    execCommand(command: string) {
      assert.equal(command, "copy");
      if (legacyResult) copied.push(selected?.textContent ?? "");
      return legacyResult;
    },
  });
  Object.assign(document.getElementById("lan-join"), { scrollIntoView() { opens++; } });
  const scrolls: ScrollToOptions[] = [];
  const window = { navigator, scrollX: 0, scrollY: 120, scrollTo(options: ScrollToOptions) { scrolls.push(options); }, getSelection() { return selection; } };
  const payload = {
    schema: "tolight-lan-join/1",
    participant_base_url: "https://192.168.10.66:8443/participant/v1/",
    ca_sha256: "c".repeat(64),
    prompt: "接入 https://192.168.10.66:8443/participant/v1/\n字面文本 <img src=x>，完成沙盒后等待人工审核。",
  };
  return {
    document, navigator, copied, payload, scrolls,
    get selected() { return selected; },
    get opens() { return opens; },
    set legacyResult(value: boolean) { legacyResult = value; },
    mount(fetchJson = async (_path: string): Promise<unknown> => payload) {
      return mountLanJoin({ document: document as unknown as Document, window: window as unknown as Window, fetchJson });
    },
  };
}

test("LAN instructions render as literal read-only text and copy via Clipboard API", async () => {
  const fixture = lanJoinFixture();
  fixture.navigator.clipboard = { async writeText(text) { fixture.copied.push(text); } };
  const mounted = fixture.mount(async (path) => {
    assert.equal(path, "lan-join.json");
    return fixture.payload;
  });
  assert.equal(await mounted.ready, true);
  const get = (id: string) => fixture.document.getElementById(id);
  assert.equal(get("lan-join").hidden, false);
  assert.equal(get("lan-join-open").hidden, false);
  assert.equal(get("lan-join-prompt").textContent, fixture.payload.prompt);
  assert.equal(get("lan-join-prompt").children.length, 0);
  await get("lan-join-copy").dispatch("click");
  assert.deepEqual(fixture.copied, [fixture.payload.prompt]);
  assert.match(get("lan-join-copy-status").textContent, /已复制/u);
  await get("lan-join-open").dispatch("click");
  assert.equal(fixture.opens, 1);
  mounted.destroy();
  await get("lan-join-copy").dispatch("click");
  assert.equal(fixture.copied.length, 1);
  assert.equal(get("lan-join").hidden, true);
});

test("LAN HTTP copies selected visible text without a secure-context Clipboard API", async () => {
  const fixture = lanJoinFixture();
  const mounted = fixture.mount();
  await mounted.ready;
  await fixture.document.getElementById("lan-join-copy").dispatch("click");
  assert.deepEqual(fixture.copied, [fixture.payload.prompt]);
  assert.equal(fixture.selected, null);
  assert.deepEqual(fixture.scrolls, [{ left: 0, top: 120, behavior: "auto" }]);
  assert.equal(fixture.document.activeElement, fixture.document.getElementById("lan-join-copy"));
  assert.match(fixture.document.getElementById("lan-join-copy-status").textContent, /已复制/u);
  mounted.destroy();
});

test("denied clipboard and legacy copy leave instructions selected for manual copying", async () => {
  const fixture = lanJoinFixture();
  fixture.navigator.clipboard = { async writeText() { throw new Error("Permission denied"); } };
  fixture.legacyResult = false;
  const mounted = fixture.mount();
  await mounted.ready;
  await fixture.document.getElementById("lan-join-copy").dispatch("click");
  assert.deepEqual(fixture.copied, []);
  assert.equal(fixture.selected, fixture.document.getElementById("lan-join-prompt"));
  assert.equal(fixture.document.activeElement, fixture.selected);
  assert.match(fixture.document.getElementById("lan-join-copy-status").textContent, /Ctrl\+C/u);
  mounted.destroy();
});

test("absent or invalid LAN metadata keeps the optional panel hidden", async () => {
  for (const input of [null, {}, { schema: "wrong" }, { schema: "tolight-lan-join/1", prompt: "unverified" }, "offline"]) {
    const fixture = lanJoinFixture();
    const mounted = fixture.mount(async () => {
      if (input === "offline") throw new Error("404");
      return input;
    });
    assert.equal(await mounted.ready, false);
    assert.equal(fixture.document.getElementById("lan-join").hidden, true);
    assert.equal(fixture.document.getElementById("lan-join-open").hidden, true);
    await fixture.document.getElementById("lan-join-copy").dispatch("click");
    assert.deepEqual(fixture.copied, []);
    mounted.destroy();
  }
});

test("unmounted LAN instructions ignore late metadata responses", async () => {
  const fixture = lanJoinFixture();
  let resolve!: (value: unknown) => void;
  const mounted = fixture.mount(() => new Promise((done) => { resolve = done; }));
  mounted.destroy();
  resolve(fixture.payload);
  assert.equal(await mounted.ready, false);
  assert.equal(fixture.document.getElementById("lan-join").hidden, true);
  assert.equal(fixture.document.getElementById("lan-join-prompt").textContent, "");
});

test("an empty room keeps the first screen at the top for LAN onboarding", async () => {
  const source = server(0);
  const document = miniDocument();
  const window = miniWindow();
  const mounted = mountApp({ document, window, controller: controllerFor(source) });
  await mounted.ready;
  assert.equal(document.getElementById("empty-state").hidden, false);
  assert.deepEqual(window.scrolls, []);
  mounted.destroy();
});
