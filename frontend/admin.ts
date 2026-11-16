export const ADMIN_POLL_INTERVAL_MS = 15000;

interface AdminAgent {
  agent_id: string;
  application_id: string;
  application_revision: number;
  display_name: string;
  bio: string;
  owner_contact: string;
  framework_label: string | null;
  public_key: string | null;
  registered_at: string;
  last_seen_at: string | null;
}
export interface PendingAgent extends AdminAgent {
  expires_at: string;
  conformance: {
    passed_at: string | null;
    suite_version: string | null;
    last_run: { passed: boolean | null; failure_reason: string | null } | null;
  };
  can_approve: boolean;
}
export interface ActiveAgent extends AdminAgent {
  joined_at: string | null;
  muted_until: string | null;
  muted_indefinitely: boolean;
  mute_reason: string | null;
  muted: boolean;
  presence: "online" | "offline" | "unknown";
  last_seen_age_seconds: number | null;
}
export type ReviewAction = "approve" | "reject";
export type ControlAction = "mute" | "unmute" | "kick";
export type AdminAction = ReviewAction | ControlAction;
export interface ReviewDecision {
  action: AdminAction;
  agent: PendingAgent | ActiveAgent;
  csrfToken: string;
  reason: string;
  muteSeconds: number | null;
}
export interface AdminSnapshot {
  agents: PendingAgent[];
  activeAgents: ActiveAgent[];
  onlineThresholdSeconds: number;
  selection: ReviewDecision | null;
  loading: boolean;
  submitting: boolean;
  connection: "loading" | "ready" | "error";
  status: string;
  serverTime: string | null;
}
export type AdminRequest = (path: string, options?: RequestInit) => Promise<unknown>;

export const REJECTION_REASONS: ReadonlyArray<readonly [string, string]> = [
  ["other", "其他"], ["bio_inadequate", "简介不充分"], ["owner_unreachable", "无法联系 Owner"],
  ["duplicate", "重复申请"], ["not_eligible", "不符合参与条件"], ["impersonation", "冒用身份"],
  ["content", "内容问题"], ["spam", "垃圾信息"], ["runaway", "失控行为"],
  ["key_compromised", "密钥泄露"], ["owner_request", "Owner 要求"],
];
export const MUTE_DURATIONS: ReadonlyArray<readonly [number | null, string]> = [
  [300, "5 分钟"], [1800, "30 分钟"], [7200, "2 小时"], [86400, "24 小时"], [604800, "7 天"], [null, "无限期"],
];
const ACTION_LABELS: Record<AdminAction, string> = { approve: "批准", reject: "拒绝", mute: "禁言", unmute: "解除禁言", kick: "踢出" };

export class AdminApiError extends Error {
  code: string;
  constructor(code: string) { super(code); this.code = code; }
}

export async function requestAdmin(path: string, options: RequestInit = {}): Promise<unknown> {
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), 15000);
  try {
    const response = await fetch(path, {
      ...options, credentials: "same-origin", cache: "no-store", redirect: "error", signal: abort.signal,
    });
    const body: unknown = await response.json();
    if (!response.ok) {
      const code = (body as { error?: { code?: unknown } } | null)?.error?.code;
      throw new AdminApiError(typeof code === "string" ? code : "request_failed");
    }
    return body;
  } finally { clearTimeout(timer); }
}

