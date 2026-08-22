import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import "./styles.css";

type ServiceStatus = "running" | "stopped" | "unavailable";
type ServiceManagement = "desktop" | "external" | "unavailable";
type CredentialStatus = "managed" | "referenced" | "missing" | "not-required";
type RemoteState = "needs-validation" | "local-ready" | "blocked";

interface ServiceState {
  status: ServiceStatus;
  management: ServiceManagement;
  manageable: boolean;
  bindAddress: string;
  pid: number | null;
  uptimeSeconds: number | null;
  detail: string;
}

interface ProviderSummary {
  id: string;
  label: string;
  protocol: string;
  enabled: boolean;
  credentialStatus: CredentialStatus;
  modelCount: number;
  official: boolean;
  baseUrl: string | null;
  secretProfile: string | null;
  allowInsecureHttp: boolean;
}

interface ModelSummary {
  id: string;
  label: string;
  providerId: string;
  providerLabel: string;
  official: boolean;
  visible: boolean;
  capabilities: string[];
  upstreamModel: string;
  contextWindow: number | null;
  maxOutputTokens: number | null;
  enabled: boolean;
}

interface UpdateModelInput {
  modelId: string;
  displayName?: string;
  upstreamModel?: string;
  contextWindow?: number;
  maxOutputTokens?: number;
  enabled?: boolean;
}

interface UpdateProviderInput {
  providerId: string;
  baseUrl?: string;
  enabled?: boolean;
  apiKey?: string;
  secretProfile?: string;
  allowInsecureHttp?: boolean;
}

interface RemoteCompatibility {
  state: RemoteState;
  ios: string;
  android: string;
  message: string;
  lastCheckedAt: number | null;
}

interface DashboardState {
  service: ServiceState;
  providers: ProviderSummary[];
  models: ModelSummary[];
  remote: RemoteCompatibility;
  catalogVersion: string;
  configPath: string;
  codexIntegration: CodexIntegrationState;
}

interface CodexIntegrationState {
  installed: boolean;
}

interface RestoreCodexResult {
  restored: boolean;
  configPath: string;
}

interface ProviderPreset {
  id: string;
  label: string;
  defaultBaseUrl: string;
  defaultModel: string;
  requiresApiKey: boolean;
  contextWindow: number | null;
  maxOutputTokens: number | null;
}

interface ProviderSetupOptions {
  presets: ProviderPreset[];
}

interface AddProviderWithModelInput {
  providerId: string;
  presetId: string;
  baseUrl: string;
  apiKey: string;
  secretProfile: string;
  modelId: string;
  upstreamModel: string;
  displayName: string;
  contextWindow: number | null;
  maxOutputTokens: number | null;
  enabled: boolean;
  allowInsecureHttp: boolean;
}

interface AddProviderWithModelResult {
  providers: ProviderSummary[];
  models: ModelSummary[];
  requiresRestart: boolean;
}

interface LocalSetupResult {
  codexConfigPath: string;
  recoveryBackupPath: string | null;
  serviceDefinitionPath: string;
  bindAddress: string;
  externalModels: string[];
  integrationInstalled: boolean;
  serviceInstalled: boolean;
  healthy: boolean;
  restartChatgptRequired: boolean;
  pickerPending: boolean;
  partial: boolean;
}

interface LocalSetupFailure {
  stage: string;
  message: string;
  integrationInstalled: boolean;
  serviceInstalled: boolean;
  healthy: boolean;
  restartChatgptRequired: boolean;
  partial: boolean;
}

type SetupStepState = "pending" | "working" | "complete" | "failed";

const fallbackState: DashboardState = {
  service: {
    status: "unavailable",
    management: "unavailable",
    manageable: false,
    bindAddress: "未连接 Tauri 后端",
    pid: null,
    uptimeSeconds: null,
    detail: "浏览器预览模式不会读取或修改路由配置，也不会模拟服务运行。",
  },
  providers: [],
  models: [],
  remote: {
    state: "needs-validation",
    ios: "未验收",
    android: "未验收",
    message: "浏览器预览无法核对本机服务；移动端仍需按当前客户端版本执行端到端验收。",
    lastCheckedAt: null,
  },
  catalogVersion: "不可用",
  configPath: "未连接",
  codexIntegration: { installed: false },
};

let dashboard: DashboardState = structuredClone(fallbackState);
let draggedModelId: string | null = null;
let providerPresets: ProviderPreset[] = [];
let providerSubmitting = false;
let upstreamModelCustomized = false;
let displayNameCustomized = false;

const customProviderPreset: ProviderPreset = {
  id: "custom-compatible",
  label: "自定义 OpenAI 兼容",
  defaultBaseUrl: "",
  defaultModel: "",
  requiresApiKey: true,
  contextWindow: null,
  maxOutputTokens: null,
};

function byId<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (!(element instanceof HTMLElement)) {
    throw new Error(`Missing element: ${id}`);
  }
  return element as T;
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  return invoke<T>(command, args);
}

const RELEASES_URL = "https://github.com/deeplearning1993/modelrela/releases";
const THEME_STORAGE_KEY = "mr.theme";
const VIEW_STORAGE_KEY = "mr.view";

type ThemeChoice = "system" | "light" | "dark";
type ViewName = "overview" | "providers" | "models" | "settings";

const VIEW_TITLES: Record<ViewName, string> = {
  overview: "概览",
  providers: "供应商",
  models: "模型",
  settings: "设置",
};

function icon(name: string): SVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", "icon");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
  use.setAttribute("href", `#${name}`);
  svg.append(use);
  return svg;
}

/* ---------- theme ---------- */

function currentTheme(): ThemeChoice {
  const value = document.documentElement.dataset.theme;
  return value === "light" || value === "dark" ? value : "system";
}

