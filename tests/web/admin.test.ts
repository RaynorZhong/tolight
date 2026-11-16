import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import { AdminApiError, AdminController, ADMIN_POLL_INTERVAL_MS, mountAdmin, requestAdmin } from "../../frontend/admin.ts";
import type { ActiveAgent, AdminRequest, PendingAgent } from "../../frontend/admin.ts";

function agent(extra: Partial<PendingAgent> = {}): PendingAgent {
  return {
    agent_id: "moon", application_id: "00000000-0000-4000-8000-000000000001", application_revision: 1,
    display_name: "Moon", bio: "独立写客户端的 Agent", owner_contact: "private-owner@example.invalid",
    framework_label: "Hermes", public_key: "A".repeat(43),
    registered_at: "2026-11-16T03:00:00Z", expires_at: "2026-11-30T03:00:00Z", last_seen_at: "2026-11-16T03:05:00Z",
    conformance: { passed_at: "2026-11-16T03:03:00Z", suite_version: "1", last_run: { passed: true, failure_reason: null } },
    can_approve: true, ...extra,
  };
}
function pending(agents: PendingAgent[] = [agent()]) {
  return { schema: "tolight-admin-pending/1", csrf_token: "local-test-csrf", server_time: "2026-11-16T03:05:00Z", agents };
}
function activeAgent(extra: Partial<ActiveAgent> = {}): ActiveAgent {
  const { expires_at: _expires, conformance: _conformance, can_approve: _canApprove, ...fields } = agent();
  return {
    ...fields, agent_id: "active-moon", application_id: "00000000-0000-4000-8000-000000000002", display_name: "Active Moon",
    joined_at: "2026-11-16T03:04:00Z", muted_until: null, muted_indefinitely: false, mute_reason: null,
    muted: false, presence: "online", last_seen_age_seconds: 0, ...extra,
  };
}
function active(agents: ActiveAgent[] = [], extra: Record<string, unknown> = {}) {
  return { schema: "tolight-admin-agents/1", csrf_token: "local-test-csrf", server_time: "2026-11-16T03:05:00Z", online_threshold_seconds: 120, agents, ...extra };
}
// Existing review scenarios have an empty active list; their request counters
// remain scoped to the pending/review operations they are intended to exercise.
class ReviewController extends AdminController {
  constructor({ request }: { request: AdminRequest }) {
    super({ request: (path, options) => path === "./api/agents" ? Promise.resolve(active()) : request(path, options) });
  }
}

function defer<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

test("pending application data is validated and copied without exposing mutable controller state", async () => {
  const payload = pending();
  const controller = new ReviewController({ request: async (path) => {
    assert.equal(path, "./api/pending");
    return payload;
  } });
  assert.equal(await controller.refresh(), true);
  payload.agents[0].bio = "external change";
  const snapshot = controller.snapshot();
  assert.equal(snapshot.connection, "ready");
  assert.equal(snapshot.agents[0].owner_contact, "private-owner@example.invalid");
  snapshot.agents[0].application_revision = 100;
  assert.equal(controller.snapshot().agents[0].application_revision, 1);
  assert.equal(controller.snapshot().agents[0].bio, "独立写客户端的 Agent");
});

test("invalid or duplicate pending records cannot enable review", async () => {
  for (const payload of [null, {}, { ...pending(), csrf_token: "" }, pending([agent(), agent()]), pending([{ ...agent(), conformance: null } as unknown as PendingAgent])]) {
    const controller = new ReviewController({ request: async () => payload });
    assert.equal(await controller.refresh(), false);
    assert.equal(controller.snapshot().connection, "error");
    assert.equal(controller.openDecision("moon", "approve"), false);
  }
});

test("approval requires passed sandbox while rejection and cancellation do not write", async () => {
  let writes = 0;
  const controller = new ReviewController({ request: async (_path, options) => {
    if (options?.method) writes++;
    return pending([agent({ can_approve: false, conformance: { passed_at: null, suite_version: null, last_run: null } })]);
  } });
  await controller.refresh();
  assert.equal(controller.openDecision("moon", "approve"), false);
  assert.equal(controller.openDecision("moon", "reject"), true);
  assert.equal(writes, 0);
  controller.cancelDecision();
  assert.equal(controller.snapshot().selection, null);
  assert.equal(writes, 0);
});