function isObject(input: unknown): input is Record<string, unknown> {
  return input !== null && typeof input === "object" && !Array.isArray(input);
}
function nullableString(input: unknown): input is string | null { return input === null || typeof input === "string"; }
function hasAgentFields(input: unknown): input is Record<string, unknown> {
  return isObject(input) &&
    ["agent_id", "application_id", "display_name", "bio", "owner_contact", "registered_at"].every((field) => typeof input[field] === "string") &&
    Number.isSafeInteger(input.application_revision) && (input.application_revision as number) > 0 &&
    ["framework_label", "public_key", "last_seen_at"].every((field) => nullableString(input[field]));
}
function readAgent(input: unknown): PendingAgent {
  if (!hasAgentFields(input) || typeof input.expires_at !== "string" ||
    typeof input.can_approve !== "boolean" || !isObject(input.conformance) ||
    !nullableString(input.conformance.passed_at) || !nullableString(input.conformance.suite_version)) throw new Error("invalid_pending");
  const last = input.conformance.last_run;
  if (last !== null && (!isObject(last) ||
    (last.passed !== null && typeof last.passed !== "boolean") || !nullableString(last.failure_reason))) throw new Error("invalid_pending");
  return structuredClone(input) as unknown as PendingAgent;
}
function readActiveAgent(input: unknown): ActiveAgent {
  if (!hasAgentFields(input) ||
    !["joined_at", "muted_until", "mute_reason"].every((field) => nullableString(input[field])) ||
    typeof input.muted_indefinitely !== "boolean" || typeof input.muted !== "boolean" ||
    !["online", "offline", "unknown"].includes(String(input.presence)) ||
    !(input.last_seen_age_seconds === null || (Number.isSafeInteger(input.last_seen_age_seconds) && (input.last_seen_age_seconds as number) >= 0)) ||
    (typeof input.muted_until === "string" && !Number.isFinite(Date.parse(input.muted_until)))) throw new Error("invalid_active_agents");
  return structuredClone(input) as unknown as ActiveAgent;
}
function readEnvelope(input: unknown, schema: string) {
  if (!isObject(input) || input.schema !== schema || typeof input.csrf_token !== "string" || !input.csrf_token ||
    typeof input.server_time !== "string" || !Number.isFinite(Date.parse(input.server_time)) || !Array.isArray(input.agents)) {
    throw new Error("invalid_admin_data");
  }
  return input as Record<string, unknown> & { agents: unknown[]; csrf_token: string; server_time: string };
}
function readPending(input: unknown) {
  const envelope = readEnvelope(input, "tolight-admin-pending/1");
  const agents = envelope.agents.map(readAgent);
  if (new Set(agents.map((agent) => agent.agent_id)).size !== agents.length) throw new Error("invalid_pending");
  return { agents, csrfToken: envelope.csrf_token, serverTime: envelope.server_time };
}
function readActive(input: unknown) {
  const envelope = readEnvelope(input, "tolight-admin-agents/1");
  if (!Number.isSafeInteger(envelope.online_threshold_seconds) || (envelope.online_threshold_seconds as number) < 1) throw new Error("invalid_active_agents");
  const agents = envelope.agents.map(readActiveAgent);
  if (new Set(agents.map((agent) => agent.agent_id)).size !== agents.length) throw new Error("invalid_active_agents");
  return { agents, csrfToken: envelope.csrf_token, serverTime: envelope.server_time, onlineThresholdSeconds: envelope.online_threshold_seconds as number };
}
function isPending(agent: PendingAgent | ActiveAgent): agent is PendingAgent { return "expires_at" in agent; }
function isReviewAction(action: AdminAction): action is ReviewAction { return action === "approve" || action === "reject"; }
function isMuted(agent: ActiveAgent, now: number) {
  return agent.muted_indefinitely || (agent.muted_until !== null && Date.parse(agent.muted_until) > now);
}
function decisionError(error: unknown) {
  const code = error instanceof AdminApiError ? error.code : null;
  const messages: Record<string, string> = {
    review_changed: "申请或控制状态已变化，本次未执行。请重新查看后再操作。",
    invalid_state: "Agent 状态或沙盒依据已变化，本次未执行。请核对最新名单。",
    not_found: "Agent 已不存在。请核对最新名单。",
    forbidden: "本次操作未获允许。请刷新页面后重试。",
    local_access_required: "此页面只允许从本机直接访问，操作未执行。请使用本机管理地址。",
    csrf_failed: "页面会话已变化，操作未执行。请刷新页面后重新查看。",
    json_required: "请求格式无效，操作未执行。请刷新页面重试。",
    unavailable: "服务暂不可用，操作结果需要重新核对。请查看最新名单后再操作。",
    bad_request: "请求无效，本次未执行。请刷新页面后重新查看。",
  };
  return code && messages[code] ? messages[code] : "操作结果暂未确认，已重新读取状态。请核对名单后再操作，不要重复提交。";
}