function applyTheme(theme: ThemeChoice): void {
  document.documentElement.dataset.theme = theme;
  try {
    localStorage.setItem(THEME_STORAGE_KEY, theme);
  } catch {
    /* storage unavailable */
  }
  const cycleIcons: Record<ThemeChoice, string> = {
    system: "#i-monitor",
    light: "#i-sun",
    dark: "#i-moon",
  };
  const cycleUse = document.getElementById("theme-cycle-icon");
  cycleUse?.setAttribute("href", cycleIcons[theme]);
  for (const button of document.querySelectorAll<HTMLButtonElement>("#theme-segment button")) {
    button.classList.toggle(
      "active",
      button.dataset.themeChoice === theme,
    );
  }
}

/* ---------- views ---------- */

function switchView(view: ViewName): void {
  if (!(view in VIEW_TITLES)) return;
  for (const section of document.querySelectorAll<HTMLElement>("[data-view-section]")) {
    section.classList.toggle("active", section.dataset.viewSection === view);
  }
  for (const item of document.querySelectorAll<HTMLElement>("#main-nav .nav-item")) {
    item.classList.toggle("active", item.dataset.view === view);
  }
  byId<HTMLHeadingElement>("view-title").textContent = VIEW_TITLES[view];
  try {
    localStorage.setItem(VIEW_STORAGE_KEY, view);
  } catch {
    /* storage unavailable */
  }
  if (location.hash !== `#${view}`) {
    history.replaceState(null, "", `#${view}`);
  }
}

function initialView(): ViewName {
  const fromHash = location.hash.slice(1);
  if (fromHash in VIEW_TITLES) return fromHash as ViewName;
  try {
    const stored = localStorage.getItem(VIEW_STORAGE_KEY);
    if (stored && stored in VIEW_TITLES) return stored as ViewName;
  } catch {
    /* storage unavailable */
  }
  return "overview";
}

/* ---------- confirm dialog ---------- */

function confirmAction(message: string, title = "确认操作"): Promise<boolean> {
  const dialog = byId<HTMLDialogElement>("confirm-dialog");
  byId<HTMLElement>("confirm-title").textContent = title;
  byId<HTMLElement>("confirm-message").textContent = message;
  return new Promise((resolve) => {
    const finish = (confirmed: boolean) => {
      dialog.close();
      resolve(confirmed);
    };
    const ok = byId<HTMLButtonElement>("confirm-ok");
    const cancel = byId<HTMLButtonElement>("confirm-cancel");
    const onOk = () => finish(true);
    const onCancel = () => finish(false);
    const onClose = () => {
      ok.removeEventListener("click", onOk);
      cancel.removeEventListener("click", onCancel);
      dialog.removeEventListener("close", onClose);
      resolve(dialog.returnValue === "ok");
    };
    ok.addEventListener("click", onOk);
    cancel.addEventListener("click", onCancel);
    dialog.addEventListener("close", onClose, { once: true });
    dialog.showModal();
  });
}

async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    setNotice("已复制到剪贴板。");
  } catch {
    setNotice(`复制失败，请手动复制：${text}`);
  }
}

function setError(error: unknown): void {
  const banner = byId<HTMLDivElement>("error-banner");
  banner.textContent = error instanceof Error ? error.message : String(error);
  banner.className = "banner error";
  banner.hidden = false;
}

function setNotice(message: string): void {
  const banner = byId<HTMLDivElement>("error-banner");
  banner.textContent = message;
  banner.className = "banner info";
  banner.hidden = false;
}

function clearError(): void {
  const banner = byId<HTMLDivElement>("error-banner");
  banner.textContent = "";
  banner.className = "banner error hidden";
  banner.hidden = true;
}

function renderService(): void {
  const { service } = dashboard;
  const running = service.status === "running";
  const pill = byId<HTMLSpanElement>("service-pill");
  const labels: Record<ServiceStatus, string> = {
    running: "运行中",
    stopped: "已停止",
    unavailable: "不可接管",
  };
  pill.textContent = labels[service.status];
  pill.className = `pill ${running ? "success" : service.status === "unavailable" ? "danger" : "neutral"}`;
  byId<HTMLElement>("bind-address").textContent = service.bindAddress;
  byId<HTMLElement>("service-pid").textContent = service.pid?.toString() ?? "—";
  byId<HTMLElement>("service-uptime").textContent = formatDuration(service.uptimeSeconds);
  byId<HTMLElement>("catalog-version").textContent = dashboard.catalogVersion;
  byId<HTMLElement>("service-detail").textContent = service.detail;
  byId<HTMLElement>("config-path").textContent = dashboard.configPath;

  const button = byId<HTMLButtonElement>("service-button");
  button.disabled = !service.manageable;
  button.textContent = running
    ? service.management === "desktop"
      ? "停止本机服务"
      : "由外部服务管理"
    : service.manageable
      ? "启动本机服务"
      : "无法从此窗口启动";
  button.className = `button ${running ? "ghost" : "primary"}`;

  const dot = byId<HTMLSpanElement>("topbar-dot");
  dot.className = `dot ${running ? "ok" : service.status === "unavailable" ? "error" : "warn"}`;
  byId<HTMLSpanElement>("topbar-status-text").textContent = running
    ? service.management === "desktop"
      ? "本机服务运行中"
      : "外部服务运行中"
    : service.status === "stopped"
      ? "服务已停止"
      : "服务不可用";

  const integrationPill = byId<HTMLSpanElement>("codex-integration-pill");
  const installed = dashboard.codexIntegration.installed;
  integrationPill.textContent = installed ? "已接入" : "未接入";
  integrationPill.className = `pill ${installed ? "success" : "neutral"}`;
}

function formatDuration(seconds: number | null): string {
  if (seconds === null) return "—";
  if (seconds < 60) return `${seconds} 秒`;
  const minutes = Math.floor(seconds / 60);
  return minutes < 60 ? `${minutes} 分钟` : `${Math.floor(minutes / 60)} 小时`;
}

