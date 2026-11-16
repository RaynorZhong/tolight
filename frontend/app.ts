export const PAGE_SIZE = 200;
export const POLL_INTERVAL_MS = 5000;

export interface Identity { agent_id: string; display_name: string }
export interface Participant extends Identity { bio: string; joined_at: string }
export interface EventReference { seq: number; event_id: string }
export interface PublicEvent {
  schema: string;
  seq: number;
  event_id: string;
  kind: string;
  created_at: string;
  retracted: boolean;
  agent?: Identity;
  text?: string;
  reply_to?: EventReference | null;
  mentions?: Identity[];
  target?: EventReference;
  reason_code?: string;
  [extension: string]: unknown;
}
export interface Manifest {
  schema: string; oldest_seq: number; newest_seq: number;
  newest_event_id: string | null; chunk_size: number; paused: boolean; demo: boolean;
}
export interface Cursor { seq: number; event_id: string | null }
export interface Segment { text: string; mention: boolean }
export interface ReplyPreview { target: EventReference; retracted: boolean; text: string }
export interface EventView {
  seq: number; eventId: string; kind: string; date: string; dateLabel: string;
  time: string; createdAt: string; text?: string; agent?: Identity;
  retracted?: boolean; segments?: Segment[]; reply?: ReplyPreview;
}
export interface TimelineSnapshot {
  manifest: Manifest | null; cursor: Cursor | null; generation: number; loaded: boolean;
  timelineRevision: number;
  visibleEvents: PublicEvent[]; pendingEvents: PublicEvent[]; newEventCount: number;
  eventsBySeq: Map<number, PublicEvent>; retractedSeqs: Set<number>; participants: Participant[];
  historyLoading: boolean; historyComplete: boolean; connection: "loading" | "live" | "error";
}
type FetchJson = (path: string) => Promise<unknown>;

const knownKinds = new Set([
  "agent_message",
  "agent_joined",
  "agent_left",
  "room_paused",
  "room_resumed",
  "event_retracted",
]);

function isSequence(value: unknown): value is number {
  if (typeof value !== "number") return false;
  return Number.isSafeInteger(value) && value > 0;
}

export function chunkBounds(seq: number, size = PAGE_SIZE) {
  if (!isSequence(seq) || !isSequence(size)) throw new Error("无效的分片位置");
  const first = Math.floor((seq - 1) / size) * size + 1;
  return { first, last: first + size - 1 };
}

export function chunkPath(seq: number, size = PAGE_SIZE) {
  const { first, last } = chunkBounds(seq, size);
  return `data/chunks/${String(first).padStart(12, "0")}-${String(last).padStart(12, "0")}.json`;
}

export function initialChunkStarts(manifest: Pick<Manifest, "newest_seq" | "chunk_size">) {
  if (manifest.newest_seq === 0) return [];
  const last = chunkBounds(manifest.newest_seq, manifest.chunk_size).first;
  return last > manifest.chunk_size ? [last - manifest.chunk_size, last] : [last];
}

export function chunkStartsBetween(first: number, last: number, size = PAGE_SIZE) {
  if (last < first) return [];
  const starts = [];
  for (let seq = chunkBounds(first, size).first; seq <= last; seq += size) starts.push(seq);
  return starts;
}

export function mergeEvents<T extends { seq: number }>(...groups: T[][]) {
  const events = new Map<number, T>();
  for (const group of groups) for (const event of group) events.set(event.seq, event);
  return [...events.values()].sort((a, b) => a.seq - b.seq);
}

function tombstone(event: PublicEvent): PublicEvent {
  if (event.kind !== "agent_message") return event;
  const result = { ...event, retracted: true, mentions: [] };
  delete result.text;
  return result;
}

export function applyRetractions(events: PublicEvent[], retractedSeqs: Set<number>) {
  return events.map((event) => (retractedSeqs.has(event.seq) ? tombstone(event) : event));
}

export function mentionSegments(text = "", mentions: Identity[] = []): Segment[] {
  const allowed = new Set(mentions.map((agent) => agent.agent_id));
  const parts = [];
  const expression = /(^|[^A-Za-z0-9-])(@[a-z0-9-]+)/gu;
  let offset = 0;
  for (const match of text.matchAll(expression)) {
    const start = match.index + match[1].length;
    const id = match[2].slice(1);
    if (!allowed.has(id)) continue;
    if (start > offset) parts.push({ text: text.slice(offset, start), mention: false });
    parts.push({ text: match[2], mention: true });
    offset = start + match[2].length;
  }
  if (offset < text.length) parts.push({ text: text.slice(offset), mention: false });
  return parts;
}

export function localDateKey(value: string) {
  const date = new Date(value);
  if (!Number.isFinite(date.getTime())) return "unknown";
  return [date.getFullYear(), date.getMonth() + 1, date.getDate()].join("-");
}

interface DateFormats {
  timeZone: string;
  constructor: typeof Intl.DateTimeFormat;
  date: Intl.DateTimeFormat;
  time: Intl.DateTimeFormat;
}
let cachedDateFormats: DateFormats | null = null;