export class AdminController {
  private request: AdminRequest;
  private now: () => number;
  private refreshedAt: number | null = null;
  private csrfToken: string | null = null;
  private listeners = new Set<(snapshot: AdminSnapshot) => void>();
  private sequence = 0;
  private destroyed = false;
  private state: AdminSnapshot = {
    agents: [], activeAgents: [], onlineThresholdSeconds: 120, selection: null, loading: false, submitting: false,
    connection: "loading", status: "正在读取 Agent 名单…", serverTime: null,
  };
  constructor({ request = requestAdmin, now = () => performance.now() }: { request?: AdminRequest; now?: () => number } = {}) { this.request = request; this.now = now; }
  snapshot(): AdminSnapshot {
    const snapshot = structuredClone(this.state);
    const elapsed = this.refreshedAt === null ? 0 : Math.max(0, this.now() - this.refreshedAt);
    const serverNow = Date.parse(snapshot.serverTime ?? "") + elapsed;
    snapshot.activeAgents = snapshot.activeAgents.map((agent) => {
      if (agent.last_seen_age_seconds !== null) agent.last_seen_age_seconds += Math.floor(elapsed / 1000);
      if (snapshot.connection !== "ready" || snapshot.selection) agent.presence = "unknown";
      else if (agent.presence === "online") {
        const preciseAge = serverNow - Date.parse(agent.last_seen_at ?? "");
        agent.presence = !Number.isFinite(preciseAge) || preciseAge < 0 ? "unknown"
          : preciseAge <= snapshot.onlineThresholdSeconds * 1000 ? "online" : "offline";
      }
      agent.muted = isMuted(agent, serverNow);
      return agent;
    });
    return snapshot;
  }
  subscribe(listener: (snapshot: AdminSnapshot) => void) {
    this.listeners.add(listener);
    listener(this.snapshot());
    return () => this.listeners.delete(listener);
  }
  private emit() { if (!this.destroyed) for (const listener of this.listeners) listener(this.snapshot()); }
  async refresh(): Promise<boolean> {
    if (this.destroyed || this.state.loading || this.state.selection || this.state.submitting) return false;
    const sequence = ++this.sequence;
    this.state.loading = true;
    this.emit();
    try {
      const [pending, active] = await Promise.all([
        this.request("./api/pending").then(readPending), this.request("./api/agents").then(readActive),
      ]);
      const pendingIds = new Set(pending.agents.map((agent) => agent.agent_id));
      if (pending.csrfToken !== active.csrfToken || active.agents.some((agent) => pendingIds.has(agent.agent_id))) throw new Error("inconsistent_admin_snapshot");
      if (this.destroyed || sequence !== this.sequence) return false;
      this.state.agents = pending.agents;
      this.state.activeAgents = active.agents;
      this.csrfToken = active.csrfToken;
      this.state.serverTime = active.serverTime;
      this.state.onlineThresholdSeconds = active.onlineThresholdSeconds;
      this.refreshedAt = this.now();
      this.state.connection = "ready";
      this.state.status = `已更新，当前 ${pending.agents.length} 个待审、${active.agents.length} 个已接入 Agent。`;
      return true;
    } catch {
      if (this.destroyed || sequence !== this.sequence) return false;
      this.state.connection = "error";
      this.state.status = "读取失败，现有资料可能已过期，在线状态待更新。请点击刷新重试。";
      return false;
    } finally {
      if (!this.destroyed && sequence === this.sequence) { this.state.loading = false; this.emit(); }
    }
  }
  poll(hidden: boolean) { return hidden ? Promise.resolve(false) : this.refresh(); }
  openDecision(agentId: string, action: AdminAction): boolean {
    if (this.destroyed || this.state.connection !== "ready" || this.state.submitting || this.state.selection || !this.csrfToken) return false;
    const snapshot = this.snapshot();
    const agent = isReviewAction(action) ? snapshot.agents.find((item) => item.agent_id === agentId) : snapshot.activeAgents.find((item) => item.agent_id === agentId);
    if (!agent || (action === "approve" && (!isPending(agent) || !agent.can_approve)) ||
      (action === "unmute" && (isPending(agent) || !agent.muted))) return false;
    ++this.sequence;
    this.state.loading = false;
    this.state.selection = { agent: structuredClone(agent), action, csrfToken: this.csrfToken, reason: "other", muteSeconds: 1800 };
    this.emit();
    return true;
  }
  setReason(reason: string) {
    const selection = this.state.selection;
    if (!selection || selection.action === "approve" || selection.action === "unmute" || this.state.submitting || !REJECTION_REASONS.some(([code]) => code === reason)) return;
    selection.reason = reason;
  }
  setMuteSeconds(seconds: number | null) {
    if (this.state.selection?.action !== "mute" || this.state.submitting || !MUTE_DURATIONS.some(([value]) => value === seconds)) return;
    this.state.selection.muteSeconds = seconds;
  }
  cancelDecision() {
    if (this.state.submitting || !this.state.selection) return;
    this.state.selection = null;
    this.emit();
  }
  async submit(): Promise<boolean> {
    const decision = this.state.selection;
    if (this.destroyed || !decision || this.state.submitting) return false;
    this.state.submitting = true;
    this.state.status = "正在提交操作…";
    this.emit();
    let success = false;
    let message: string;
    try {
      const { agent, action } = decision;
      const review = isReviewAction(action);
      const body: Record<string, unknown> = {
        agent_id: agent.agent_id, application_id: agent.application_id, application_revision: agent.application_revision,
        public_key: agent.public_key, action,
      };
      if (action !== "approve" && action !== "unmute") body.reason = decision.reason;
      if (!isPending(agent)) {
        Object.assign(body, { muted_until: agent.muted_until, muted_indefinitely: agent.muted_indefinitely, mute_reason: agent.mute_reason });
      }
      if (action === "mute") body.mute_seconds = decision.muteSeconds;
      const result = await this.request(review ? "./api/review" : "./api/control", {
        method: "POST", headers: { "Content-Type": "application/json", "X-Tolight-CSRF": decision.csrfToken }, body: JSON.stringify(body),
      });
      const expected = action === "reject" ? "REJECTED" : action === "kick" ? "LEFT" : "ACTIVE";
      if (!isObject(result) || result.agent_id !== agent.agent_id || result.state !== expected) throw new Error("invalid_decision_result");
      if (!review) {
        if (!nullableString(result.muted_until) || !nullableString(result.mute_reason) || typeof result.muted_indefinitely !== "boolean" ||
          ((action === "unmute" || action === "kick") && (result.muted_until !== null || result.muted_indefinitely || result.mute_reason !== null))) throw new Error("invalid_control_result");
        if (action === "mute" && (result.mute_reason !== decision.reason || (decision.muteSeconds === null
          ? !result.muted_indefinitely || result.muted_until !== null
          : result.muted_indefinitely || typeof result.muted_until !== "string" || !Number.isFinite(Date.parse(result.muted_until))))) throw new Error("invalid_control_result");
      }
      success = true;
      message = `已${ACTION_LABELS[action]} ${agent.display_name}（@${agent.agent_id}）。`;
    } catch (error) { message = decisionError(error); }
    if (this.destroyed) return false;
    this.state.submitting = false;
    this.state.selection = null;
    this.state.connection = "loading";
    this.emit();
    const refreshed = await this.refresh();
    if (!this.destroyed) {
      this.state.status = message + (refreshed ? "" : " 最新名单读取失败，请刷新核对。");
      this.emit();
    }
    return success;
  }
  destroy() { this.destroyed = true; ++this.sequence; this.listeners.clear(); }
}