function renderProviders(): void {
  const list = byId<HTMLDivElement>("provider-list");
  list.replaceChildren(
    ...dashboard.providers.map((provider) => {
      const row = document.createElement("article");
      row.className = "provider-row";

      const identity = document.createElement("div");
      identity.className = "provider-identity";
      const mark = document.createElement("span");
      mark.className = `provider-mark ${provider.official ? "official" : "external"}`;
      mark.textContent = provider.label.slice(0, 1).toUpperCase();
      const text = document.createElement("span");
      const name = document.createElement("strong");
      name.textContent = provider.label;
      const protocol = document.createElement("small");
      protocol.textContent = provider.protocol;
      text.append(name, protocol);
      identity.append(mark, text);

      const details = document.createElement("div");
      details.className = "provider-details";
      const models = document.createElement("span");
      models.textContent = provider.official ? "官方目录动态获取" : `${provider.modelCount} 个模型`;
      const credential = document.createElement("span");
      credential.className = `credential ${provider.credentialStatus}`;
      credential.textContent = credentialLabel(provider.credentialStatus);
      const enabled = document.createElement("span");
      enabled.className = `pill ${provider.enabled ? "success" : "neutral"}`;
      enabled.textContent = provider.enabled ? "已启用" : "未启用";
      details.append(models, credential, enabled);
      if (provider.allowInsecureHttp) {
        const insecure = document.createElement("span");
        insecure.className = "credential missing";
        insecure.textContent = "明文 HTTP";
        details.append(insecure);
      }
      row.append(identity, details);

      if (!provider.official) {
        row.dataset.providerId = provider.id;
        const actions = document.createElement("div");
        actions.className = "row-actions";
        const edit = document.createElement("button");
        edit.className = "icon-button";
        edit.type = "button";
        edit.title = "编辑供应商";
        edit.setAttribute("aria-label", `编辑供应商 ${provider.label}`);
        edit.append(icon("i-pencil"));
        edit.addEventListener("click", () => openProviderEditDialog(provider));
        const remove = document.createElement("button");
        remove.className = "icon-button danger";
        remove.type = "button";
        remove.title = "删除供应商";
        remove.setAttribute("aria-label", `删除供应商 ${provider.label}`);
        remove.append(icon("i-trash"));
        remove.addEventListener("click", () => removeProvider(provider.id, provider.label));
        actions.append(edit, remove);
        row.append(actions);
      }
      return row;
    }),
  );
}

function credentialLabel(status: CredentialStatus): string {
  const labels: Record<CredentialStatus, string> = {
    managed: "由 ChatGPT 管理",
    referenced: "已保存密钥引用（未读取凭据）",
    missing: "需要安全配置",
    "not-required": "无需密钥",
  };
  return labels[status];
}

function renderModels(): void {
  const list = byId<HTMLOListElement>("model-list");
  list.replaceChildren(
    ...dashboard.models.map((model) => {
      const item = document.createElement("li");
      item.className = `model-row ${model.visible ? "" : "muted"}`;
      item.draggable = true;
      item.dataset.modelId = model.id;

      const handle = document.createElement("button");
      handle.className = "drag-handle";
      handle.type = "button";
      handle.title = "拖动排序";
      handle.setAttribute("aria-label", `拖动 ${model.label} 排序`);
      handle.append(icon("i-grip"));

      const identity = document.createElement("div");
      identity.className = "model-identity";
      const heading = document.createElement("div");
      const name = document.createElement("strong");
      name.textContent = model.label;
      const origin = document.createElement("span");
      origin.className = `origin ${model.official ? "official" : "external"}`;
      origin.textContent = model.official ? "官方" : "第三方";
      heading.append(name, origin);
      const provider = document.createElement("small");
      provider.textContent = `${model.providerLabel} / ${model.id}`;
      const capabilities = document.createElement("div");
      capabilities.className = "capabilities";
      for (const capability of model.capabilities) {
        const tag = document.createElement("span");
        tag.textContent = capability;
        capabilities.append(tag);
      }
      identity.append(heading, provider, capabilities);

      const toggleLabel = document.createElement("label");
      toggleLabel.className = "toggle";
      const toggle = document.createElement("input");
      toggle.type = "checkbox";
      toggle.checked = model.visible;
      toggle.setAttribute("aria-label", `在 Codex 中显示 ${model.label}`);
      const slider = document.createElement("span");
      slider.setAttribute("aria-hidden", "true");
      toggle.addEventListener("change", () => {
        void setModelVisibility(model.id, toggle.checked);
      });
      toggleLabel.append(toggle, slider);

      item.addEventListener("dragstart", () => {
        draggedModelId = model.id;
        item.classList.add("dragging");
      });
      item.addEventListener("dragend", () => {
        draggedModelId = null;
        item.classList.remove("dragging");
      });
      item.addEventListener("dragover", (event) => {
        event.preventDefault();
        item.classList.add("drag-target");
      });
      item.addEventListener("dragleave", () => item.classList.remove("drag-target"));
      item.addEventListener("drop", (event) => {
        event.preventDefault();
        item.classList.remove("drag-target");
        if (draggedModelId && draggedModelId !== model.id) {
          void reorderModels(draggedModelId, model.id);
        }
      });

      let actions: HTMLElement | null = null;
      if (!model.official) {
        actions = document.createElement("div");
        actions.className = "row-actions";
        const edit = document.createElement("button");
        edit.className = "icon-button";
        edit.type = "button";
        edit.title = "编辑模型";
        edit.setAttribute("aria-label", `编辑模型 ${model.label}`);
        edit.append(icon("i-pencil"));
        edit.addEventListener("click", () => openModelEditDialog(model));
        const remove = document.createElement("button");
        remove.className = "icon-button danger";
        remove.type = "button";
        remove.title = "删除模型";
        remove.setAttribute("aria-label", `删除模型 ${model.label}`);
        remove.append(icon("i-trash"));
        remove.addEventListener("click", () => deleteModel(model.id, model.label));
        actions.append(edit, remove);
      }

      if (actions) {
        item.append(handle, identity, toggleLabel, actions);
      } else {
        item.append(handle, identity, toggleLabel);
      }
      return item;
    }),
  );

  const count = dashboard.models.filter((model) => model.visible).length;
  byId<HTMLSpanElement>("visible-count").textContent = `${count} 个已显示`;
  const setup = byId<HTMLButtonElement>("complete-setup-button");
  setup.disabled = count === 0;
  setup.title = count === 0 ? "请先添加并启用至少一个第三方模型" : "自动完成本机 Codex 接入";
  const restore = byId<HTMLButtonElement>("restore-codex-button");
  restore.disabled = !dashboard.codexIntegration.installed;
  restore.title = dashboard.codexIntegration.installed
    ? "把 Codex 本机配置还原为接入本路由前的状态"
    : "当前没有已安装的 Codex 集成";
}