test("explicit approval sends exactly the reviewed application basis and CSRF token", async () => {
  const writes: RequestInit[] = [];
  const controller = new ReviewController({ request: async (path, options) => {
    if (path === "./api/pending") return pending(writes.length ? [] : [agent()]);
    assert.equal(path, "./api/review");
    writes.push(options!);
    return { agent_id: "moon", state: "ACTIVE" };
  } });
  await controller.refresh();
  assert.equal(controller.openDecision("moon", "approve"), true);
  assert.equal(writes.length, 0);
  assert.equal(controller.snapshot().selection?.agent.owner_contact, "private-owner@example.invalid");
  assert.equal(await controller.submit(), true);
  assert.equal(writes.length, 1);
  assert.equal(writes[0].method, "POST");
  assert.deepEqual(writes[0].headers, { "Content-Type": "application/json", "X-Tolight-CSRF": "local-test-csrf" });
  assert.deepEqual(JSON.parse(String(writes[0].body)), {
    agent_id: "moon", application_id: agent().application_id, application_revision: 1, public_key: agent().public_key, action: "approve",
  });
  assert.equal(controller.snapshot().agents.length, 0);
  assert.equal(controller.snapshot().selection, null);
  assert.match(controller.snapshot().status, /已批准 Moon/u);
});

test("rejection submits only an existing reason code after confirmation", async () => {
  let sent: Record<string, unknown> | null = null;
  const controller = new ReviewController({ request: async (path, options) => {
    if (path === "./api/pending") return pending(sent ? [] : [agent()]);
    sent = JSON.parse(String(options?.body));
    return { agent_id: "moon", state: "REJECTED" };
  } });
  await controller.refresh();
  controller.openDecision("moon", "reject");
  controller.setReason("bio_inadequate");
  controller.setReason("invented-reason");
  assert.equal(await controller.submit(), true);
  assert.equal((sent as Record<string, unknown> | null)?.reason, "bio_inadequate");
  assert.equal((sent as Record<string, unknown> | null)?.action, "reject");
  assert.match(controller.snapshot().status, /已拒绝/u);
});

test("a pending write prevents duplicate submit, cancel, refresh and polling", async () => {
  const write = defer<unknown>();
  let gets = 0;
  let writes = 0;
  const controller = new ReviewController({ request: async (path) => {
    if (path === "./api/pending") { gets++; return pending(writes ? [] : [agent()]); }
    writes++;
    return write.promise;
  } });
  await controller.refresh();
  controller.openDecision("moon", "approve");
  const first = controller.submit();
  assert.equal(await controller.submit(), false);
  controller.cancelDecision();
  assert.notEqual(controller.snapshot().selection, null);
  assert.equal(await controller.refresh(), false);
  assert.equal(await controller.poll(false), false);
  assert.equal(gets, 1);
  assert.equal(writes, 1);
  write.resolve({ agent_id: "moon", state: "ACTIVE" });
  assert.equal(await first, true);
  assert.equal(gets, 2);
});

test("late background refresh cannot replace an open reviewed snapshot and stale decisions reload", async () => {
  const held = defer<unknown>();
  let gets = 0;
  const bodies: Record<string, unknown>[] = [];
  const controller = new ReviewController({ request: async (path, options) => {
    if (path === "./api/review") {
      bodies.push(JSON.parse(String(options?.body)));
      throw new AdminApiError("review_changed");
    }
    gets++;
    if (gets === 2) return held.promise;
    return pending([agent({ application_revision: gets === 1 ? 1 : 2 })]);
  } });
  await controller.refresh();
  const background = controller.refresh();
  assert.equal(controller.openDecision("moon", "approve"), true);
  held.resolve(pending([agent({ application_revision: 2, bio: "new application" })]));
  assert.equal(await background, false);
  assert.equal(controller.snapshot().selection?.agent.application_revision, 1);
  assert.equal(controller.snapshot().selection?.agent.bio, agent().bio);
  assert.equal(await controller.submit(), false);
  assert.equal(bodies.length, 1);
  assert.equal(bodies[0].application_revision, 1);
  assert.equal(controller.snapshot().agents[0].application_revision, 2);
  assert.equal(controller.snapshot().selection, null);
  assert.match(controller.snapshot().status, /状态已变化/u);
});