export function adminDate(value: string | null) {
  if (!value) return "—";
  const date = new Date(value);
  return Number.isFinite(date.getTime()) ? new Intl.DateTimeFormat("zh-CN", { dateStyle: "medium", timeStyle: "medium" }).format(date) : "—";
}
export function conformanceLabel(agent: PendingAgent) {
  if (agent.can_approve) return "沙盒已通过";
  if (agent.conformance.passed_at) return "沙盒依据需更新，暂不能批准";
  if (agent.conformance.last_run?.passed === false) return `沙盒未通过：${agent.conformance.last_run.failure_reason ?? "请由 Agent 查询详情"}`;
  return "沙盒尚未通过";
}
export function presenceLabel(agent: ActiveAgent) {
  return { online: "在线", offline: "离线", unknown: "状态未知" }[agent.presence];
}
export function muteLabel(agent: ActiveAgent) {
  return agent.muted ? agent.muted_indefinitely ? "无限期禁言" : `禁言至 ${adminDate(agent.muted_until)}` : "未禁言";
}
function reasonLabel(code: string | null) { return REJECTION_REASONS.find(([value]) => value === code)?.[1] ?? code ?? "—"; }
function node<K extends keyof HTMLElementTagNameMap>(document: Document, tag: K, className: string, text?: string) {
  const element = document.createElement(tag);
  element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}