function render(): void {
  renderService();
  renderProviders();
  renderModels();
}

async function refresh(): Promise<void> {
  clearError();
  try {
    dashboard = await call<DashboardState>("dashboard_state");
  } catch (error) {
    dashboard = structuredClone(fallbackState);
    setError(`当前未连接 Tauri 后端，界面保持空状态且不会读写配置。${String(error)}`);
  }
  render();
}

async function toggleService(): Promise<void> {
  clearError();
  const shouldRun = dashboard.service.status !== "running";
  try {
    dashboard.service = await call<ServiceState>("set_service_running", { running: shouldRun });
    renderService();
  } catch (error) {
    setError(error);
  }
}

async function setModelVisibility(modelId: string, visible: boolean): Promise<void> {
  clearError();
  try {
    dashboard.models = await call<ModelSummary[]>("set_model_visibility", { modelId, visible });
    setNotice("模型显示配置已持久化；若路由正在运行，请重启服务以加载新配置。");
  } catch (error) {
    setError(error);
  }
  renderModels();
}

async function reorderModels(sourceId: string, targetId: string): Promise<void> {
  clearError();
  const reordered = [...dashboard.models];
  const sourceIndex = reordered.findIndex((model) => model.id === sourceId);
  const targetIndex = reordered.findIndex((model) => model.id === targetId);
  if (sourceIndex < 0 || targetIndex < 0) return;
  const [source] = reordered.splice(sourceIndex, 1);
  if (!source) return;
  reordered.splice(targetIndex, 0, source);

  try {
    dashboard.models = await call<ModelSummary[]>("reorder_models", {
      orderedIds: reordered.map((model) => model.id),
    });
    setNotice("外部模型顺序已持久化；未知及官方目录项保持原位。运行中的路由需重启后生效。");
  } catch (error) {
    setError(error);
  }
  renderModels();
}

async function removeProvider(providerId: string, label: string): Promise<void> {
  const modelCount = dashboard.models.filter((model) => model.providerId === providerId).length;
  const message = modelCount > 0
    ? `供应商「${label}」下仍有 ${modelCount} 个模型，请先删除这些模型后再删除供应商。`
    : `确认删除供应商「${label}」？密钥引用会从配置移除，但操作系统凭据库中的密钥将保留。`;
  if (modelCount > 0) {
    setNotice(message);
    return;
  }
  if (!(await confirmAction(message, "删除供应商"))) return;
  clearError();
  try {
    const result = await call<AddProviderWithModelResult>("remove_provider", { providerId });
    dashboard.providers = result.providers;
    dashboard.models = result.models;
    setNotice(
      result.requiresRestart
        ? `供应商「${label}」已删除；路由正在运行，请重启服务以加载新配置。`
        : `供应商「${label}」已删除。`,
    );
  } catch (error) {
    setError(error);
  }
  renderProviders();
  renderModels();
}

async function deleteModel(modelId: string, label: string): Promise<void> {
  if (!(await confirmAction(`确认删除模型「${label}」？`, "删除模型"))) return;
  clearError();
  try {
    const result = await call<AddProviderWithModelResult>("delete_model", { modelId });
    dashboard.providers = result.providers;
    dashboard.models = result.models;
    setNotice(
      result.requiresRestart
        ? `模型「${label}」已删除；路由正在运行，请重启服务以加载新配置。`
        : `模型「${label}」已删除。`,
    );
  } catch (error) {
    setError(error);
  }
  renderProviders();
  renderModels();
}

async function restoreCodexConfig(): Promise<void> {
  const confirmed = await confirmAction(
    "将把 Codex 本机配置还原为接入本路由前的状态（撤销 model_provider、openai_base_url、remote_control 的修改）。路由凭据与模型配置不受影响。确认继续？",
    "恢复 Codex 默认配置",
  );
  if (!confirmed) return;
  clearError();
  try {
    const result = await call<RestoreCodexResult>("restore_codex_config");
    if (result.restored) {
      setNotice(
        `已还原 Codex 默认配置（${result.configPath}）。请完全退出并重新打开 Codex / ChatGPT 客户端以恢复官方登录与目录。`,
      );
      await refresh();
    } else {
      setNotice("当前没有已安装的 Codex 集成，无需还原。");
    }
  } catch (error) {
    setError(error);
  }
}

function openModelEditDialog(model: ModelSummary): void {
  const dialog = byId<HTMLDialogElement>("model-edit-dialog");
  byId<HTMLElement>("model-edit-title").textContent = `编辑模型 ${model.label}`;
  byId<HTMLInputElement>("model-edit-id").value = model.id;
  byId<HTMLInputElement>("model-edit-display-name").value = model.label;
  byId<HTMLInputElement>("model-edit-upstream").value = model.upstreamModel;
  setOptionalNumber("model-edit-context-window", model.contextWindow);
  setOptionalNumber("model-edit-max-output", model.maxOutputTokens);
  byId<HTMLInputElement>("model-edit-enabled").checked = model.enabled;
  byId<HTMLElement>("model-edit-error").classList.add("hidden");
  dialog.showModal();
}