test("hidden pages and open confirmation skip polling without changing its snapshot", async () => {
  let gets = 0;
  const controller = new ReviewController({ request: async () => { gets++; return pending(); } });
  await controller.refresh();
  assert.equal(await controller.poll(true), false);
  controller.openDecision("moon", "reject");
  assert.equal(await controller.poll(false), false);
  assert.equal(controller.openDecision("moon", "approve"), false);
  assert.equal(gets, 1);
  controller.cancelDecision();
  assert.equal(await controller.poll(false), true);
  assert.equal(gets, 2);
});

test("cancelling a confirmation lets a new read proceed while the abandoned older read stays ignored", async () => {
  const old = defer<unknown>();
  let gets = 0;
  const controller = new ReviewController({ request: async () => {
    gets++;
    if (gets === 2) return old.promise;
    return pending([agent({ application_revision: gets })]);
  } });
  await controller.refresh();
  const abandoned = controller.refresh();
  controller.openDecision("moon", "approve");
  controller.cancelDecision();
  assert.equal(await controller.refresh(), true);
  assert.equal(controller.snapshot().agents[0].application_revision, 3);
  old.resolve(pending([agent({ application_revision: 2 })]));
  assert.equal(await abandoned, false);
  assert.equal(controller.snapshot().agents[0].application_revision, 3);
  assert.equal(controller.snapshot().loading, false);
});

test("unknown write outcome is never retried and reloads the current pending state", async () => {
  let writes = 0;
  const controller = new ReviewController({ request: async (path) => {
    if (path === "./api/pending") return pending(writes ? [] : [agent()]);
    writes++;
    throw new Error("response lost after commit");
  } });
  await controller.refresh();
  controller.openDecision("moon", "approve");
  assert.equal(await controller.submit(), false);
  assert.equal(writes, 1);
  assert.deepEqual(controller.snapshot().agents, []);
  assert.equal(controller.snapshot().selection, null);
  assert.match(controller.snapshot().status, /结果暂未确认/u);
  assert.equal(await controller.submit(), false);
  assert.equal(writes, 1);
});

test("failed reads disable decisions on stale records and destroyed controllers ignore late reads", async () => {
  let fail = false;
  const controller = new ReviewController({ request: async () => {
    if (fail) throw new Error("offline");
    return pending();
  } });
  await controller.refresh();
  fail = true;
  assert.equal(await controller.refresh(), false);
  assert.equal(controller.snapshot().agents.length, 1);
  assert.equal(controller.openDecision("moon", "approve"), false);
  const held = defer<unknown>();
  const late = new ReviewController({ request: () => held.promise });
  const running = late.refresh();
  late.destroy();
  held.resolve(pending());
  assert.equal(await running, false);
  assert.equal(late.snapshot().agents.length, 0);
  assert.equal(await late.poll(false), false);
});

test("the network adapter prohibits redirects and uses same-origin no-store with no write retries", async () => {
  const original = globalThis.fetch;
  const requests: RequestInit[] = [];
  globalThis.fetch = async (_input, options) => {
    requests.push(options!);
    return new Response(JSON.stringify({ error: { code: "csrf_failed" } }), { status: 403 });
  };
  try {
    await assert.rejects(() => requestAdmin("./api/review", { method: "POST", body: "{}" }), (error) => error instanceof AdminApiError && error.code === "csrf_failed");
    assert.equal(requests.length, 1);
    assert.equal(requests[0].cache, "no-store");
    assert.equal(requests[0].redirect, "error");
    assert.equal(requests[0].credentials, "same-origin");
  } finally { globalThis.fetch = original; }
});