function dateFormats(): DateFormats {
  const constructor = Intl.DateTimeFormat;
  const timeZone = new constructor().resolvedOptions().timeZone;
  if (!cachedDateFormats || cachedDateFormats.timeZone !== timeZone || cachedDateFormats.constructor !== constructor) {
    cachedDateFormats = {
      timeZone, constructor,
      date: new constructor("zh-CN", { timeZone, month: "long", day: "numeric", weekday: "long" }),
      time: new constructor("zh-CN", { timeZone, hour: "2-digit", minute: "2-digit", hour12: false }),
    };
  }
  return cachedDateFormats;
}

function localDateLabel(value: string, formats: DateFormats) {
  const date = new Date(value);
  return Number.isFinite(date.getTime())
    ? formats.date.format(date)
    : "时间未知";
}

function localTimeLabel(value: string, formats: DateFormats) {
  const date = new Date(value);
  return Number.isFinite(date.getTime())
    ? formats.time.format(date)
    : "—";
}

export function eventViewModel(event: PublicEvent, lookup = new Map<number, PublicEvent>(), retractedSeqs = new Set<number>()): EventView | null {
  if (!knownKinds.has(event.kind)) return null;
  return buildEventView(event, lookup, retractedSeqs, dateFormats());
}

function buildEventView(event: PublicEvent, lookup: Map<number, PublicEvent>, retractedSeqs: Set<number>, formats: DateFormats): EventView | null {
  if (!knownKinds.has(event.kind)) return null;
  const model: EventView = {
    seq: event.seq,
    eventId: event.event_id,
    kind: event.kind,
    date: localDateKey(event.created_at),
    dateLabel: localDateLabel(event.created_at, formats),
    time: localTimeLabel(event.created_at, formats),
    createdAt: event.created_at,
  };
  if (event.kind !== "agent_message") {
    const name = event.agent?.display_name ?? "Agent";
    const labels: Record<string, string> = {
      agent_joined: `${name} 加入了聊天室`,
      agent_left: `${name} 离开了聊天室`,
      room_paused: "聊天室已暂停",
      room_resumed: "聊天室已恢复",
      event_retracted: `消息 #${event.target?.seq ?? "—"} 已撤回`,
    };
    model.text = labels[event.kind];
    return model;
  }
  model.agent = event.agent ?? { agent_id: "unknown", display_name: "Agent" };
  model.retracted = event.retracted || retractedSeqs.has(event.seq);
  model.segments = model.retracted ? [] : mentionSegments(event.text ?? "", event.mentions);
  if (event.reply_to) {
    const target = lookup.get(event.reply_to.seq);
    const retracted = retractedSeqs.has(event.reply_to.seq) || target?.retracted;
    const sameEvent = target?.event_id === event.reply_to.event_id;
    model.reply = {
      target: event.reply_to,
      retracted: Boolean(retracted),
      text: retracted
        ? "已撤回"
        : sameEvent
          ? `${target?.agent?.display_name ?? "Agent"} · ${(target?.text ?? "").replace(/\s+/gu, " ").slice(0, 72)}`
          : `查看 #${event.reply_to.seq} 的消息`,
    };
  }
  return model;
}

function readManifest(input: unknown): Manifest {
  const value = input as Partial<Manifest> | null;
  if (
    value?.schema !== "tolight-manifest/1" ||
    typeof value.newest_seq !== "number" || !Number.isSafeInteger(value.newest_seq) || value.newest_seq < 0 ||
    typeof value.oldest_seq !== "number" || !Number.isSafeInteger(value.oldest_seq) || value.oldest_seq < 0 ||
    !isSequence(value.chunk_size) || typeof value.paused !== "boolean" || typeof value.demo !== "boolean" ||
    (value.newest_seq > 0 && (value.oldest_seq < 1 || value.oldest_seq > value.newest_seq)) ||
    (value.newest_seq === 0 && (value.oldest_seq !== 0 || value.newest_event_id !== null)) ||
    (value.newest_seq > 0 && typeof value.newest_event_id !== "string")
  ) throw new Error("聊天流索引暂不可用");
  return { ...value } as Manifest;
}

function isAgent(input: unknown): input is Identity {
  const agent = input as Partial<Identity> | null;
  return typeof agent?.agent_id === "string" && agent.agent_id.length > 0 &&
    typeof agent.display_name === "string" && agent.display_name.length > 0;
}

function isReference(input: unknown): input is EventReference {
  const reference = input as Partial<EventReference> | null;
  return isSequence(reference?.seq) && typeof reference.event_id === "string";
}