async function submitModelEdit(event: SubmitEvent): Promise<void> {
  event.preventDefault();
  const modelId = byId<HTMLInputElement>("model-edit-id").value;
  const displayName = byId<HTMLInputElement>("model-edit-display-name").value.trim();
  const upstream = byId<HTMLInputElement>("model-edit-upstream").value.trim();
  if (!displayName || !upstream) {
    showModelEditError("显示名称和上游模型 ID 不能为空。");
    return;
  }
  const input: UpdateModelInput = {
    modelId,
    displayName,
    upstreamModel: upstream,
    contextWindow: readOptionalNumber("model-edit-context-window") ?? undefined,
    maxOutputTokens: readOptionalNumber("model-edit-max-output") ?? undefined,
    enabled: byId<HTMLInputElement>("model-edit-enabled").checked,
  };
  try {
    const result = await call<AddProviderWithModelResult>("update_model", { input });
    dashboard.providers = result.providers;
    dashboard.models = result.models;
    byId<HTMLDialogElement>("model-edit-dialog").close();
    setNotice(
      result.requiresRestart
        ? "模型已更新；路由正在运行，请重启服务以加载新配置。"
        : "模型已更新。",
    );
  } catch (error) {
    showModelEditError(error);
  }
  renderProviders();
  renderModels();
}

function showModelEditError(message: unknown): void {
  const banner = byId<HTMLElement>("model-edit-error");
  banner.textContent = message instanceof Error ? message.message : String(message);
  banner.classList.remove("hidden");
}

function openProviderEditDialog(provider: ProviderSummary): void {
  const dialog = byId<HTMLDialogElement>("provider-edit-dialog");
  byId<HTMLElement>("provider-edit-title").textContent = `编辑供应商 ${provider.label}`;
  byId<HTMLInputElement>("provider-edit-id").value = provider.id;
  byId<HTMLInputElement>("provider-edit-base-url").value = provider.baseUrl ?? "";
  byId<HTMLInputElement>("provider-edit-enabled").checked = provider.enabled;
  byId<HTMLInputElement>("provider-edit-allow-insecure").checked =
    provider.allowInsecureHttp;
  byId<HTMLInputElement>("provider-edit-api-key").value = "";
  byId<HTMLElement>("provider-edit-error").classList.add("hidden");
  dialog.showModal();
}

async function submitProviderEdit(event: SubmitEvent): Promise<void> {
  event.preventDefault();
  const providerId = byId<HTMLInputElement>("provider-edit-id").value;
  const baseUrl = byId<HTMLInputElement>("provider-edit-base-url").value.trim();
  const apiKey = byId<HTMLInputElement>("provider-edit-api-key").value;
  const input: UpdateProviderInput = {
    providerId,
    baseUrl,
    enabled: byId<HTMLInputElement>("provider-edit-enabled").checked,
    allowInsecureHttp: byId<HTMLInputElement>("provider-edit-allow-insecure").checked,
  };
  if (apiKey) input.apiKey = apiKey;
  try {
    const result = await call<AddProviderWithModelResult>("update_provider", { input });
    dashboard.providers = result.providers;
    dashboard.models = result.models;
    byId<HTMLDialogElement>("provider-edit-dialog").close();
    setNotice(
      result.requiresRestart
        ? "供应商已更新；路由正在运行，请重启服务以加载新配置。"
        : "供应商已更新。",
    );
  } catch (error) {
    showProviderEditError(error);
  }
  renderProviders();
  renderModels();
}

function showProviderEditError(message: unknown): void {
  const banner = byId<HTMLElement>("provider-edit-error");
  banner.textContent = message instanceof Error ? message.message : String(message);
  banner.classList.remove("hidden");
}

function setProviderFormError(error: unknown): void {
  const message = byId<HTMLDivElement>("provider-form-error");
  message.textContent = error instanceof Error ? error.message : String(error);
  message.className = "form-message error";
}

function clearProviderFormError(): void {
  const message = byId<HTMLDivElement>("provider-form-error");
  message.textContent = "";
  message.className = "form-message error hidden";
}

function setSetupStep(id: string, state: SetupStepState, status: string): void {
  const step = byId<HTMLLIElement>(id);
  step.className = state;
  const label = step.querySelector("strong");
  if (label) label.textContent = status;
}

function resetSetupProgress(): void {
  byId<HTMLFieldSetElement>("provider-fieldset").classList.remove("hidden");
  byId<HTMLElement>("provider-dialog-intro").classList.remove("hidden");
  byId<HTMLElement>("local-setup-progress").classList.add("hidden");
  byId<HTMLElement>("local-setup-error").classList.add("hidden");
  byId<HTMLButtonElement>("local-setup-retry").classList.add("hidden");
  byId<HTMLButtonElement>("local-setup-close").classList.add("hidden");
  const mark = byId<HTMLElement>("local-setup-mark");
  mark.className = "setup-mark working";
  setSetupStep("setup-step-provider", "complete", "已保存");
  setSetupStep("setup-step-startup", "working", "处理中");
  setSetupStep("setup-step-health", "pending", "等待");
  setSetupStep("setup-step-integration", "pending", "等待");
}

function beginSetupProgress(): void {
  resetSetupProgress();
  byId<HTMLFieldSetElement>("provider-fieldset").classList.add("hidden");
  byId<HTMLElement>("provider-dialog-intro").classList.add("hidden");
  byId<HTMLElement>("local-setup-progress").classList.remove("hidden");
  byId<HTMLElement>("local-setup-title").textContent = "正在一键接入 Codex";
  byId<HTMLElement>("local-setup-summary").textContent =
    "供应商已安全保存，正在注册本机服务并完成 Codex 配置。";
}

async function setupExistingConfiguration(): Promise<void> {
  clearError();
  clearProviderFormError();
  const dialog = byId<HTMLDialogElement>("provider-dialog");
  if (!dialog.open) dialog.showModal();
  beginSetupProgress();
  setSetupStep("setup-step-provider", "complete", "使用现有配置");
  await completeLocalSetup();
}