class DomNode {
  children: DomNode[] = [];
  dataset: Record<string, string> = {};
  listeners = new Map<string, Set<() => unknown>>();
  className = "";
  hidden = false;
  open = false;
  disabled = false;
  type = "";
  value = "";
  _text = "";
  onFocus: (() => void) | null = null;
  set textContent(value: string) { this._text = value; this.children = []; }
  get textContent(): string { return this._text + this.children.map((child) => child.textContent).join(""); }
  appendChild(child: DomNode) { this.children.push(child); return child; }
  replaceChildren(...children: DomNode[]) { this.children = children; this._text = ""; }
  setAttribute(_name: string, _value: string) {}
  addEventListener(type: string, listener: () => unknown) {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type)!.add(listener);
  }
  removeEventListener(type: string, listener: () => unknown) { this.listeners.get(type)?.delete(listener); }
  focus(_options?: FocusOptions) { this.onFocus?.(); }
  contains(child: DomNode | null): boolean { return child !== null && (this === child || this.children.some((node) => node.contains(child))); }
  async dispatch(type: string) { await Promise.all([...this.listeners.get(type) ?? []].map((listener) => listener())); }
}
function adminDom(controller: AdminController) {
  const elements = new Map(["refresh", "review-confirm", "review-cancel", "review-reason", "pending-count", "status", "updated-at", "empty-state", "review-panel", "pending-list", "review-title", "review-details", "review-description", "review-reason-field", "review-reason-label", "review-mute-field", "review-mute-duration", "active-count", "active-list", "active-empty-state", "presence-note"].map((id) => [id, new DomNode()]));
  const document = Object.assign(new DomNode(), {
    hidden: false,
    activeElement: null as DomNode | null,
    body: new DomNode(),
    getElementById(id: string) { assert.ok(elements.has(id), id); return elements.get(id)!; },
    createElement(_tag: string) { const result = new DomNode(); result.onFocus = () => { document.activeElement = result; }; return result; },
  });
  for (const element of elements.values()) element.onFocus = () => { document.activeElement = element; };
  elements.get("review-panel")!.appendChild(elements.get("review-title")!);
  elements.get("review-panel")!.appendChild(elements.get("review-confirm")!);
  elements.get("review-panel")!.appendChild(elements.get("review-cancel")!);
  let tick: (() => unknown) | null = null;
  const window = {
    setInterval(callback: () => unknown, interval: number) { assert.equal(interval, ADMIN_POLL_INTERVAL_MS); tick = callback; return 1; },
    clearInterval() { tick = null; },
  };
  const mounted = mountAdmin({ document: document as unknown as Document, window: window as unknown as Window, controller });
  return { document, elements, mounted, tick() { tick?.(); }, get timerActive() { return tick !== null; } };
}

test("the private page renders contact and hostile text literally in both list and confirmation", async () => {
  const hostile = agent({ display_name: "<img src=x>", bio: "<script>hello</script>" });
  const controller = new ReviewController({ request: async () => pending([hostile]) });
  const dom = adminDom(controller);
  await dom.mounted.ready;
  const list = dom.elements.get("pending-list")!;
  assert.ok(list.textContent.includes(hostile.owner_contact));
  assert.ok(list.textContent.includes(hostile.display_name));
  assert.ok(list.textContent.includes(hostile.bio));
  controller.openDecision("moon", "approve");
  assert.equal(dom.elements.get("review-panel")!.hidden, false);
  assert.ok(dom.elements.get("review-details")!.textContent.includes(hostile.owner_contact));
  assert.ok(dom.elements.get("review-details")!.textContent.includes(hostile.public_key!));
  assert.ok(dom.elements.get("review-title")!.textContent.includes(hostile.display_name));
  dom.mounted.destroy();
  assert.equal(dom.timerActive, false);
});

test("the mounted poll skips hidden pages and active confirmation", async () => {
  let gets = 0;
  const controller = new ReviewController({ request: async () => { gets++; return pending(); } });
  const dom = adminDom(controller);
  await dom.mounted.ready;
  dom.document.hidden = true;
  dom.tick();
  assert.equal(gets, 1);
  dom.document.hidden = false;
  controller.openDecision("moon", "approve");
  dom.tick();
  assert.equal(gets, 1);
  controller.cancelDecision();
  dom.tick();
  assert.equal(gets, 2);
  dom.mounted.destroy();
});

test("cancelling returns keyboard focus to the reviewed agent and completing a decision focuses its result", async () => {
  let wrote = false;
  const controller = new ReviewController({ request: async (path) => {
    if (path === "./api/pending") return pending(wrote ? [] : [agent()]);
    wrote = true;
    return { agent_id: "moon", state: "ACTIVE" };
  } });
  const dom = adminDom(controller);
  await dom.mounted.ready;
  controller.openDecision("moon", "approve");
  dom.elements.get("review-cancel")!.focus();
  await dom.elements.get("review-cancel")!.dispatch("click");
  assert.equal(dom.document.activeElement?.textContent, "批准");
  controller.openDecision("moon", "approve");
  dom.elements.get("review-confirm")!.focus();
  await dom.elements.get("review-confirm")!.dispatch("click");
  assert.equal(dom.document.activeElement, dom.elements.get("status"));
  assert.match(dom.document.activeElement!.textContent, /已批准/u);
  dom.mounted.destroy();
});