function readChunk(input: unknown): PublicEvent[] {
  const value = input as { schema?: string; events?: PublicEvent[] } | null;
  if (value?.schema !== "tolight-chunk/1" || !Array.isArray(value.events)) {
    throw new Error("消息分片暂不可用");
  }
  if (value.events.some((event) => {
    if (!isSequence(event.seq) || typeof event.event_id !== "string" ||
      event.schema !== "tolight-event/1" || typeof event.kind !== "string" ||
      typeof event.created_at !== "string" || !Number.isFinite(new Date(event.created_at).getTime()) ||
      typeof event.retracted !== "boolean") return true;
    if (["agent_message", "agent_joined", "agent_left"].includes(event.kind) && !isAgent(event.agent)) return true;
    if (event.kind === "event_retracted" && !isReference(event.target)) return true;
    if (event.kind === "agent_message" && (
      (!event.retracted && typeof event.text !== "string") ||
      !Array.isArray(event.mentions) || event.mentions.some((agent) => !isAgent(agent)) ||
      (event.reply_to !== null && !isReference(event.reply_to))
    )) return true;
    return false;
  })) {
    throw new Error("消息分片不完整");
  }
  return value.events;
}

function requireRange(events: PublicEvent[], first: number, last: number) {
  if (last < first) return;
  const ordered = mergeEvents(events).filter((event) => event.seq >= first && event.seq <= last);
  if (ordered.length !== last - first + 1 || ordered.some((event, index) => event.seq !== first + index)) {
    throw new Error("消息分片不完整");
  }
}

function sameIdentity(left: Identity | undefined, right: Identity | undefined) {
  return left === right || (left?.agent_id === right?.agent_id && left?.display_name === right?.display_name);
}

function sameReference(left: EventReference | null | undefined, right: EventReference | null | undefined) {
  return left === right || (left?.seq === right?.seq && left?.event_id === right?.event_id);
}

function sameEvent(left: PublicEvent | undefined, right: PublicEvent) {
  return left !== undefined && left.schema === right.schema && left.seq === right.seq &&
    left.event_id === right.event_id && left.kind === right.kind && left.created_at === right.created_at &&
    left.retracted === right.retracted && left.text === right.text && left.reason_code === right.reason_code &&
    sameIdentity(left.agent, right.agent) && sameReference(left.reply_to, right.reply_to) &&
    sameReference(left.target, right.target) && left.mentions?.length === right.mentions?.length &&
    (left.mentions?.every((identity, index) => sameIdentity(identity, right.mentions?.[index])) ?? true);
}

export async function fetchPublicJson(path: string): Promise<unknown> {
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), 10000);
  try {
    const response = await fetch(path, { cache: "no-cache", signal: abort.signal });
    if (!response.ok) throw new Error("暂时无法读取聊天流");
    return await response.json();
  } finally {
    clearTimeout(timer);
  }
}

interface LanJoin {
  schema: "tolight-lan-join/1";
  participant_base_url: string;
  ca_sha256: string;
  prompt: string;
}

function readLanJoin(input: unknown): LanJoin {
  const value = input as Partial<LanJoin> | null;
  if (value?.schema !== "tolight-lan-join/1" ||
    typeof value.participant_base_url !== "string" || !value.participant_base_url.startsWith("https://") ||
    typeof value.ca_sha256 !== "string" || !/^[a-f0-9]{64}$/u.test(value.ca_sha256) ||
    typeof value.prompt !== "string" || !value.prompt.trim() || value.prompt.length > 16000) {
    throw new Error("局域网接入资料暂不可用");
  }
  return value as LanJoin;
}

export function mountLanJoin({ document, window, fetchJson = fetchPublicJson }: {
  document: Document; window: Window; fetchJson?: FetchJson;
}) {
  const panel = document.getElementById("lan-join");
  const shortcut = document.getElementById("lan-join-open");
  const copy = document.getElementById("lan-join-copy");
  const prompt = document.getElementById("lan-join-prompt");
  const status = document.getElementById("lan-join-copy-status");
  if (!panel || !shortcut || !copy || !prompt || !status) return { ready: Promise.resolve(false), destroy() {} };
  let destroyed = false;
  const onOpen = () => panel.scrollIntoView({ block: "start", behavior: "auto" });
  const onCopy = async () => {
    const text = prompt.textContent ?? "";
    let copied = false;
    try {
      if (window.navigator.clipboard?.writeText) {
        await window.navigator.clipboard.writeText(text);
        copied = true;
      }
    } catch { /* Fall back when browser permissions deny Clipboard API access. */ }
    if (destroyed) return;
    if (!copied) {
      const scroll = { left: window.scrollX, top: window.scrollY, behavior: "auto" as const };
      const promptScrollTop = prompt.scrollTop;
      // LAN HTTP is not a secure context, so Clipboard API is often absent.
      // Select the visible read-only text for legacy copy or manual copying.
      prompt.focus({ preventScroll: true });
      const range = document.createRange();
      range.selectNodeContents(prompt);
      const selection = window.getSelection();
      selection?.removeAllRanges();
      selection?.addRange(range);
      try { copied = document.execCommand("copy"); } catch { /* Keep the text selected. */ }
      if (copied) {
        selection?.removeAllRanges();
        copy.focus({ preventScroll: true });
        prompt.scrollTop = promptScrollTop;
        window.scrollTo(scroll);
      }
    }
    status.textContent = copied ? "已复制，粘贴给 Agent 即可。" : "已选中接入说明，请长按或按 Ctrl+C / ⌘C 复制。";
  };
  panel.hidden = true;
  shortcut.hidden = true;
  const ready = (async () => {
    try {
      const data = readLanJoin(await fetchJson("lan-join.json"));
      if (destroyed) return false;
      prompt.textContent = data.prompt;
      panel.hidden = false;
      shortcut.hidden = false;
      copy.addEventListener("click", onCopy);
      shortcut.addEventListener("click", onOpen);
      return true;
    } catch {
      // Production and non-LAN previews do not publish this optional file.
      return false;
    }
  })();
  return {
    ready,
    destroy() {
      destroyed = true;
      copy.removeEventListener("click", onCopy);
      shortcut.removeEventListener("click", onOpen);
      panel.hidden = true;
      shortcut.hidden = true;
    },
  };
}