function asLocalSetupFailure(error: unknown): LocalSetupFailure {
  let candidate: unknown = error;
  if (typeof error === "string") {
    try {
      candidate = JSON.parse(error) as unknown;
    } catch {
      candidate = undefined;
    }
  }
  if (candidate && typeof candidate === "object") {
    const value = candidate as Partial<LocalSetupFailure>;
    if (typeof value.message === "string") {
      return {
        stage: typeof value.stage === "string" ? value.stage : "unknown",
        message: value.message,
        integrationInstalled: value.integrationInstalled === true,
        serviceInstalled: value.serviceInstalled === true,
        healthy: value.healthy === true,
        restartChatgptRequired: value.restartChatgptRequired === true,
        partial: value.partial === true,
      };
    }
  }
  return {
    stage: "unknown",
    message: error instanceof Error ? error.message : String(error),
    integrationInstalled: false,
    serviceInstalled: false,
    healthy: false,
    restartChatgptRequired: false,
    partial: false,
  };
}

function setupStageLabel(stage: string): string {
  if (["stop-desktop-service", "prepare-service", "inspect-service"].includes(stage)) {
    return "准备本机服务";
  }
  if (["install-service", "verify-service"].includes(stage)) return "注册登录自启";
  if (["health-check", "catalog-validation"].includes(stage)) return "启动与模型目录检查";
  if (["install-codex-integration", "verify-codex-integration"].includes(stage)) {
    return "写入 Codex 配置";
  }
  return "本机接入";
}

function renderSetupFailure(failure: LocalSetupFailure): void {
  const mark = byId<HTMLElement>("local-setup-mark");
  mark.className = "setup-mark failed";
  byId<HTMLElement>("local-setup-title").textContent = `${setupStageLabel(failure.stage)}未完成`;
  byId<HTMLElement>("local-setup-summary").textContent = failure.partial
    ? "已完成的步骤会保留，重试只继续本机接入，不会再次提交 API Key。"
    : "供应商和密钥已经安全保存；修正下方问题后可直接重试，无需再次输入 API Key。";

  setSetupStep(
    "setup-step-startup",
    failure.serviceInstalled ? "complete" : "failed",
    failure.serviceInstalled ? "已完成" : "未完成",
  );
  setSetupStep(
    "setup-step-health",
    failure.healthy ? "complete" : failure.serviceInstalled ? "failed" : "pending",
    failure.healthy ? "已通过" : failure.serviceInstalled ? "未通过" : "等待",
  );
  setSetupStep(
    "setup-step-integration",
    failure.integrationInstalled ? "complete" : failure.healthy ? "failed" : "pending",
    failure.integrationInstalled ? "已完成" : failure.healthy ? "未完成" : "等待",
  );

  const error = byId<HTMLElement>("local-setup-error");
  error.textContent = `${setupStageLabel(failure.stage)}：${failure.message}`;
  error.className = "form-message error";
  byId<HTMLButtonElement>("local-setup-retry").classList.remove("hidden");
  byId<HTMLButtonElement>("local-setup-close").classList.remove("hidden");
}

async function completeLocalSetup(): Promise<void> {
  providerSubmitting = true;
  byId<HTMLButtonElement>("provider-dialog-close").disabled = true;
  byId<HTMLButtonElement>("local-setup-retry").classList.add("hidden");
  byId<HTMLButtonElement>("local-setup-close").classList.add("hidden");
  byId<HTMLElement>("local-setup-error").classList.add("hidden");
  const mark = byId<HTMLElement>("local-setup-mark");
  mark.className = "setup-mark working";
  byId<HTMLElement>("local-setup-title").textContent = "正在一键接入 Codex";
  byId<HTMLElement>("local-setup-summary").textContent =
    "正在注册登录自启、启动路由并安全合并 Codex 配置。";

  try {
    const result = await call<LocalSetupResult>("complete_local_setup");
    setSetupStep(
      "setup-step-startup",
      result.serviceInstalled ? "complete" : "failed",
      result.serviceInstalled ? "已完成" : "未完成",
    );
    setSetupStep(
      "setup-step-health",
      result.healthy ? "complete" : "failed",
      result.healthy ? "已通过" : "未通过",
    );
    setSetupStep(
      "setup-step-integration",
      result.integrationInstalled ? "complete" : "failed",
      result.integrationInstalled ? "已完成" : "未完成",
    );
    mark.className = "setup-mark complete";
    byId<HTMLElement>("local-setup-title").textContent = "接入完成";
    const needsChatgptRestart = result.restartChatgptRequired || result.pickerPending;
    const successMessage = needsChatgptRestart
      ? "请完全退出并重新打开 ChatGPT，让 Codex 模型目录完成刷新。"
      : "本机路由与 Codex 配置已经生效。";
    byId<HTMLElement>("local-setup-summary").textContent = successMessage;
    byId<HTMLButtonElement>("local-setup-close").classList.remove("hidden");
    await refresh();
    setNotice(`接入完成。${successMessage}`);
  } catch (error) {
    renderSetupFailure(asLocalSetupFailure(error));
  } finally {
    providerSubmitting = false;
    byId<HTMLButtonElement>("provider-dialog-close").disabled = false;
  }
}

function selectedProviderPreset(): ProviderPreset | undefined {
  const selectedId = byId<HTMLSelectElement>("provider-preset").value;
  return providerPresets.find((preset) => preset.id === selectedId);
}

function setOptionalNumber(id: string, value: number | null): void {
  byId<HTMLInputElement>(id).value = value === null ? "" : String(value);
}

function readOptionalNumber(id: string): number | null {
  const value = byId<HTMLInputElement>(id).value.trim();
  if (value === "") return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
}