test("operator UI resources are separate and use no unsafe DOM sinks or third-party resources", async () => {
  const html = await readFile(new URL("../../operator-ui/index.html", import.meta.url), "utf8");
  const source = await readFile(new URL("../../frontend/admin.ts", import.meta.url), "utf8");
  assert.match(html, /<script type="module" src="\.\/admin\.js"><\/script>/u);
  assert.doesNotMatch(html, /(?:src|href)\s*=\s*["']https?:\/\/|\bonclick\s*=|<form\b/iu);
  assert.doesNotMatch(source, /innerHTML|insertAdjacentHTML|localStorage|sessionStorage|\beval\(/u);
  assert.match(source, /textContent/u);
});

test("both lists publish atomically and mixed sessions or duplicate identities fail closed", async () => {
  let generation = 0;
  let held = defer<unknown>();
  const controller = new AdminController({ request: async (path) => {
    if (path === "./api/pending") return pending([agent({ application_revision: generation + 1 })]);
    if (generation === 0) return active([activeAgent()]);
    return held.promise;
  } });
  assert.equal(await controller.refresh(), true);
  generation++;
  const updating = controller.refresh();
  await Promise.resolve();
  assert.equal(controller.snapshot().agents[0].application_revision, 1);
  assert.equal(controller.snapshot().activeAgents[0].display_name, "Active Moon");
  held.reject(new Error("active list unavailable"));
  assert.equal(await updating, false);
  assert.equal(controller.snapshot().agents[0].application_revision, 1);
  assert.equal(controller.snapshot().activeAgents[0].presence, "unknown");
  assert.equal(controller.openDecision("active-moon", "mute"), false);
  for (const invalid of [active([], { csrf_token: "other-process" }), active([activeAgent({ agent_id: "moon" })])]) {
    const mixed = new AdminController({ request: async (path) => path === "./api/pending" ? pending() : invalid });
    assert.equal(await mixed.refresh(), false);
    assert.equal(mixed.snapshot().connection, "error");
    assert.deepEqual(mixed.snapshot().agents, []);
    assert.deepEqual(mixed.snapshot().activeAgents, []);
  }
});

test("active response validation rejects malformed or ambiguous control metadata", async () => {
  for (const invalid of [
    active([], { online_threshold_seconds: 0 }), active([], { server_time: "yesterday" }),
    active([activeAgent(), activeAgent()]), active([{ ...activeAgent(), presence: "probably" } as unknown as ActiveAgent]),
    active([activeAgent({ last_seen_age_seconds: -1 })]), active([activeAgent({ muted_until: "not-a-date" })]),
    active([{ ...activeAgent(), muted_indefinitely: undefined } as unknown as ActiveAgent]),
  ]) {
    const controller = new AdminController({ request: async (path) => path === "./api/pending" ? pending([]) : invalid });
    assert.equal(await controller.refresh(), false);
    assert.equal(controller.openDecision("active-moon", "kick"), false);
  }
});

test("activity ages against server time and never/future or stale observations stay unknown", async () => {
  let time = 0;
  let offline = false;
  const controller = new AdminController({ now: () => time, request: async (path) => {
    if (offline) throw new Error("offline");
    return path === "./api/pending" ? pending([]) : active([
      activeAgent({ last_seen_age_seconds: 120, last_seen_at: "2026-11-16T03:03:00Z" }),
      activeAgent({ agent_id: "never", application_id: "never", presence: "unknown", last_seen_at: null, last_seen_age_seconds: null }),
      activeAgent({ agent_id: "future", application_id: "future", presence: "unknown", last_seen_at: "2026-11-16T04:00:00Z", last_seen_age_seconds: null }),
    ]);
  } });
  await controller.refresh();
  assert.deepEqual(controller.snapshot().activeAgents.map((a) => a.presence), ["online", "unknown", "unknown"]);
  time += 1000;
  assert.deepEqual(controller.snapshot().activeAgents.map((a) => a.presence), ["offline", "unknown", "unknown"]);
  offline = true;
  await controller.refresh();
  assert.deepEqual(controller.snapshot().activeAgents.map((a) => a.presence), ["unknown", "unknown", "unknown"]);
  assert.match(controller.snapshot().status, /在线状态待更新/u);
});

test("expired finite mutes clear in the view and cannot open an unmute decision", async () => {
  let time = 0;
  const controller = new AdminController({ now: () => time, request: async (path) => path === "./api/pending" ? pending([]) : active([
    activeAgent({ muted: true, muted_until: "2026-11-16T03:05:01Z", mute_reason: "spam" }),
    activeAgent({ agent_id: "indefinite", application_id: "indefinite", muted: true, muted_indefinitely: true, mute_reason: "runaway" }),
  ]) });
  await controller.refresh();
  assert.equal(controller.snapshot().activeAgents[0].muted, true);
  time = 1000;
  assert.equal(controller.snapshot().activeAgents[0].muted, false);
  assert.equal(controller.openDecision("active-moon", "unmute"), false);
  assert.equal(controller.openDecision("indefinite", "unmute"), true);
  assert.equal(controller.snapshot().selection?.agent.agent_id, "indefinite");
});

test("mute confirmation submits the viewed raw control basis and only a selected duration", async () => {
  const before = activeAgent({ muted_until: "2026-11-16T03:04:00Z", mute_reason: "other" });
  let sent: Record<string, unknown> | null = null;
  let endpoint = "";
  const controller = new AdminController({ request: async (path, options) => {
    if (path === "./api/pending") return pending([]);
    if (path === "./api/agents") return active([sent ? activeAgent({ muted: true, muted_indefinitely: true, mute_reason: "spam" }) : before]);
    endpoint = path;
    sent = JSON.parse(String(options?.body));
    assert.equal((options?.headers as Record<string, string>)["X-Tolight-CSRF"], "local-test-csrf");
    assert.equal(options?.method, "POST");
    return { agent_id: before.agent_id, state: "ACTIVE", muted_until: null, muted_indefinitely: true, mute_reason: "spam" };
  } });
  await controller.refresh();
  assert.equal(controller.openDecision(before.agent_id, "mute"), true);
  assert.equal(controller.snapshot().selection?.muteSeconds, 1800);
  assert.deepEqual(controller.snapshot().activeAgents.map(a => a.presence), ["unknown"]);
  controller.setMuteSeconds(1);
  assert.equal(controller.snapshot().selection?.muteSeconds, 1800);
  controller.setMuteSeconds(null);
  controller.setReason("spam");
  assert.equal(await controller.submit(), true);
  assert.equal(endpoint, "./api/control");
  assert.deepEqual(sent, {
    agent_id: before.agent_id, application_id: before.application_id, application_revision: before.application_revision,
    public_key: before.public_key, action: "mute", reason: "spam", muted_until: before.muted_until,
    muted_indefinitely: false, mute_reason: "other", mute_seconds: null,
  });
  assert.match(controller.snapshot().status, /已禁言 Active Moon/u);
  assert.equal(controller.snapshot().activeAgents[0].muted_indefinitely, true);
});

test("unmute and kick use the frozen control basis and omit unrelated fields", async () => {
  for (const action of ["unmute", "kick"] as const) {
    const before = activeAgent({ muted: true, muted_indefinitely: true, mute_reason: "runaway" });
    let body: Record<string, unknown> | null = null;
    const controller = new AdminController({ request: async (path, options) => {
      if (path === "./api/pending") return pending([]);
      if (path === "./api/agents") return active(body ? action === "kick" ? [] : [activeAgent()] : [before]);
      body = JSON.parse(String(options?.body));
      return { agent_id: before.agent_id, state: action === "kick" ? "LEFT" : "ACTIVE", muted_until: null, muted_indefinitely: false, mute_reason: null };
    } });
    await controller.refresh();
    assert.equal(controller.openDecision(before.agent_id, action), true);
    controller.setReason("owner_request");
    assert.equal(await controller.submit(), true);
    assert.equal((body as Record<string, unknown> | null)?.action, action);
    assert.equal((body as Record<string, unknown> | null)?.muted_indefinitely, true);
    assert.equal((body as Record<string, unknown> | null)?.mute_reason, "runaway");
    assert.equal(Object.hasOwn(body!, "mute_seconds"), false);
    assert.equal(Object.hasOwn(body!, "reason"), action === "kick");
    if (action === "kick") assert.deepEqual(controller.snapshot().activeAgents, []);
  }
});

test("control submissions never repeat and refresh both lists after uncertain or stale results", async () => {
  for (const error of [new Error("response lost"), new AdminApiError("review_changed")]) {
    const write = defer<unknown>();
    const calls: string[] = [];
    const controller = new AdminController({ request: async (path) => {
      calls.push(path);
      if (path === "./api/pending") return pending([]);
      if (path === "./api/agents") return active([activeAgent()]);
      return write.promise;
    } });
    await controller.refresh();
    controller.openDecision("active-moon", "kick");
    const submitting = controller.submit();
    assert.equal(await controller.submit(), false);
    controller.cancelDecision();
    assert.notEqual(controller.snapshot().selection, null);
    assert.equal(await controller.poll(false), false);
    write.reject(error);
    assert.equal(await submitting, false);
    assert.equal(calls.filter(path => path === "./api/control").length, 1);
    assert.equal(calls.filter(path => path === "./api/pending").length, 2);
    assert.equal(calls.filter(path => path === "./api/agents").length, 2);
    assert.equal(controller.snapshot().selection, null);
    assert.equal(await controller.submit(), false);
  }
});

test("an active-list refresh cannot overwrite a frozen mute snapshot", async () => {
  const held = defer<unknown>();
  let activeReads = 0;
  const controller = new AdminController({ request: async (path) => {
    if (path === "./api/pending") return pending([]);
    activeReads++;
    return activeReads === 2 ? held.promise : active([activeAgent()]);
  } });
  await controller.refresh();
  const staleRead = controller.refresh();
  controller.openDecision("active-moon", "mute");
  held.resolve(active([activeAgent({ muted: true, muted_indefinitely: true, mute_reason: "spam" })]));
  assert.equal(await staleRead, false);
  const selection = controller.snapshot().selection!;
  assert.equal((selection.agent as ActiveAgent).muted_indefinitely, false);
  assert.equal((selection.agent as ActiveAgent).mute_reason, null);
  controller.cancelDecision();
});

test("mounted active cards keep private details collapsed and controls require explicit confirmation", async () => {
  const controller = new AdminController({ request: async (path) => path === "./api/pending" ? pending([]) : active([activeAgent()]) });
  const dom = adminDom(controller);
  await dom.mounted.ready;
  const card = dom.elements.get("active-list")!.children[0];
  assert.ok(card.textContent.includes("Active Moon"));
  assert.ok(card.textContent.includes("在线"));
  assert.ok(card.textContent.includes("未禁言"));
  assert.equal(dom.elements.get("active-count")!.textContent, "1");
  assert.match(dom.elements.get("presence-note")!.textContent, /120 秒.*活动记录/u);
  const actions = card.children.find(node => node.className === "card-actions")!;
  assert.deepEqual(actions.children.map(node => [node.textContent, node.disabled]), [["禁言", false], ["解除禁言", true], ["踢出", false]]);
  await actions.children[0].dispatch("click");
  assert.equal(dom.elements.get("review-mute-field")!.hidden, false);
  assert.equal(dom.elements.get("review-mute-duration")!.value, "1800");
  assert.equal(dom.elements.get("review-reason-label")!.textContent, "禁言原因");
  assert.ok(dom.elements.get("review-details")!.textContent.includes(activeAgent().owner_contact));
  dom.elements.get("review-mute-duration")!.value = "300";
  await dom.elements.get("review-mute-duration")!.dispatch("change");
  assert.equal(controller.snapshot().selection?.muteSeconds, 300);
  await dom.elements.get("review-cancel")!.dispatch("click");
  assert.equal(dom.document.activeElement?.textContent, "禁言");
  const refreshedActions = dom.elements.get("active-list")!.children[0].children.find(node => node.className === "card-actions")!;
  await refreshedActions.children[2].dispatch("click");
  assert.match(dom.elements.get("review-description")!.textContent, /吊销现有密钥.*历史消息保留/u);
  assert.equal(dom.elements.get("review-mute-field")!.hidden, true);
  assert.equal(dom.elements.get("review-reason-label")!.textContent, "踢出原因");
  await dom.elements.get("review-cancel")!.dispatch("click");
  assert.equal(dom.document.activeElement?.textContent, "踢出");
  dom.mounted.destroy();
});

test("server offline stays offline at the truncated 120-second boundary and online ages precisely", async () => {
  let time = 0;
  const controller = new AdminController({ now: () => time, request: async (path) => path === "./api/pending" ? pending([]) : active([
    activeAgent({ agent_id: "precise-offline", presence: "offline", last_seen_age_seconds: 120, last_seen_at: "2026-11-16T03:02:59.999Z" }),
    activeAgent({ agent_id: "near-boundary", presence: "online", last_seen_age_seconds: 119, last_seen_at: "2026-11-16T03:03:00.001Z" }),
    activeAgent({ agent_id: "future-unknown", presence: "unknown", last_seen_age_seconds: 0, last_seen_at: "2026-11-16T03:06:00Z" }),
  ]) });
  await controller.refresh();
  assert.deepEqual(controller.snapshot().activeAgents.map(a => a.presence), ["offline", "online", "unknown"]);
  time = 2;
  assert.deepEqual(controller.snapshot().activeAgents.map(a => a.presence), ["offline", "offline", "unknown"]);
  time = 61000;
  assert.deepEqual(controller.snapshot().activeAgents.map(a => a.presence), ["offline", "offline", "unknown"]);
});

test("a contradictory mute result is treated as unknown and refreshed without a second write", async () => {
  for (const result of [
    { muted_until: null, muted_indefinitely: false, mute_reason: null },
    { muted_until: "bad-date", muted_indefinitely: false, mute_reason: "other" },
    { muted_until: "2026-11-16T03:35:00Z", muted_indefinitely: false, mute_reason: "spam" },
  ]) {
    let writes = 0;
    let activeReads = 0;
    const controller = new AdminController({ request: async (path) => {
      if (path === "./api/pending") return pending([]);
      if (path === "./api/agents") { activeReads++; return active([activeAgent()]); }
      writes++;
      return { agent_id: "active-moon", state: "ACTIVE", ...result };
    } });
    await controller.refresh();
    controller.openDecision("active-moon", "mute");
    assert.equal(await controller.submit(), false);
    assert.equal(writes, 1);
    assert.equal(activeReads, 2);
    assert.match(controller.snapshot().status, /结果暂未确认/u);
    assert.equal(controller.snapshot().selection, null);
  }
});

function activeProfile(dom: ReturnType<typeof adminDom>) {
  const profile = dom.elements.get("active-list")!.children[0].children.find(node => node.className === "profile-details");
  assert.ok(profile);
  return profile;
}

test("active profile expansion and summary keyboard focus survive both polling renders", async () => {
  const controller = new AdminController({ request: async (path) => path === "./api/pending" ? pending([]) : active([activeAgent()]) });
  const dom = adminDom(controller);
  await dom.mounted.ready;
  const original = activeProfile(dom);
  assert.equal(original.open, false);
  original.open = true;
  original.children[0].focus();
  await controller.refresh();
  const refreshed = activeProfile(dom);
  assert.notEqual(refreshed, original);
  assert.equal(refreshed.open, true);
  assert.equal(dom.document.activeElement, refreshed.children[0]);
  // Capturing native DOM state also preserves a subsequent user collapse.
  refreshed.open = false;
  await controller.refresh();
  assert.equal(activeProfile(dom).open, false);
  assert.equal(dom.document.activeElement, activeProfile(dom).children[0]);
  dom.mounted.destroy();
});

test("profile state belongs to the application identity and is not inherited by a replacement", async () => {
  let applicationId = activeAgent().application_id;
  const controller = new AdminController({ request: async (path) => path === "./api/pending" ? pending([]) : active([activeAgent({ application_id: applicationId })]) });
  const dom = adminDom(controller);
  await dom.mounted.ready;
  const previous = activeProfile(dom);
  previous.open = true;
  previous.children[0].focus();
  applicationId = "00000000-0000-4000-8000-000000000099";
  await controller.refresh();
  const replacement = activeProfile(dom);
  assert.equal(replacement.open, false);
  assert.notEqual(dom.document.activeElement, replacement.children[0]);
  assert.equal(dom.document.activeElement, dom.elements.get("status"));
  dom.mounted.destroy();
});

test("late profile refresh preserves expansion without taking focus moved by the operator", async () => {
  const held = defer<unknown>();
  let activeReads = 0;
  const controller = new AdminController({ request: async (path) => {
    if (path === "./api/pending") return pending([]);
    activeReads++;
    return activeReads === 2 ? held.promise : active([activeAgent()]);
  } });
  const dom = adminDom(controller);
  await dom.mounted.ready;
  const previous = activeProfile(dom);
  previous.open = true;
  previous.children[0].focus();
  const refreshing = controller.refresh();
  const status = dom.elements.get("status")!;
  status.focus();
  held.resolve(active([activeAgent({ last_seen_age_seconds: 2 })]));
  assert.equal(await refreshing, true);
  assert.equal(activeProfile(dom).open, true);
  assert.equal(dom.document.activeElement, status);
  dom.mounted.destroy();
});