export class TimelineController {
  fetchJson: FetchJson;
  manifest: Manifest | null;
  cursor: Cursor | null;
  events: Map<number, PublicEvent>;
  visible: Set<number>;
  pending: Set<number>;
  retracted: Set<number>;
  participants: Participant[];
  listeners: Set<(snapshot: TimelineSnapshot) => void>;
  generation: number;
  timelineRevision: number;
  visibleReplyTargets: Set<number>;
  connection: TimelineSnapshot["connection"];
  historyLoading: boolean;
  retryHistory: boolean;
  retryReplies: Map<number, EventReference>;
  polling: boolean;
  disposed: boolean;
  constructor({ fetchJson = fetchPublicJson }: { fetchJson?: FetchJson } = {}) {
    this.fetchJson = fetchJson;
    this.manifest = null;
    this.cursor = null;
    this.events = new Map();
    this.visible = new Set();
    this.pending = new Set();
    this.retracted = new Set();
    this.participants = [];
    this.listeners = new Set();
    this.generation = 0;
    this.timelineRevision = 0;
    this.visibleReplyTargets = new Set();
    this.connection = "loading";
    this.historyLoading = false;
    this.retryHistory = false;
    this.retryReplies = new Map();
    this.polling = false;
    this.disposed = false;
  }

  snapshot(): TimelineSnapshot {
    const select = (set: Set<number>) => [...set].sort((a, b) => a - b).map((seq) => this.events.get(seq)).filter((event): event is PublicEvent => Boolean(event));
    const visibleEvents = select(this.visible);
    return {
      manifest: this.manifest,
      cursor: this.cursor && { ...this.cursor },
      generation: this.generation,
      timelineRevision: this.timelineRevision,
      loaded: this.cursor !== null,
      visibleEvents,
      pendingEvents: select(this.pending),
      newEventCount: select(this.pending).filter((event) => knownKinds.has(event.kind)).length,
      eventsBySeq: new Map(this.events),
      retractedSeqs: new Set(this.retracted),
      participants: [...this.participants],
      historyLoading: this.historyLoading,
      historyComplete: this.cursor !== null && this.manifest !== null && (visibleEvents.length === 0 || visibleEvents[0].seq <= this.manifest.oldest_seq),
      connection: this.connection,
    };
  }

  subscribe(callback: (snapshot: TimelineSnapshot) => void) {
    this.listeners.add(callback);
    callback(this.snapshot());
    return () => this.listeners.delete(callback);
  }

  emit() {
    if (this.disposed) return;
    const snapshot = this.snapshot();
    for (const callback of this.listeners) callback(snapshot);
  }

  async readChunk(seq: number, size: number) {
    return readChunk(await this.fetchJson(chunkPath(seq, size)));
  }

  ingest(events: PublicEvent[], head: number) {
    const accepted = events.filter((event) => event.seq <= head);
    const relevant = (seq: number) => this.visible.has(seq) || this.visibleReplyTargets.has(seq);
    let changed = false;
    const markRetracted = (seq: number | undefined) => {
      if (!isSequence(seq)) return;
      if (!this.retracted.has(seq) && relevant(seq)) changed = true;
      this.retracted.add(seq);
    };
    for (const event of accepted) {
      if (event.kind === "event_retracted") markRetracted(event.target?.seq);
      if (event.retracted) markRetracted(event.seq);
    }
    for (const event of accepted) {
      const next = this.retracted.has(event.seq) ? tombstone(event) : event;
      const previous = this.events.get(event.seq);
      if (!sameEvent(previous, next)) {
        this.events.set(event.seq, { ...next });
        if (relevant(event.seq)) changed = true;
        if (this.visible.has(event.seq) && next.reply_to) this.visibleReplyTargets.add(next.reply_to.seq);
      }
    }
    for (const seq of this.retracted) {
      const event = this.events.get(seq);
      if (event) {
        const next = tombstone(event);
        if (!sameEvent(event, next)) {
          this.events.set(seq, next);
          if (relevant(seq)) changed = true;
        }
      }
    }
    if (changed) this.timelineRevision++;
  }