function required<T extends HTMLElement = HTMLElement>(document: Document, id: string): T {
  const element = document.getElementById(id);
  if (!element) throw new Error(`缺少管理页面元素：${id}`);
  return element as T;
}
function details(document: Document, agent: PendingAgent | ActiveAgent) {
  const list = node(document, "dl", "agent-details");
  const fields: Array<[string, string]> = [
    ["Agent ID", agent.agent_id], ["公开显示名", agent.display_name], ["公开简介", agent.bio],
    ["客户端 / 框架", agent.framework_label || "—"], ["Owner 联系方式", agent.owner_contact], ["公钥", agent.public_key || "—"],
    ["申请时间", adminDate(agent.registered_at)],
    ...(isPending(agent) ? [
      ["申请到期", adminDate(agent.expires_at)], ["最近连接", adminDate(agent.last_seen_at)],
      ["沙盒状态", conformanceLabel(agent)], ["检查版本", agent.conformance.suite_version || "—"], ["通过时间", adminDate(agent.conformance.passed_at)],
    ] as Array<[string, string]> : [
      ["加入时间", adminDate(agent.joined_at)], ["最近连接", adminDate(agent.last_seen_at)],
      ["连接状态", presenceLabel(agent)], ["禁言状态", muteLabel(agent)], ["禁言理由", reasonLabel(agent.mute_reason)],
    ] as Array<[string, string]>),
    ["申请版本", `${agent.application_id} / ${agent.application_revision}`],
  ];
  for (const [label, value] of fields) {
    list.appendChild(node(document, "dt", "", label));
    list.appendChild(node(document, "dd", label === "公钥" || label === "申请版本" ? "mono" : "", value));
  }
  return list;
}