function applyProviderPreset(preset: ProviderPreset): void {
  const custom = preset.id === customProviderPreset.id;
  const providerId = byId<HTMLInputElement>("provider-id");
  providerId.value = custom ? "custom-provider" : preset.id;
  providerId.pattern = custom
    ? "[a-z0-9][a-z0-9-]*"
    : "[A-Za-z0-9][A-Za-z0-9._-]*";
  providerId.title = custom
    ? "只能使用小写字母、数字和连字符，并以小写字母或数字开头"
    : "使用字母、数字、点、下划线或连字符，并以字母或数字开头";
  byId<HTMLElement>("provider-id-hint").textContent = custom
    ? "自定义供应商 ID 只能包含小写字母、数字和连字符，例如 my-provider。"
    : "预设供应商可使用字母、数字、点、下划线和连字符。";
  byId<HTMLInputElement>("provider-base-url").value = preset.defaultBaseUrl;
  byId<HTMLInputElement>("provider-model-id").value = preset.defaultModel;
  byId<HTMLInputElement>("provider-upstream-model").value = preset.defaultModel;
  byId<HTMLInputElement>("provider-display-name").value = preset.defaultModel;
  upstreamModelCustomized = false;
  displayNameCustomized = false;
  setOptionalNumber("provider-context-window", preset.contextWindow);
  setOptionalNumber("provider-max-output", preset.maxOutputTokens);

  const apiKey = byId<HTMLInputElement>("provider-api-key");
  apiKey.disabled = !preset.requiresApiKey;
  apiKey.required = preset.requiresApiKey;
  apiKey.value = "";
  apiKey.placeholder = preset.requiresApiKey ? "输入内容不可见" : "此预设无需 API Key";
  byId<HTMLElement>("provider-api-key-hint").textContent = preset.requiresApiKey
    ? "密钥将保存到当前用户的操作系统凭据库。"
    : "该预设无需鉴权，API Key 输入已禁用。";
  byId<HTMLElement>("provider-preset-hint").textContent = custom
    ? "直接输入兼容服务的 Base URL、API Key 和模型 ID；其他参数可在高级设置中修改。"
    : "推荐地址和模型已自动填写；如服务商提供了其他地址，也可以直接修改。";
}

function renderProviderPresets(options: ProviderSetupOptions): void {
  const presets = options.presets.filter(
    (preset) => preset.id !== customProviderPreset.id,
  );
  const providedCustom = options.presets.find(
    (preset) => preset.id === customProviderPreset.id,
  );
  providerPresets = [...presets, providedCustom ?? customProviderPreset];

  const select = byId<HTMLSelectElement>("provider-preset");
  select.replaceChildren(
    ...providerPresets.map((preset) => {
      const option = document.createElement("option");
      option.value = preset.id;
      option.textContent = preset.label;
      return option;
    }),
  );

  const initial = providerPresets[0] ?? customProviderPreset;
  select.value = initial.id;
  applyProviderPreset(initial);
}

function closeProviderDialog(): void {
  if (providerSubmitting) return;
  byId<HTMLInputElement>("provider-api-key").value = "";
  clearProviderFormError();
  byId<HTMLDialogElement>("provider-dialog").close();
}

async function openProviderDialog(): Promise<void> {
  clearError();
  clearProviderFormError();
  byId<HTMLInputElement>("provider-allow-insecure").checked = false;
  const dialog = byId<HTMLDialogElement>("provider-dialog");
  const fieldset = byId<HTMLFieldSetElement>("provider-fieldset");
  const select = byId<HTMLSelectElement>("provider-preset");
  resetSetupProgress();
  dialog.showModal();
  fieldset.disabled = true;
  select.replaceChildren(new Option("正在读取预设…", ""));

  try {
    renderProviderPresets(await call<ProviderSetupOptions>("provider_setup_options"));
    fieldset.disabled = false;
    select.focus();
  } catch (error) {
    setProviderFormError(error);
    select.replaceChildren(new Option("预设读取失败", ""));
    byId<HTMLButtonElement>("provider-cancel").disabled = false;
    byId<HTMLButtonElement>("provider-dialog-close").disabled = false;
  }
}

async function submitProviderForm(event: SubmitEvent): Promise<void> {
  event.preventDefault();
  clearProviderFormError();

  const form = byId<HTMLFormElement>("provider-form");
  if (!form.reportValidity()) return;
  const preset = selectedProviderPreset();
  if (!preset) {
    setProviderFormError("请选择供应商预设。");
    return;
  }

  const apiKey = byId<HTMLInputElement>("provider-api-key");
  const fieldset = byId<HTMLFieldSetElement>("provider-fieldset");
  const submit = byId<HTMLButtonElement>("provider-submit");
  const originalSubmitText = submit.textContent;
  const input: AddProviderWithModelInput = {
    providerId: byId<HTMLInputElement>("provider-id").value.trim(),
    presetId: preset.id,
    baseUrl: byId<HTMLInputElement>("provider-base-url").value.trim(),
    apiKey: preset.requiresApiKey ? apiKey.value : "",
    secretProfile:
      byId<HTMLInputElement>("provider-secret-profile").value.trim() || "default",
    modelId: byId<HTMLInputElement>("provider-model-id").value.trim(),
    upstreamModel: byId<HTMLInputElement>("provider-upstream-model").value.trim(),
    displayName: byId<HTMLInputElement>("provider-display-name").value.trim(),
    contextWindow: readOptionalNumber("provider-context-window"),
    maxOutputTokens: readOptionalNumber("provider-max-output"),
    enabled: byId<HTMLInputElement>("provider-enabled").checked,
    allowInsecureHttp: byId<HTMLInputElement>("provider-allow-insecure").checked,
  };

  providerSubmitting = true;
  fieldset.disabled = true;
  byId<HTMLButtonElement>("provider-dialog-close").disabled = true;
  submit.textContent = "正在安全保存…";
  let saved = false;
  try {
    const result = await call<AddProviderWithModelResult>("add_provider_with_model", {
      input,
    });
    saved = true;
    dashboard.providers = result.providers;
    dashboard.models = result.models;
    renderProviders();
    renderModels();
  } catch (error) {
    setProviderFormError(error);
  } finally {
    if (saved) {
      // The key reached the OS credential store; never keep it in the form.
      apiKey.value = "";
      input.apiKey = "";
    } else {
      // Keep the typed key on failure so the user can retry the import
      // without re-entering it; it is still cleared when the dialog closes.
      providerSubmitting = false;
      fieldset.disabled = false;
      byId<HTMLButtonElement>("provider-dialog-close").disabled = false;
      submit.textContent = originalSubmitText;
    }
  }

  if (!saved) return;
  submit.textContent = originalSubmitText;
  beginSetupProgress();
  await completeLocalSetup();
}