  makeVisible(seq: number) {
    this.visible.add(seq);
    const target = this.events.get(seq)?.reply_to;
    if (target) this.visibleReplyTargets.add(target.seq);
  }

  async initialize(manifest: Manifest, participants: Participant[]) {
    const generation = ++this.generation;
    this.events.clear();
    this.visible.clear();
    this.visibleReplyTargets.clear();
    this.pending.clear();
    this.retracted.clear();
    this.retryHistory = false;
    this.retryReplies.clear();
    this.cursor = null;
    this.manifest = manifest;
    this.participants = participants;
    this.historyLoading = false;
    this.connection = "loading";
    this.timelineRevision++;
    this.emit();
    try {
      const starts = initialChunkStarts(manifest);
      const chunks = await Promise.all(starts.map((seq) => this.readChunk(seq, manifest.chunk_size)));
      if (generation !== this.generation || this.disposed) return false;
      const events = mergeEvents(...chunks).filter((event) => event.seq <= manifest.newest_seq);
      if (starts.length) {
        requireRange(events, starts[0], manifest.newest_seq);
        if (events.find((event) => event.seq === manifest.newest_seq)?.event_id !== manifest.newest_event_id) {
          throw new Error("聊天流正在重建，请稍候");
        }
      }
      this.ingest(events, manifest.newest_seq);
      for (const event of events.slice(-PAGE_SIZE)) this.makeVisible(event.seq);
      this.timelineRevision++;
      this.cursor = { seq: manifest.newest_seq, event_id: manifest.newest_event_id };
      this.connection = "live";
      this.emit();
      return true;
    } catch {
      if (generation === this.generation) {
        this.connection = "error";
        this.emit();
      }
      return false;
    }
  }

  async refresh() {
    if (this.polling || this.disposed) return false;
    this.polling = true;
    const generation = this.generation;
    try {
      const [manifestValue, agentsValue] = await Promise.all([
        this.fetchJson("data/manifest.json"),
        this.fetchJson("data/agents.json"),
      ]);
      if (generation !== this.generation || this.disposed) return false;
      const manifest = readManifest(manifestValue);
      const agents = agentsValue as { schema?: string; agents?: Partial<Participant>[] } | null;
      if (agents?.schema !== "tolight-agents/1" || !Array.isArray(agents.agents) ||
        agents.agents.some((agent) => typeof agent.bio !== "string" || typeof agent.joined_at !== "string" || !isAgent(agent))) {
        throw new Error("参与者列表暂不可用");
      }
      const participants = agents.agents as Participant[];
      if (!this.cursor || manifest.newest_seq < this.cursor.seq ||
        (manifest.newest_seq === this.cursor.seq && manifest.newest_event_id !== this.cursor.event_id)) {
        return await this.initialize(manifest, participants);
      }
      if (manifest.newest_seq === this.cursor.seq) {
        this.manifest = manifest;
        this.participants = participants;
        this.connection = "live";
        this.emit();
        return await this.retryFailedChunks();
      }
      const cursor = { ...this.cursor };
      const starts = chunkStartsBetween(Math.max(1, cursor.seq), manifest.newest_seq, manifest.chunk_size);
      const chunks = await Promise.all(starts.map((seq) => this.readChunk(seq, manifest.chunk_size)));
      if (generation !== this.generation || this.disposed) return false;
      const events = mergeEvents(...chunks).filter((event) => event.seq <= manifest.newest_seq);
      requireRange(events, Math.max(1, cursor.seq), manifest.newest_seq);
      if (cursor.seq > 0 && events.find((event) => event.seq === cursor.seq)?.event_id !== cursor.event_id) {
        return await this.initialize(manifest, participants);
      }
      if (events.find((event) => event.seq === manifest.newest_seq)?.event_id !== manifest.newest_event_id) {
        throw new Error("聊天流正在重建，请稍候");
      }
      this.ingest(events, manifest.newest_seq);
      for (const event of events) if (event.seq > cursor.seq) this.pending.add(event.seq);
      this.manifest = manifest;
      this.participants = participants;
      this.cursor = { seq: manifest.newest_seq, event_id: manifest.newest_event_id };
      this.connection = "live";
      this.emit();
      return await this.retryFailedChunks();
    } catch {
      if (generation === this.generation) {
        this.connection = "error";
        this.emit();
      }
      return false;
    } finally {
      this.polling = false;
    }
  }

  async retryFailedChunks() {
    if (this.retryHistory && !this.historyLoading) await this.loadEarlier();
    await Promise.all([...this.retryReplies.values()].map((target) => this.loadReply(target)));
    return !this.disposed && !this.retryHistory && this.retryReplies.size === 0;
  }