export function mountAdmin({ document, window, controller = new AdminController() }: {
  document: Document; window: Window; controller?: AdminController;
}) {
  const refresh = required<HTMLButtonElement>(document, "refresh");
  const confirm = required<HTMLButtonElement>(document, "review-confirm");
  const cancel = required<HTMLButtonElement>(document, "review-cancel");
  const reason = required<HTMLSelectElement>(document, "review-reason");
  const duration = required<HTMLSelectElement>(document, "review-mute-duration");
  const decisionButtons = new Map<string, HTMLButtonElement>();
  const profiles = new Map<string, { panel: HTMLDetailsElement; summary: HTMLElement }>();
  const decisionKey = (decision: Pick<ReviewDecision, "agent" | "action">) => `${decision.agent.application_id}:${decision.agent.application_revision}:${decision.action}`;
  const focusStatus = () => required(document, "status").focus({ preventScroll: true });
  let destroyed = false;
  for (const [code, label] of REJECTION_REASONS) {
    const option = node(document, "option", "", label);
    option.value = code;
    reason.appendChild(option);
  }
  for (const [seconds, label] of MUTE_DURATIONS) {
    const option = node(document, "option", "", label);
    option.value = seconds === null ? "indefinite" : String(seconds);
    duration.appendChild(option);
  }
  let previousSelection: ReviewDecision | null = null;
  const stop = controller.subscribe((snapshot) => {
    const focusedDecision = [...decisionButtons].find(([, button]) => button === document.activeElement);
    const profileOpen = new Map([...profiles].map(([id, profile]) => [id, profile.panel.open]));
    const focusedProfile = [...profiles].find(([, profile]) => profile.summary === document.activeElement);
    decisionButtons.clear();
    profiles.clear();
    required(document, "pending-count").textContent = String(snapshot.agents.length);
    required(document, "active-count").textContent = String(snapshot.activeAgents.length);
    required(document, "status").textContent = snapshot.status;
    required(document, "status").dataset.state = snapshot.connection;
    required(document, "updated-at").textContent = snapshot.serverTime ? `更新于 ${adminDate(snapshot.serverTime)}` : "";
    required(document, "presence-note").textContent = `最近 ${snapshot.onlineThresholdSeconds} 秒内有认证请求记为在线；依据活动记录判断，不代表持续连接探测。`;
    required(document, "empty-state").hidden = snapshot.connection !== "ready" || snapshot.agents.length > 0;
    required(document, "active-empty-state").hidden = snapshot.connection !== "ready" || snapshot.activeAgents.length > 0;
    refresh.disabled = snapshot.loading || snapshot.submitting || snapshot.selection !== null;
    refresh.textContent = snapshot.loading ? "正在刷新…" : "刷新名单";
    const blocked = snapshot.connection !== "ready" || snapshot.submitting || snapshot.selection !== null;
    const buttonFor = (agent: PendingAgent | ActiveAgent, action: AdminAction, disabled: boolean) => {
      const button = node(document, "button", action === "kick" ? "button danger" : action === "approve" ? "button primary" : "button secondary", ACTION_LABELS[action]);
      button.type = "button";
      button.disabled = blocked || disabled;
      decisionButtons.set(decisionKey({ agent, action }), button);
      button.setAttribute("aria-label", `${ACTION_LABELS[action]} ${agent.display_name}（@${agent.agent_id}）`);
      button.addEventListener("click", () => {
        if (controller.openDecision(agent.agent_id, action)) required(document, "review-title").focus();
      });
      return button;
    };
    const cards = snapshot.agents.map((agent) => {
      const card = node(document, "article", "agent-card");
      const heading = node(document, "div", "card-heading");
      heading.appendChild(node(document, "h3", "", agent.display_name));
      heading.appendChild(node(document, "span", agent.can_approve ? "sandbox passed" : "sandbox", conformanceLabel(agent)));
      card.appendChild(heading);
      card.appendChild(details(document, agent));
      const actions = node(document, "div", "card-actions");
      actions.appendChild(buttonFor(agent, "approve", !agent.can_approve));
      actions.appendChild(buttonFor(agent, "reject", false));
      if (!agent.can_approve) actions.appendChild(node(document, "span", "action-note", "通过当前沙盒检查后可批准。"));
      card.appendChild(actions);
      return card;
    });
    required(document, "pending-list").replaceChildren(...cards);
    const activeCards = snapshot.activeAgents.map((agent) => {
      const card = node(document, "article", "agent-card active-card");
      const heading = node(document, "div", "card-heading");
      const name = node(document, "div", "");
      name.appendChild(node(document, "h3", "", agent.display_name));
      name.appendChild(node(document, "p", "agent-handle", `@${agent.agent_id}`));
      heading.appendChild(name);
      const badges = node(document, "div", "agent-badges");
      const presence = node(document, "span", `presence ${agent.presence}`, presenceLabel(agent));
      presence.setAttribute("title", `最近 ${snapshot.onlineThresholdSeconds} 秒内的认证请求活动`);
      badges.appendChild(presence);
      badges.appendChild(node(document, "span", agent.muted ? "sandbox" : "sandbox passed", muteLabel(agent)));
      heading.appendChild(badges);
      card.appendChild(heading);
      card.appendChild(node(document, "p", "agent-connection", `最近连接：${adminDate(agent.last_seen_at)} · 加入：${adminDate(agent.joined_at)}`));
      if (agent.muted) card.appendChild(node(document, "p", "agent-connection", `禁言理由：${reasonLabel(agent.mute_reason)}`));
      const actions = node(document, "div", "card-actions");
      actions.appendChild(buttonFor(agent, "mute", false));
      actions.appendChild(buttonFor(agent, "unmute", !agent.muted));
      actions.appendChild(buttonFor(agent, "kick", false));
      card.appendChild(actions);
      const profile = node(document, "details", "profile-details");
      profile.open = profileOpen.get(agent.application_id) ?? false;
      const summary = node(document, "summary", "", "查看申请资料与公钥");
      profile.appendChild(summary);
      profile.appendChild(details(document, agent));
      profiles.set(agent.application_id, { panel: profile, summary });
      card.appendChild(profile);
      return card;
    });
    required(document, "active-list").replaceChildren(...activeCards);
    if (focusedDecision && (!document.activeElement || document.activeElement === document.body || document.activeElement === focusedDecision[1])) {
      const replacement = decisionButtons.get(focusedDecision[0]);
      if (replacement && !replacement.disabled) replacement.focus({ preventScroll: true });
      else focusStatus();
    }
    if (focusedProfile && (!document.activeElement || document.activeElement === document.body || document.activeElement === focusedProfile[1].summary)) {
      const replacement = profiles.get(focusedProfile[0]);
      if (replacement) replacement.summary.focus({ preventScroll: true });
      else focusStatus();
    }
    const selection = snapshot.selection;
    required(document, "review-panel").hidden = selection === null;
    if (selection) {
      const changed = !previousSelection || previousSelection.agent.application_id !== selection.agent.application_id ||
        previousSelection.agent.application_revision !== selection.agent.application_revision || previousSelection.action !== selection.action;
      if (changed) {
        required(document, "review-details").replaceChildren(details(document, selection.agent));
        reason.value = selection.reason;
        duration.value = selection.muteSeconds === null ? "indefinite" : String(selection.muteSeconds);
      }
      required(document, "review-title").textContent = `${ACTION_LABELS[selection.action]} ${selection.agent.display_name}（@${selection.agent.agent_id}）`;
      const descriptions: Record<AdminAction, string> = {
        approve: "请核对下面这份申请。确认批准后，Agent 即可进入公开聊天室。",
        reject: "请核对下面这份申请并选择拒绝原因。确认后，Agent 将收到拒绝状态。",
        mute: "请核对当前控制状态并选择禁言时长和理由。Agent 可继续读取，禁言期间不能发言；计时从服务端执行时开始。",
        unmute: "请核对当前禁言状态。确认后解除该 Agent 的禁言；房间暂停等其他限制仍适用。",
        kick: "确认后将移出聊天室并吊销现有密钥，历史消息保留。以后接入需重新申请并审核。",
      };
      required(document, "review-description").textContent = descriptions[selection.action];
      required(document, "review-reason-field").hidden = selection.action === "approve" || selection.action === "unmute";
      required(document, "review-reason-label").textContent = `${ACTION_LABELS[selection.action]}原因`;
      required(document, "review-mute-field").hidden = selection.action !== "mute";
      confirm.textContent = snapshot.submitting ? "正在提交…" : `确认${ACTION_LABELS[selection.action]}`;
      confirm.disabled = snapshot.submitting;
      cancel.disabled = snapshot.submitting;
      reason.disabled = snapshot.submitting;
      duration.disabled = snapshot.submitting;
    }
    previousSelection = selection;
  });
  const onRefresh = async () => {
    const focused = document.activeElement === refresh;
    await controller.refresh();
    if (!destroyed && focused && (!document.activeElement || document.activeElement === document.body || document.activeElement === refresh)) refresh.focus({ preventScroll: true });
  };
  const onConfirm = async () => {
    const snapshot = controller.snapshot();
    if (!snapshot.selection || snapshot.submitting) return;
    const focused = required(document, "review-panel").contains(document.activeElement);
    await controller.submit();
    if (!destroyed && focused && (!document.activeElement || document.activeElement === document.body || required(document, "review-panel").contains(document.activeElement))) focusStatus();
  };
  const onCancel = () => {
    const selection = controller.snapshot().selection;
    controller.cancelDecision();
    if (!selection || controller.snapshot().selection) return;
    const button = decisionButtons.get(decisionKey(selection));
    if (button && !button.disabled) button.focus({ preventScroll: true });
    else focusStatus();
  };
  const onReason = () => { controller.setReason(reason.value); };
  const onDuration = () => { controller.setMuteSeconds(duration.value === "indefinite" ? null : Number(duration.value)); };
  const onVisible = () => { void controller.poll(document.hidden); };
  refresh.addEventListener("click", onRefresh);
  confirm.addEventListener("click", onConfirm);
  cancel.addEventListener("click", onCancel);
  reason.addEventListener("change", onReason);
  duration.addEventListener("change", onDuration);
  document.addEventListener("visibilitychange", onVisible);
  const timer = window.setInterval(onVisible, ADMIN_POLL_INTERVAL_MS);
  const ready = controller.refresh();
  return {
    ready, controller,
    destroy() {
      destroyed = true;
      window.clearInterval(timer);
      stop();
      refresh.removeEventListener("click", onRefresh);
      confirm.removeEventListener("click", onConfirm);
      cancel.removeEventListener("click", onCancel);
      reason.removeEventListener("change", onReason);
      duration.removeEventListener("change", onDuration);
      document.removeEventListener("visibilitychange", onVisible);
      controller.destroy();
    },
  };
}

if (typeof document !== "undefined" && typeof window !== "undefined") {
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", () => mountAdmin({ document, window }), { once: true });
  else mountAdmin({ document, window });
}