byId<HTMLButtonElement>("refresh-button").addEventListener("click", () => void refresh());
byId<HTMLButtonElement>("service-button").addEventListener("click", () => void toggleService());
byId<HTMLButtonElement>("add-provider-button").addEventListener("click", () => {
  void openProviderDialog();
});
byId<HTMLButtonElement>("complete-setup-button").addEventListener("click", () => {
  void setupExistingConfiguration();
});
byId<HTMLButtonElement>("restore-codex-button").addEventListener("click", () => {
  void restoreCodexConfig();
});
byId<HTMLSelectElement>("provider-preset").addEventListener("change", () => {
  const preset = selectedProviderPreset();
  if (preset) applyProviderPreset(preset);
});
byId<HTMLInputElement>("provider-model-id").addEventListener("input", (event) => {
  const value = (event.currentTarget as HTMLInputElement).value;
  if (!upstreamModelCustomized) byId<HTMLInputElement>("provider-upstream-model").value = value;
  if (!displayNameCustomized) byId<HTMLInputElement>("provider-display-name").value = value;
});
byId<HTMLInputElement>("provider-upstream-model").addEventListener("input", () => {
  upstreamModelCustomized = true;
});
byId<HTMLInputElement>("provider-display-name").addEventListener("input", () => {
  displayNameCustomized = true;
});
byId<HTMLButtonElement>("provider-cancel").addEventListener("click", closeProviderDialog);
byId<HTMLButtonElement>("provider-dialog-close").addEventListener("click", closeProviderDialog);
byId<HTMLButtonElement>("local-setup-close").addEventListener("click", closeProviderDialog);
byId<HTMLFormElement>("model-edit-form").addEventListener("submit", (event) => {
  void submitModelEdit(event);
});
byId<HTMLButtonElement>("model-edit-cancel").addEventListener("click", () => {
  byId<HTMLDialogElement>("model-edit-dialog").close();
});
byId<HTMLButtonElement>("model-edit-close").addEventListener("click", () => {
  byId<HTMLDialogElement>("model-edit-dialog").close();
});
byId<HTMLFormElement>("provider-edit-form").addEventListener("submit", (event) => {
  void submitProviderEdit(event);
});
byId<HTMLButtonElement>("provider-edit-cancel").addEventListener("click", () => {
  byId<HTMLDialogElement>("provider-edit-dialog").close();
});
byId<HTMLButtonElement>("provider-edit-close").addEventListener("click", () => {
  byId<HTMLDialogElement>("provider-edit-dialog").close();
});
byId<HTMLButtonElement>("local-setup-retry").addEventListener("click", () => {
  setSetupStep("setup-step-startup", "working", "处理中");
  setSetupStep("setup-step-health", "pending", "等待");
  setSetupStep("setup-step-integration", "pending", "等待");
  void completeLocalSetup();
});
byId<HTMLDialogElement>("provider-dialog").addEventListener("close", () => {
  byId<HTMLInputElement>("provider-api-key").value = "";
});
byId<HTMLDialogElement>("provider-dialog").addEventListener("cancel", (event) => {
  if (providerSubmitting) event.preventDefault();
});
byId<HTMLFormElement>("provider-form").addEventListener("submit", (event) => {
  void submitProviderForm(event);
});

/* ---------- navigation, theme, settings wiring ---------- */

for (const item of document.querySelectorAll<HTMLButtonElement>("#main-nav .nav-item")) {
  item.addEventListener("click", () => {
    const view = item.dataset.view;
    if (view) switchView(view as ViewName);
  });
}
window.addEventListener("hashchange", () => switchView(initialView()));

byId<HTMLButtonElement>("theme-cycle-button").addEventListener("click", () => {
  const order: ThemeChoice[] = ["system", "light", "dark"];
  const next = order[(order.indexOf(currentTheme()) + 1) % order.length];
  if (next) applyTheme(next);
});
for (const button of document.querySelectorAll<HTMLButtonElement>("#theme-segment button")) {
  button.addEventListener("click", () => {
    const choice = button.dataset.themeChoice;
    if (choice === "system" || choice === "light" || choice === "dark") {
      applyTheme(choice);
    }
  });
}

byId<HTMLButtonElement>("releases-button").addEventListener("click", () => {
  void call("open_releases_page").catch((error) => setError(error));
});
byId<HTMLButtonElement>("releases-copy").addEventListener("click", () => {
  void copyText(RELEASES_URL);
});
byId<HTMLButtonElement>("config-path-copy").addEventListener("click", () => {
  void copyText(dashboard.configPath);
});
byId<HTMLButtonElement>("config-path-reveal").addEventListener("click", () => {
  void call("reveal_path", { path: dashboard.configPath }).catch((error) => setError(error));
});

async function loadVersion(): Promise<void> {
  let version = "—";
  try {
    version = await getVersion();
  } catch {
    /* browser preview */
  }
  byId<HTMLSpanElement>("app-version").textContent = `v${version}`;
  byId<HTMLSpanElement>("settings-version").textContent = `v${version}`;
}

applyTheme(currentTheme());
switchView(initialView());
void loadVersion();
void refresh();