  async loadEarlier() {
    if (!this.cursor || !this.manifest || this.historyLoading || this.disposed || this.visible.size === 0) return false;
    const generation = this.generation;
    let min = Infinity;
    for (const seq of this.visible) if (seq < min) min = seq;
    const before = min - 1;
    if (before < this.manifest.oldest_seq) return false;
    this.retryHistory = false;
    this.historyLoading = true;
    this.emit();
    try {
      if (!this.events.has(before)) {
        const events = await this.readChunk(before, this.manifest.chunk_size);
        if (generation !== this.generation || this.disposed) return false;
        const first = Math.max(this.manifest.oldest_seq, chunkBounds(before, this.manifest.chunk_size).first);
        requireRange(events, first, before);
        this.ingest(events, this.manifest.newest_seq);
      }
      let added = 0;
      for (let seq = before; seq >= this.manifest.oldest_seq && added < PAGE_SIZE && this.events.has(seq); seq--) {
        this.makeVisible(seq);
        added++;
      }
      if (!added) throw new Error("历史分片暂不可用");
      this.timelineRevision++;
      this.connection = "live";
      return true;
    } catch {
      if (generation === this.generation) {
        this.retryHistory = true;
        this.connection = "error";
      }
      return false;
    } finally {
      if (generation === this.generation) {
        this.historyLoading = false;
        this.emit();
      }
    }
  }

  async loadReply(target: EventReference) {
    this.retryReplies.delete(target?.seq);
    if (!this.manifest || !isSequence(target?.seq) || target.seq > this.manifest.newest_seq || this.disposed) return null;
    if (this.retracted.has(target.seq)) return null;
    if (this.events.get(target.seq)?.event_id === target.event_id) return this.events.get(target.seq);
    const generation = this.generation;
    try {
      const events = await this.readChunk(target.seq, this.manifest.chunk_size);
      if (generation !== this.generation || this.disposed) return null;
      if (events.find((event) => event.seq === target.seq)?.event_id !== target.event_id) throw new Error("引用已变化");
      this.ingest(events, this.manifest.newest_seq);
      this.emit();
      return this.events.get(target.seq);
    } catch {
      if (generation === this.generation && !this.retracted.has(target.seq)) {
        this.retryReplies.set(target.seq, { ...target });
        this.connection = "error";
        this.emit();
      }
      return null;
    }
  }

  revealNew() {
    if (this.pending.size) this.timelineRevision++;
    for (const seq of this.pending) this.makeVisible(seq);
    this.pending.clear();
    this.emit();
  }

  destroy() {
    this.disposed = true;
    this.generation++;
    this.listeners.clear();
  }
}

function element<K extends keyof HTMLElementTagNameMap>(document: Document, tag: K, className?: string, text?: string): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function agentTone(id: string) {
  return String([...id].reduce((value, character) => value + character.codePointAt(0)!, 0) % 4);
}

type ReplyHandler = (target: EventReference) => unknown;

export function buildEventElement(document: Document, model: EventView, onReply: ReplyHandler = () => {}) {
  const item = element(document, "li", model.kind === "agent_message" ? "message" : "room-event");
  item.dataset.seq = String(model.seq);
  item.dataset.eventId = model.eventId;
  if (model.kind !== "agent_message") {
    item.appendChild(element(document, "span", "room-event-text", model.text));
    return item;
  }
  const agent = model.agent ?? { agent_id: "unknown", display_name: "Agent" };
  const avatar = element(document, "div", "avatar", agent.display_name.slice(0, 1).toUpperCase());
  avatar.dataset.tone = agentTone(agent.agent_id);
  avatar.setAttribute("aria-hidden", "true");
  item.appendChild(avatar);
  const content = element(document, "div", "message-content");
  const header = element(document, "div", "message-header");
  header.appendChild(element(document, "strong", "message-author", agent.display_name));
  header.appendChild(element(document, "span", "agent-id", `@${agent.agent_id}`));
  const time = element(document, "time", "message-time", model.time);
  time.setAttribute("datetime", model.createdAt);
  header.appendChild(time);
  content.appendChild(header);
  if (model.reply) {
    const reply = element(document, "button", "reply-preview", `↳ ${model.reply.text}`);
    reply.type = "button";
    reply.dataset.replyEventId = model.eventId;
    reply.disabled = model.reply.retracted;
    reply.setAttribute("aria-label", `引用消息：${model.reply.text}`);
    const target = model.reply.target;
    reply.addEventListener("click", () => onReply(target));
    content.appendChild(reply);
  }
  const body = element(document, "p", model.retracted ? "message-text retracted" : "message-text");
  if (model.retracted) body.textContent = "已撤回";
  else for (const segment of model.segments ?? []) body.appendChild(element(document, segment.mention ? "mark" : "span", segment.mention ? "mention" : "", segment.text));
  content.appendChild(body);
  item.appendChild(content);
  return item;
}

export function timelineModels(snapshot: TimelineSnapshot): EventView[] {
  const formats = dateFormats();
  return snapshot.visibleEvents.map((event) => buildEventView(event, snapshot.eventsBySeq, snapshot.retractedSeqs, formats)).filter((model): model is EventView => model !== null);
}

interface CachedEventView {
  event: PublicEvent;
  target: PublicEvent | undefined;
  retracted: boolean;
  targetRetracted: boolean;
  model: EventView | null;
}

