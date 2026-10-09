// @ts-check
/** @typedef {{input: number, output: number}} Tokens */
/** @typedef {{requests: Record<string, number>, tokens: Record<string, Tokens>, unknown_usage_requests: number}} Usage */
/** @typedef {{kind: string, unit: string, state: string, per_minute: number | null}} Limit */
/** @typedef {{id: string, capabilities: Record<string, unknown> | null, upstream_rate_limits: unknown, limits: Limit[], usage: Usage}} Model */
/** @typedef {{data: Model[], other_usage: {usage: Usage}[], observed_at: string, config_sha256: string, catalog_fetched_at: string | null, running_config_synced_at: string | null}} View */

/** @param {string} id @returns {HTMLElement} */
function element(id) {
  const node = document.getElementById(id);
  if (!(node instanceof HTMLElement)) throw new Error(`Missing element: ${id}`);
  return node;
}

/** @param {number} value @returns {string} */
function number(value) {
  return value.toLocaleString("zh-CN");
}

/** @param {Tokens} tokens @returns {number} */
function total(tokens) {
  return tokens.input + tokens.output;
}

/** @param {string | null} value @returns {string} */
function date(value) {
  return value ? new Date(value).toLocaleString("zh-CN") : "未提供";
}

/** @param {string} text @param {string | null} detail @returns {HTMLTableCellElement} */
function cell(text, detail = null) {
  const node = document.createElement("td");
  node.className = "number";
  node.textContent = text;
  if (detail) {
    const small = document.createElement("small");
    small.textContent = detail;
    node.append(small);
  }
  return node;
}

/** @param {Model} model @param {string} kind @returns {string} */
function limits(model, kind) {
  const values = model.limits.filter((limit) => limit.kind === kind);
  if (!values.length) return "未提供";
  return values
    .map((limit) => {
      if (limit.state === "unlimited") return "无限额";
      if (limit.state !== "limited" || limit.per_minute === null) return "未知";
      return `${number(limit.per_minute)} ${limit.unit}`;
    })
    .join(" / ");
}

/** @param {View} view @returns {void} */
function render(view) {
  const rows = view.data.map((model) => {
    const row = document.createElement("tr");
    const identity = cell("");
    const title = document.createElement("strong");
    title.textContent = model.id;
    identity.append(title);
    const capability = document.createElement("small");
    const supported = Object.entries(model.capabilities || {})
      .filter(([, value]) => value === true || value === "true")
      .map(([key]) => key);
    capability.textContent = supported.join(" · ") || "能力未提供";
    identity.append(capability);
    const details = document.createElement("details");
    const summary = document.createElement("summary");
    summary.textContent = "能力、限额与用量详情";
    const raw = document.createElement("pre");
    raw.textContent = JSON.stringify(
      {
        capabilities: model.capabilities,
        rate_limits: model.upstream_rate_limits,
        usage: model.usage,
      },
      null,
      2,
    );
    details.append(summary, raw);
    identity.append(details);
    const usage = model.usage;
    const reported = total(usage.tokens.last_minute_reported);
    const estimated = total(usage.tokens.last_minute_estimated);
    row.append(
      identity,
      cell(limits(model, "requests")),
      cell(limits(model, "tokens")),
      cell(number(usage.requests.last_minute)),
      cell(
        number(reported),
        `入 ${number(usage.tokens.last_minute_reported.input)} / 出 ${number(usage.tokens.last_minute_reported.output)}${estimated ? ` · 另有 ≈ ${number(estimated)} 估算` : ""}`,
      ),
      cell(
        `≈ ${number(total(usage.tokens.in_flight_estimated))}`,
        `入 ≈ ${number(usage.tokens.in_flight_estimated.input)} / 出 ≈ ${number(usage.tokens.in_flight_estimated.output)}`,
      ),
      cell(number(usage.requests.in_flight)),
    );
    return row;
  });
  element("models").replaceChildren(...rows);
  element("empty").hidden = rows.length > 0;
  if (!rows.length) element("empty").textContent = "当前配置没有模型。";
  const usage = [
    ...view.data.map((model) => model.usage),
    ...view.other_usage.map((item) => item.usage),
  ];
  element("requests").textContent = number(
    usage.reduce((sum, value) => sum + value.requests.total, 0),
  );
  element("reported").textContent = number(
    usage.reduce((sum, value) => sum + total(value.tokens.reported), 0),
  );
  element("estimated").textContent =
    `≈ ${number(usage.reduce((sum, value) => sum + total(value.tokens.estimated), 0))}`;
  element("active").textContent = number(
    usage.reduce((sum, value) => sum + value.requests.in_flight, 0),
  );
  element("failures").textContent =
    `${number(usage.reduce((sum, value) => sum + value.requests.failed + value.requests.cancelled + value.requests.abandoned, 0))} 个失败、取消或中断请求`;
  element("sync").textContent =
    `目录抓取：${date(view.catalog_fetched_at)} · 配置同步：${date(view.running_config_synced_at)}`;
  const unknown = usage.reduce(
    (sum, value) => sum + value.unknown_usage_requests,
    0,
  );
  element("observation").textContent =
    `最近观测：${date(view.observed_at)} · 配置 ${view.config_sha256.slice(0, 12)}${unknown ? ` · ${number(unknown)} 个请求的 token 用量不完整` : ""}`;
}

let generation = 0;
let apiKey = "";

/** @param {number} current @returns {Promise<void>} */
async function update(current) {
  try {
    const response = await fetch("/gateway/models", {
      headers: { Authorization: `Bearer ${apiKey}` },
      cache: "no-store",
      signal: AbortSignal.timeout(10000),
    });
    if (current !== generation) return;
    if (!response.ok)
      throw new Error(
        response.status === 401
          ? "网关 key 无效"
          : `更新失败 (${response.status})`,
      );
    /** @type {View} */
    const view = await response.json();
    if (current !== generation) return;
    render(view);
    element("state").textContent = "实时观测";
    element("state").className = "state live";
  } catch (error) {
    if (current !== generation) return;
    element("state").textContent =
      error instanceof Error ? error.message : "更新失败";
    element("state").className = "state error";
  } finally {
    if (current === generation)
      window.setTimeout(() => void update(current), 3000);
  }
}

element("connect").addEventListener("submit", (event) => {
  event.preventDefault();
  const input = element("key");
  if (!(input instanceof HTMLInputElement))
    throw new Error("Missing key input");
  apiKey = input.value;
  input.value = "";
  generation += 1;
  element("state").textContent = "连接中";
  element("state").className = "state";
  void update(generation);
});