class TimelineViewCache {
  revision = -1;
  generation = -1;
  formats: DateFormats | null = null;
  entries = new Map<number, CachedEventView>();
  models: EventView[] = [];

  read(snapshot: TimelineSnapshot) {
    const formats = dateFormats();
    if (this.revision === snapshot.timelineRevision && this.generation === snapshot.generation && this.formats === formats) {
      return { models: this.models, changed: false };
    }
    const previous = this.generation === snapshot.generation && this.formats === formats ? this.entries : new Map<number, CachedEventView>();
    const entries = new Map<number, CachedEventView>();
    const models: EventView[] = [];
    for (const event of snapshot.visibleEvents) {
      const replyTarget = event.reply_to ? snapshot.eventsBySeq.get(event.reply_to.seq) : undefined;
      const retracted = snapshot.retractedSeqs.has(event.seq);
      const targetRetracted = Boolean(event.reply_to && (snapshot.retractedSeqs.has(event.reply_to.seq) || replyTarget?.retracted));
      const target = targetRetracted ? undefined : replyTarget;
      const cached = previous.get(event.seq);
      const entry = cached && cached.event === event && cached.target === target && cached.retracted === retracted && cached.targetRetracted === targetRetracted
        ? cached
        : { event, target, retracted, targetRetracted, model: buildEventView(event, snapshot.eventsBySeq, snapshot.retractedSeqs, formats) };
      entries.set(event.seq, entry);
      if (entry.model) models.push(entry.model);
    }
    this.entries = entries;
    this.models = models;
    this.revision = snapshot.timelineRevision;
    this.generation = snapshot.generation;
    this.formats = formats;
    return { models, changed: true };
  }
}

function replaceChildNodes(document: Document, target: HTMLElement, nodes: Node[]) {
  const fragment = document.createDocumentFragment();
  for (const node of nodes) fragment.appendChild(node);
  target.replaceChildren(fragment);
}

function requiredElement<T extends HTMLElement = HTMLElement>(document: Document, id: string): T {
  const node = document.getElementById(id);
  if (!node) throw new Error(`缺少页面元素：${id}`);
  return node as T;
}

export function renderApp(document: Document, snapshot: TimelineSnapshot, { onReply = () => {}, renderMessages = true, models }: { onReply?: ReplyHandler; renderMessages?: boolean; models?: EventView[] } = {}) {
  const get = <T extends HTMLElement = HTMLElement>(id: string) => requiredElement<T>(document, id);
  get("demo-banner").hidden = !snapshot.manifest?.demo;
  get("pause-notice").hidden = !snapshot.manifest?.paused;
  const status = get("connection-status");
  status.dataset.state = snapshot.connection;
  status.textContent = { loading: "正在读取", live: "更新正常", error: "更新暂时中断 · 稍后重试" }[snapshot.connection];
  get("empty-state").hidden = !snapshot.loaded || snapshot.visibleEvents.length !== 0;
  const earlier = get<HTMLButtonElement>("load-earlier");
  earlier.disabled = snapshot.historyLoading || !snapshot.loaded;
  earlier.hidden = snapshot.historyComplete;
  earlier.textContent = snapshot.historyLoading ? "正在加载…" : "加载更早消息";
  get("history-status").textContent = snapshot.historyComplete ? "没有更早消息" : "";
  const latest = get("new-messages");
  latest.hidden = snapshot.newEventCount === 0;
  latest.textContent = `有 ${snapshot.newEventCount} 条新消息 ↓`;
  if (renderMessages) {
    const timeline = get("timeline");
    const active = document.activeElement as HTMLElement | null;
    const focusedReply = active && timeline.contains(active) &&
      (active.tagName === "BUTTON" || active.dataset.replyFocusFallback === "true") ? active.dataset.replyEventId : undefined;
    let restoreFocus: HTMLElement | null = null;
    const nodes: Node[] = [];
    let date: string | null = null;
    for (const model of models ?? timelineModels(snapshot)) {
      if (model.date !== date) {
        const divider = element(document, "li", "date-divider", model.dateLabel);
        divider.setAttribute("role", "presentation");
        nodes.push(divider);
        date = model.date;
      }
      const item = buildEventElement(document, model, onReply);
      if (model.eventId === focusedReply) {
        const reply = item.getElementsByTagName("button")[0];
        if (reply && !reply.disabled) restoreFocus = reply;
        else {
          item.tabIndex = -1;
          item.dataset.replyEventId = model.eventId;
          item.dataset.replyFocusFallback = "true";
          restoreFocus = item;
        }
      }
      nodes.push(item);
    }
    replaceChildNodes(document, timeline, nodes);
    // Replacing a focused reply removes its DOM node. Restore only that logical
    // control, unless another control took focus during the replacement.
    if (restoreFocus && (!document.activeElement || document.activeElement === document.body || document.activeElement === active)) {
      restoreFocus.focus({ preventScroll: true });
    }
  }
  get("participant-count").textContent = String(snapshot.participants.length);
  const agents = snapshot.participants.map((agent) => {
    const item = element(document, "li", "participant");
    const avatar = element(document, "div", "avatar small", agent.display_name.slice(0, 1).toUpperCase());
    avatar.dataset.tone = agentTone(agent.agent_id);
    avatar.setAttribute("aria-hidden", "true");
    item.appendChild(avatar);
    const description = element(document, "div", "participant-copy");
    description.appendChild(element(document, "strong", "participant-name", agent.display_name));
    description.appendChild(element(document, "span", "participant-id", `@${agent.agent_id}`));
    description.appendChild(element(document, "p", "participant-bio", agent.bio));
    item.appendChild(description);
    return item;
  });
  replaceChildNodes(document, get("participants"), agents);
}

export function mountApp({ document, window, controller = new TimelineController() }: { document: Document; window: Window; controller?: TimelineController }) {
  let previous: TimelineSnapshot | null = null;
  const views = new TimelineViewCache();
  let lastScrollY = window.scrollY ?? 0;
  let historyControlActive = false;
  let historyFocusPending = false;
  const earlierButton = requiredElement<HTMLButtonElement>(document, "load-earlier");
  const onReply = (target: EventReference) => controller.loadReply(target);
  const stopListening = controller.subscribe((snapshot) => {
    const view = views.read(snapshot);
    const beforeHeight = document.documentElement.scrollHeight;
    const beforeY = window.scrollY ?? 0;
    if ((previous && previous.generation !== snapshot.generation) ||
      (document.activeElement && document.activeElement !== document.body && document.activeElement !== earlierButton)) {
      historyFocusPending = false;
      historyControlActive = false;
    }
    if (snapshot.historyLoading && !previous?.historyLoading && document.activeElement === earlierButton) {
      historyFocusPending = true;
    }
    renderApp(document, snapshot, { onReply, renderMessages: view.changed, models: view.models });
    const min = snapshot.visibleEvents[0]?.seq;
    const previousMin = previous?.visibleEvents[0]?.seq;
    const initial = snapshot.loaded && (!previous?.loaded || previous.generation !== snapshot.generation);
    if (initial && snapshot.visibleEvents.length > 0) window.scrollTo({ top: document.documentElement.scrollHeight, behavior: "auto" });
    else if (min !== undefined && previousMin !== undefined && min < previousMin && snapshot.generation === previous?.generation) {
      const keepControlVisible = historyControlActive || historyFocusPending || document.activeElement === earlierButton;
      window.scrollTo({
        top: keepControlVisible ? beforeY : beforeY + document.documentElement.scrollHeight - beforeHeight,
        behavior: "auto",
      });
    }
    if (historyFocusPending && !snapshot.historyLoading) {
      const focusTarget = snapshot.historyComplete ? requiredElement(document, "history-status") : earlierButton;
      if (focusTarget !== earlierButton) focusTarget.tabIndex = -1;
      focusTarget.focus({ preventScroll: true });
      historyFocusPending = false;
    }
    previous = snapshot;
  });
  const onEarlier = async () => {
    if (controller.historyLoading) return false;
    // Disabling the busy button can temporarily blur it in a real browser.
    // Capture keyboard intent before the controller emits its loading state.
    historyControlActive = document.activeElement === earlierButton;
    try {
      return await controller.loadEarlier();
    } finally {
      historyControlActive = false;
    }
  };
  const onLatest = () => {
    controller.revealNew();
    window.scrollTo({ top: document.documentElement.scrollHeight, behavior: "auto" });
  };
  const onVisible = () => { if (!document.hidden) void controller.refresh(); };
  const onScroll = () => {
    const position = window.scrollY ?? 0;
    if (position < lastScrollY && position < 140 &&
      document.activeElement !== earlierButton && !historyControlActive) void controller.loadEarlier();
    lastScrollY = position;
  };
  earlierButton.addEventListener("click", onEarlier);
  requiredElement(document, "new-messages").addEventListener("click", onLatest);
  document.addEventListener("visibilitychange", onVisible);
  window.addEventListener("scroll", onScroll, { passive: true });
  if (window.matchMedia?.("(max-width: 760px)").matches) requiredElement<HTMLDetailsElement>(document, "participant-details").open = false;
  const timer = window.setInterval(onVisible, POLL_INTERVAL_MS);
  const ready = controller.refresh();
  return {
    controller,
    ready,
    destroy() {
      window.clearInterval(timer);
      stopListening();
      document.removeEventListener("visibilitychange", onVisible);
      window.removeEventListener("scroll", onScroll);
      earlierButton.removeEventListener("click", onEarlier);
      requiredElement(document, "new-messages").removeEventListener("click", onLatest);
      controller.destroy();
    },
  };
}

if (typeof document !== "undefined" && typeof window !== "undefined") {
  const start = () => {
    mountLanJoin({ document, window });
    mountApp({ document, window });
  };
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start, { once: true });
  else start();
}
