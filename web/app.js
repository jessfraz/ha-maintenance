"use strict";

const byId = (id) => document.getElementById(id);
const base = new URL(document.baseURI);
const isEmbedded = window.parent !== window;
const isIngress = base.pathname.includes("/api/hassio_ingress/");
let token = "";
let connected = false;
let isOwner = false;
let canManageInstructions = false;
let defaultedDue = false;
let items = [];
let selected = null;
let editing = null;
let listGeneration = 0;
let detailGeneration = 0;
let eventsPage = null;
let historyBusyGeneration = null;
let historyRequest = 0;
let editorBusy = false;
let editorIdTouched = false;
let createRequest = null;
let loadedArchive = false;
let parentPrefix = null;
let metadataRowId = 0;
const completionRequests = new Map();
const completionBusy = new Set();
const checklistRequests = new Map();
const checklistBusy = new Set();

class ApiError extends Error {
  constructor(message, status) {
    super(message);
    this.status = status;
  }
}

function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  if (className) node.className = className;
  return node;
}

function notice(message, error = false) {
  byId("notice").textContent = message;
  byId("notice").className = error ? "error" : "";
}

function errorMessage(error) {
  if (error instanceof ApiError && error.status === 401)
    return connected
      ? "Your connection needs a valid token. Reload this tab to connect again."
      : "The token was not accepted. Check it and try again.";
  return error instanceof Error
    ? error.message
    : "The request could not be completed.";
}

async function api(path, options = {}) {
  const headers = new Headers(options.headers);
  headers.set("X-Maintenance-Request", "1");
  if (token) headers.set("Authorization", `Bearer ${token}`);
  if (options.body !== undefined)
    headers.set("Content-Type", "application/json");
  let response;
  try {
    response = await fetch(new URL(path, base), {
      ...options,
      headers,
      credentials: "same-origin",
      cache: "no-store",
    });
  } catch {
    throw new Error(
      "The service could not be reached. Check your connection and try again.",
    );
  }
  if (response.status === 204) return null;
  let body;
  try {
    body = await response.json();
  } catch {
    throw new ApiError(
      "The service returned an unreadable response. Refresh to check its current state.",
      response.status,
    );
  }
  if (!response.ok)
    throw new ApiError(
      body.message || `Request failed (${response.status}).`,
      response.status,
    );
  return body;
}

function guarded(action, errorTarget = null) {
  return async (event) => {
    event?.preventDefault();
    try {
      await action(event);
    } catch (error) {
      if (errorTarget) byId(errorTarget).textContent = errorMessage(error);
      else notice(errorMessage(error), true);
    }
  };
}

function today(timeZone) {
  const parts = new Intl.DateTimeFormat("en", {
    timeZone,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
  }).formatToParts(new Date());
  const part = (type) => parts.find((value) => value.type === type).value;
  return `${part("year")}-${part("month")}-${part("day")}`;
}

function dateLabel(value) {
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    year: "numeric",
    timeZone: "UTC",
  }).format(new Date(`${value}T12:00:00Z`));
}

function timestampLabel(value) {
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(value));
}

function actorLabel(actor) {
  return actor.kind === "system" ? `${actor.name} (system)` : actor.name;
}

function checklistProgress(steps) {
  return `${steps.filter((step) => step.completion).length} of ${steps.length} steps done`;
}

function itemBusy(item) {
  return completionBusy.has(item.id) || checklistBusy.has(item.id);
}

function itemPending(item) {
  return completionRequests.has(item.id) || checklistRequests.has(item.id);
}

function dueInfo(item) {
  if (item.schedule.kind === "instruction")
    return { label: "Standing instruction", className: "undated", days: null };
  const due = item.occurrence.due;
  if (item.status === "completed")
    return { label: "Completed", className: "", days: null };
  if (!due) return { label: "No due date", className: "undated", days: null };
  const days = Math.round(
    (Date.parse(`${due}T12:00:00Z`) -
      Date.parse(`${today(item.timezone)}T12:00:00Z`)) /
      86400000,
  );
  if (days < 0)
    return {
      label: `${-days} day${days === -1 ? "" : "s"} overdue`,
      className: "overdue",
      days,
    };
  if (days === 0) return { label: "Due today", className: "", days };
  if (days === 1) return { label: "Due tomorrow", className: "", days };
  return { label: `Due in ${days} days`, className: "", days };
}

function cadence(schedule) {
  if (schedule.kind === "once") return "One-off";
  if (schedule.kind === "standing") return "Standing task";
  if (schedule.kind === "instruction") return "Standing instruction";
  return (
    "Every " +
    ["years", "months", "days"]
      .filter((unit) => schedule.interval[unit] > 0)
      .map(
        (unit) =>
          `${schedule.interval[unit]} ${schedule.interval[unit] === 1 ? unit.slice(0, -1) : unit}`,
      )
      .join(", ")
  );
}

function itemPath(id) {
  return `items/${encodeURIComponent(id)}`;
}

function routeId() {
  const relative = location.pathname.startsWith(base.pathname)
    ? location.pathname.slice(base.pathname.length)
    : "";
  const match = /^items\/([a-z0-9]+(?:-[a-z0-9]+)*)\/?$/.exec(relative);
  return match ? match[1] : null;
}

function navigate(id, replace = false) {
  const path = id ? itemPath(id) : "";
  const url = new URL(path, base);
  if (location.pathname !== url.pathname)
    history[replace ? "replaceState" : "pushState"](null, "", url.pathname);
  if (parentPrefix)
    window.parent.postMessage(
      {
        type: "home-assistant/navigate",
        path: parentPrefix + (path ? `/${path}` : "/"),
        options: { replace },
      },
      location.origin,
    );
}

function safeLink(url, label) {
  try {
    const parsed = new URL(url);
    if (
      !["https:", "http:"].includes(parsed.protocol) ||
      parsed.username ||
      parsed.password
    )
      return null;
    const link = element("a", `${label} ↗`);
    link.href = parsed.href;
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    return link;
  } catch {
    return null;
  }
}

// Location and group often match ("Exterior"); show a shared value once.
function itemPlace(item) {
  return [...new Set([item.location, item.group].filter(Boolean))].join(" / ");
}

function renderSummary(summary) {
  const root = byId("summary");
  root.replaceChildren();
  for (const [key, label] of [
    ["active", "Active items"],
    ["overdue", "Overdue"],
    ["due_soon", "Due in 14 days"],
    ["completed", "Completions recorded"],
  ]) {
    const stat = element(
      "div",
      undefined,
      `summary-stat${key === "overdue" && summary[key] > 0 ? " attention" : ""}`,
    );
    stat.append(
      element("strong", new Intl.NumberFormat().format(summary[key])),
      element("span", label),
    );
    root.append(stat);
  }
}

function renderList() {
  const query = byId("query").value.trim().toLocaleLowerCase();
  const view = byId("view").value;
  const visible = items
    .filter((item) => {
      const matches = [item.name, item.summary, item.location, item.group]
        .filter(Boolean)
        .some((value) => value.toLocaleLowerCase().includes(query));
      const days = dueInfo(item).days;
      return (
        matches &&
        (view === "archived" || item.schedule.kind !== "instruction") &&
        (view === "archived" ||
          (view === "completed"
            ? item.status === "completed"
            : item.status === "needs_action")) &&
        (view !== "overdue" || (days !== null && days < 0)) &&
        (view !== "due_soon" || (days !== null && days >= 0 && days <= 14))
      );
    })
    .sort(
      (left, right) =>
        (left.occurrence.due ?? "9999-12-31").localeCompare(
          right.occurrence.due ?? "9999-12-31",
        ) ||
        left.name.localeCompare(right.name) ||
        left.id.localeCompare(right.id),
    );
  const root = byId("list");
  root.replaceChildren();
  byId("queue-title").textContent =
    view === "archived" ? "Archived" : view === "completed" ? "Completed" : "Queue";
  byId("list-status").textContent =
    `${visible.length} ${visible.length === 1 ? "item" : "items"}${query ? ` matching “${byId("query").value.trim()}”` : ""} · earliest due first`;
  if (!visible.length) {
    const empty = element("div", undefined, "empty-state");
    const title = query
      ? "No matching items"
      : view === "overdue"
        ? "Nothing overdue"
        : view === "due_soon"
          ? "Nothing due in the next two weeks"
          : view === "archived"
            ? "No archived items"
            : view === "completed" ? "No completed items" : "No items";
    const copy = query
      ? "Try a different task, item name, location, or group."
      : view === "active"
        ? isOwner
          ? "Add an item to start the queue."
          : "Items appear here when an owner adds them."
        : "Choose another view.";
    empty.append(element("h2", title), element("p", copy));
    root.append(empty);
  }
  for (const item of visible) {
    root.append(itemCard(item));
  }
  const instructions = items
    .filter((item) =>
      !item.archived && item.schedule.kind === "instruction" &&
      [item.name, item.summary, item.description, item.location, item.group]
        .filter(Boolean)
        .some((value) => value.toLocaleLowerCase().includes(query)),
    )
    .sort((left, right) => left.summary.localeCompare(right.summary));
  byId("standing-instructions").hidden = !instructions.length;
  byId("instruction-list").replaceChildren(...instructions.map(itemCard));
}

function itemCard(item) {
  const instruction = item.schedule.kind === "instruction";
  const card = element(
    "article",
    undefined,
    `item-card${item.archived ? " archived" : ""}${instruction ? " instruction-card" : ""}`,
  );
  const content = element("div");
  const place = itemPlace(item);
  if (place) content.append(element("p", place, "item-place"));
  const link = element("a", item.summary);
  link.href = new URL(itemPath(item.id), base).href;
  link.addEventListener("click", (event) => {
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey)
      return;
    event.preventDefault();
    openDetail(item.id).catch((error) => notice(errorMessage(error), true));
  });
  content.append(link, element("div", item.name, "item-name"));
  if (!instruction) content.append(
    element(
      "div",
      [
        cadence(item.schedule),
        item.occurrence.checklist.length
          ? checklistProgress(item.occurrence.checklist)
          : null,
      ]
        .filter(Boolean)
        .join(" · "),
      "item-meta",
    ),
  );
  if (instruction && item.description)
    content.append(element("p", item.description, "task-description"));
  const due = element("div", undefined, "item-due");
  const info = dueInfo(item);
  due.append(
    element(
      "span",
      item.archived ? "Archived" : info.label,
      `badge ${item.archived ? "archived" : info.className}`,
    ),
  );
  if (item.occurrence.due) {
    const time = element("time", dateLabel(item.occurrence.due));
    time.dateTime = item.occurrence.due;
    due.append(time);
  }
  card.append(content, due);
  return card;
}

async function load() {
  const generation = ++listGeneration;
  const archived = byId("view").value === "archived";
  byId("refresh").disabled = true;
  byId("list").setAttribute("aria-busy", "true");
  byId("list-status").textContent = "Loading maintenance…";
  try {
    const loadItems = async () => {
      const result = [];
      const cursors = new Set();
      let cursor = null;
      do {
        const params = new URLSearchParams({
          limit: "200",
          archived: String(archived),
        });
        if (cursor) params.set("page_token", cursor);
        const page = await api(`v1/items?${params}`);
        if (generation !== listGeneration) return null;
        result.push(...page.items);
        cursor = page.next_page;
        if (cursor && cursors.has(cursor))
          throw new Error(
            "The service repeated a list page. Refresh to try again.",
          );
        if (cursor) cursors.add(cursor);
      } while (cursor);
      return result;
    };
    const [loaded, summary, system] = await Promise.all([
      loadItems(),
      api("v1/summary"),
      api("v1/system"),
    ]);
    if (generation !== listGeneration || !loaded) return;
    items = loaded;
    loadedArchive = archived;
    renderSystem(system);
    renderSummary(summary);
    renderList();
    if (selected) renderDetail();
  } catch (error) {
    if (generation === listGeneration) {
      byId("list-status").textContent =
        "Maintenance could not be refreshed. The last loaded items may be out of date.";
      throw error;
    }
  } finally {
    if (generation === listGeneration) {
      byId("refresh").disabled = false;
      byId("list").setAttribute("aria-busy", "false");
    }
  }
}

function section(title) {
  const node = element("section", undefined, "detail-section");
  node.append(element("h3", title));
  return node;
}

function action(label, handler, className = "quiet") {
  const button = element("button", label, className);
  button.type = "button";
  button.addEventListener(
    "click",
    guarded(async () => {
      button.disabled = true;
      try {
        await handler();
      } finally {
        button.disabled = false;
      }
    }, "detail-error"),
  );
  return button;
}

function renderChecklist(item) {
  const steps = item.occurrence.checklist;
  const pending = checklistRequests.get(item.id);
  if (!steps.length && !pending) return null;
  const fieldset = element("fieldset", undefined, "occurrence-checklist");
  fieldset.append(element("legend", "Checklist"));
  fieldset.setAttribute("aria-busy", String(checklistBusy.has(item.id)));
  const heading = element("div", undefined, "checklist-progress");
  const progress = element("progress");
  progress.max = Math.max(steps.length, 1);
  progress.value = steps.filter((step) => step.completion).length;
  progress.setAttribute("aria-label", "Checklist progress");
  heading.append(progress, element("p", checklistProgress(steps), "muted"));
  fieldset.append(heading);
  const list = element("ol", undefined, "checklist-steps");
  for (const step of steps) {
    const row = element("li");
    const label = element("label", undefined, "checklist-step");
    const input = element("input");
    input.type = "checkbox";
    input.id = `checklist-${step.id}`;
    input.checked = Boolean(step.completion);
    input.disabled =
      item.archived ||
      item.status === "completed" ||
      itemBusy(item) ||
      itemPending(item);
    const text = element("span", undefined, "checklist-step-text");
    const summary = element("span", step.summary);
    summary.id = `checklist-label-${step.id}`;
    input.setAttribute("aria-labelledby", summary.id);
    text.append(summary);
    if (step.completion) {
      const attribution = element(
        "small",
        `Done by ${actorLabel(step.completion.actor)} · ${timestampLabel(step.completion.completed_at)}`,
      );
      attribution.id = `checklist-attribution-${step.id}`;
      input.setAttribute("aria-describedby", attribution.id);
      text.append(attribution);
    }
    label.append(input, text);
    input.addEventListener(
      "change",
      guarded(
        () => updateChecklist(item, step.id, input.checked),
        "detail-error",
      ),
    );
    row.append(label);
    list.append(row);
  }
  fieldset.append(list);
  if (pending && !checklistBusy.has(item.id)) {
    const box = element("div", undefined, "checklist-pending");
    const changed =
      pending.occurrenceId !== item.occurrence.id ||
      pending.revision !== item.revision;
    box.append(
      element(
        "p",
        changed
          ? "This item changed. Review the current checklist before making another update."
          : "The last step update has not been confirmed. Retry it before changing another step.",
        "muted",
      ),
    );
    const retry = action(
      changed ? "Review updated checklist" : "Retry step update",
      () => updateChecklist(item, pending.stepId, pending.completed),
    );
    retry.id = "retry-checklist";
    box.append(retry);
    fieldset.append(box);
  }
  return fieldset;
}

function checklistHistory(steps) {
  if (!steps.length) return null;
  const details = element("details", undefined, "history-checklist");
  details.append(element("summary", `Checklist · ${checklistProgress(steps)}`));
  const list = element("ul", undefined, "checklist-history");
  for (const step of steps) {
    const row = element("li");
    row.append(
      element("span", step.completion ? "Done" : "Not done", "step-result"),
      element("span", step.summary),
    );
    if (step.completion)
      row.append(
        element(
          "span",
          `${actorLabel(step.completion.actor)} · ${timestampLabel(step.completion.completed_at)}`,
          "step-attribution",
        ),
      );
    list.append(row);
  }
  details.append(list);
  return details;
}

function renderDetail() {
  const item = selected;
  if (!item) return;
  const instruction = item.schedule.kind === "instruction";
  byId("history-title").closest("section").hidden = instruction;
  byId("detail-title").textContent = item.summary;
  byId("detail-location").textContent = itemPlace(item);
  const root = byId("detail-content");
  root.replaceChildren();
  root.append(element("p", item.name, "detail-item-name"));
  if (item.description)
    root.append(element("p", item.description, "task-description"));
  if (instruction) {
    root.append(element(
      "p",
      "Standing instruction · Only the owner can edit or remove it.",
      "muted",
    ));
  } else {
    const timing = element("div", undefined, "timing");
    const due = element("div");
    due.append(
      element("span", item.archived ? "ARCHIVED" : "NEXT DUE"),
      element("strong", item.archived ? "Schedule paused" : dueInfo(item).label),
      element("p", item.occurrence.due
        ? `${dateLabel(item.occurrence.due)} · ${item.timezone}` : item.timezone),
    );
    const interval = element("div");
    interval.append(
      element("span", "SCHEDULE"),
      element("strong", cadence(item.schedule)),
    );
    timing.append(due, interval);
    root.append(timing);
  }
  const checklist = renderChecklist(item);
  if (checklist) root.append(checklist);
  if (item.instructions.length) {
    const instructions = section("Instructions");
    const list = element("ol", undefined, "instructions");
    for (const instruction of item.instructions)
      list.append(element("li", instruction));
    instructions.append(list);
    root.append(instructions);
  }
  if (item.specs.length) {
    const specs = section("Specifications");
    const list = element("dl", undefined, "specs");
    for (const spec of item.specs) {
      const row = element("div");
      row.append(element("dt", spec.label), element("dd", spec.value));
      list.append(row);
    }
    specs.append(list);
    root.append(specs);
  }
  if (item.resources.length || item.reorder_url) {
    const references = section("References");
    const links = element("div", undefined, "resources");
    for (const resource of item.resources) {
      const link = safeLink(resource.url, resource.label);
      if (link) links.append(link);
    }
    if (item.reorder_url) {
      const link = safeLink(item.reorder_url, "Reorder");
      if (link) links.append(link);
    }
    references.append(links);
    root.append(references);
  }
  const actions = byId("actions");
  actions.replaceChildren();
  if (
    !instruction &&
    !item.archived &&
    (item.status === "needs_action" || completionRequests.has(item.id))
  ) {
    const pending = completionRequests.get(item.id);
    const changed =
      pending &&
      (pending.occurrenceId !== item.occurrence.id ||
        pending.revision !== item.revision);
    const complete = action(
      changed
        ? "Review current occurrence"
        : pending
          ? "Retry completion"
          : "Mark maintenance done",
      () => completeItem(item),
      "",
    );
    complete.disabled = itemBusy(item) || checklistRequests.has(item.id);
    actions.append(complete);
    if (item.occurrence.checklist.some((step) => !step.completion)) {
      actions.append(
        element(
          "p",
          "Complete the maintenance item when ready. Any unfinished steps remain visible in its history.",
          "muted completion-help",
        ),
      );
    }
  }
  actions.append(action("Copy NFC link", () => copyItemLink(item)));
  if (isOwner && (!instruction || canManageInstructions)) {
    const edit = action("Edit", () => openEditor(item));
    edit.disabled = itemBusy(item) || itemPending(item);
    actions.append(edit);
    const archive = action(
      item.archived ? "Restore item" : instruction ? "Remove instruction" : "Archive",
      () => archiveItem(item),
      "quiet archive-action",
    );
    archive.disabled = itemBusy(item) || itemPending(item);
    actions.append(archive);
  }
}

async function copyItemLink(item) {
  if ((isEmbedded || isIngress) && !parentPrefix)
    throw new Error(
      "Open this item from the Home Assistant sidebar before copying its permanent NFC link.",
    );
  const url = parentPrefix
    ? new URL(`${parentPrefix}/${itemPath(item.id)}`, location.origin)
    : new URL(itemPath(item.id), base);
  try {
    await navigator.clipboard.writeText(url.href);
  } catch {
    throw new Error(
      "Clipboard access is unavailable. Open this item’s link in a browser and copy its address.",
    );
  }
  notice(
    "Item link copied. It contains no API token and can be written to an NFC tag.",
  );
  byId("detail-status").textContent =
    "NFC link copied. It contains no API token.";
}

async function openDetail(id, updateRoute = true) {
  const generation = ++detailGeneration;
  byId("detail-error").textContent = "";
  byId("detail-status").textContent = "";
  const item = await api(`v1/items/${encodeURIComponent(id)}`);
  if (generation !== detailGeneration) return;
  selected = item;
  eventsPage = null;
  byId("events").replaceChildren();
  byId("more-events").hidden = true;
  byId("events-status").textContent = "Loading history…";
  renderDetail();
  if (!byId("detail").open) byId("detail").showModal();
  if (updateRoute) navigate(id);
  if (item.schedule.kind !== "instruction") await loadEvents(generation);
}

function closeDetail(updateRoute = true) {
  ++detailGeneration;
  selected = null;
  byId("detail").close();
  if (updateRoute) navigate(null);
}

async function loadEvents(generation = detailGeneration) {
  if (!selected || historyBusyGeneration === generation) return;
  const id = selected.id;
  const params = new URLSearchParams({ limit: "50" });
  if (eventsPage) params.set("page_token", eventsPage);
  historyBusyGeneration = generation;
  const request = ++historyRequest;
  byId("more-events").disabled = true;
  try {
    const page = await api(
      `v1/items/${encodeURIComponent(id)}/events?${params}`,
    );
    if (generation !== detailGeneration || selected?.id !== id) return;
    for (const event of page.items) {
      const row = element("li");
      row.append(
        element("strong", event.actor ? `Completed by ${actorLabel(event.actor)}` : "Completion"),
      );
      if (event.completed_at) {
        const time = element("time", timestampLabel(event.completed_at));
        time.dateTime = event.completed_at;
        row.append(time);
      } else row.append(element("p", "Completion date unknown", "muted"));
      const dates = [
        event.previous_due ? `Previously due ${dateLabel(event.previous_due)}` : null,
        event.next_due ? `Next due ${dateLabel(event.next_due)}` : null,
      ].filter(Boolean);
      if (dates.length) row.append(element("p", dates.join(" · ")));
      const checklist = checklistHistory(event.checklist);
      if (checklist) row.append(checklist);
      byId("events").append(row);
    }
    eventsPage = page.next_page;
    byId("more-events").hidden = !eventsPage;
    byId("more-events").textContent = "Load more history";
    byId("events-status").textContent = byId("events").children.length
      ? ""
      : "No completions recorded yet.";
  } catch (error) {
    if (generation === detailGeneration) {
      byId("events-status").textContent =
        `History unavailable. ${errorMessage(error)}`;
      byId("more-events").hidden = false;
      byId("more-events").textContent = "Retry history";
    }
  } finally {
    if (request === historyRequest) {
      historyBusyGeneration = null;
      byId("more-events").disabled = false;
    }
  }
}

async function completeItem(item) {
  if (itemBusy(item) || checklistRequests.has(item.id)) return;
  let request = completionRequests.get(item.id);
  if (
    request &&
    (request.occurrenceId !== item.occurrence.id ||
      request.revision !== item.revision)
  ) {
    completionRequests.delete(item.id);
    byId("detail-error").textContent =
      "This item changed. Review its current due date before completing it again.";
    renderDetail();
    return;
  }
  if (!request) {
    request = {
      key: crypto.randomUUID(),
      occurrenceId: item.occurrence.id,
      revision: item.revision,
    };
    completionRequests.set(item.id, request);
  }
  completionBusy.add(item.id);
  byId("detail-error").textContent = "";
  renderDetail();
  let updated;
  try {
    updated = await api(`v1/items/${encodeURIComponent(item.id)}/completions`, {
      method: "POST",
      headers: {
        "If-Match": `"${request.revision}"`,
        "Idempotency-Key": request.key,
      },
      body: JSON.stringify({ occurrence_id: request.occurrenceId }),
    });
  } catch (error) {
    if (error instanceof ApiError && [409, 412].includes(error.status)) {
      completionRequests.delete(item.id);
      if (selected?.id === item.id) await openDetail(item.id, false);
      byId("detail-error").textContent =
        "This item changed or its occurrence was already completed. The latest version is shown. Review it before completing again.";
    } else {
      byId("detail-error").textContent =
        `${errorMessage(error)} If the completion response was lost, Retry completion safely checks the same occurrence.`;
    }
    return;
  } finally {
    completionBusy.delete(item.id);
    if (selected?.id === item.id) renderDetail();
  }
  completionRequests.delete(item.id);
  if (selected?.id === item.id) {
    ++detailGeneration;
    selected = updated;
    renderDetail();
    eventsPage = null;
    byId("events").replaceChildren();
    await loadEvents();
  }
  const result = updated.status === "completed"
    ? `${updated.summary} completed.`
    : updated.occurrence.due
      ? `${updated.summary} recorded. Next due ${dateLabel(updated.occurrence.due)}.`
      : `${updated.summary} recorded.`;
  notice(result);
  if (selected?.id === item.id)
    byId("detail-status").textContent = result;
  await load().catch((error) =>
    notice(
      `Maintenance was recorded, but the queue could not be refreshed. ${errorMessage(error)}`,
      true,
    ),
  );
}

async function updateChecklist(item, stepId, completed) {
  if (itemBusy(item) || item.archived || completionRequests.has(item.id))
    return;
  let request = checklistRequests.get(item.id);
  if (
    request &&
    (request.occurrenceId !== item.occurrence.id ||
      request.revision !== item.revision ||
      request.stepId !== stepId ||
      request.completed !== completed)
  ) {
    checklistRequests.delete(item.id);
    byId("detail-error").textContent =
      "Review the current checklist before making another change.";
    renderDetail();
    return;
  }
  if (!request) {
    request = {
      key: crypto.randomUUID(),
      stepId,
      completed,
      occurrenceId: item.occurrence.id,
      revision: item.revision,
    };
    checklistRequests.set(item.id, request);
  }
  const generation = detailGeneration;
  checklistBusy.add(item.id);
  byId("detail-error").textContent = "";
  byId("detail-status").textContent = "Saving checklist progress…";
  renderDetail();
  let updated;
  try {
    updated = await api(
      `v1/items/${encodeURIComponent(item.id)}/checklist/${encodeURIComponent(request.stepId)}`,
      {
        method: "PUT",
        headers: {
          "If-Match": `"${request.revision}"`,
          "Idempotency-Key": request.key,
        },
        body: JSON.stringify({
          occurrence_id: request.occurrenceId,
          completed: request.completed,
        }),
      },
    );
    checklistRequests.delete(item.id);
    if (selected?.id === item.id) {
      if (
        selected.revision === request.revision &&
        selected.occurrence.id === request.occurrenceId
      )
        selected = updated;
      byId("detail-status").textContent =
        `Checklist progress saved. ${checklistProgress(selected.occurrence.checklist)}.`;
    }
  } catch (error) {
    if (error instanceof ApiError && [404, 409, 412].includes(error.status)) {
      checklistRequests.delete(item.id);
      if (selected?.id === item.id) {
        await openDetail(item.id, false);
        byId("detail-error").textContent =
          "This item or checklist changed. Review the current occurrence before updating a step again.";
      }
    } else if (selected?.id === item.id) {
      byId("detail-error").textContent =
        `${errorMessage(error)} Retry step update to confirm the original change safely.`;
    }
    if (selected?.id === item.id) byId("detail-status").textContent = "";
  } finally {
    checklistBusy.delete(item.id);
    if (selected?.id === item.id && byId("detail").open) {
      const scroll = byId("detail").scrollTop;
      renderDetail();
      byId("detail").scrollTop = scroll;
      if (
        generation === detailGeneration &&
        document.activeElement === document.body
      ) {
        const focus = checklistRequests.has(item.id)
          ? byId("retry-checklist")
          : byId(`checklist-${stepId}`);
        focus?.focus({ preventScroll: true });
      }
    }
  }
  if (updated)
    await load().catch((error) =>
      notice(
        `Checklist progress was saved, but the queue could not be refreshed. ${errorMessage(error)}`,
        true,
      ),
    );
}

async function archiveItem(item) {
  if (
    item.schedule.kind === "instruction" && !item.archived &&
    !window.confirm("Remove this standing instruction? You can restore it from Archived.")
  ) return;
  let updated;
  try {
    updated = await api(`v1/items/${encodeURIComponent(item.id)}/archive`, {
      method: item.archived ? "DELETE" : "PUT",
      headers: { "If-Match": `"${item.revision}"` },
    });
  } catch (error) {
    if (error instanceof ApiError && error.status === 412) {
      if (selected?.id === item.id) await openDetail(item.id, false);
      byId("detail-error").textContent =
        "This item changed. Review the latest version before changing its archive state.";
    } else
      byId("detail-error").textContent =
        `${errorMessage(error)} Refresh the item before retrying to check its current archive state.`;
    return;
  }
  if (selected?.id === item.id) {
    selected =
      updated || (await api(`v1/items/${encodeURIComponent(item.id)}`));
    renderDetail();
  }
  const message = item.schedule.kind === "instruction"
    ? item.archived
      ? "Standing instruction restored."
      : "Standing instruction removed. You can restore it from Archived."
    : item.archived
      ? "Item restored to the maintenance queue."
      : "Item archived. Its history and NFC link are preserved.";
  notice(message);
  if (selected?.id === item.id)
    byId("detail-status").textContent = message;
  await load().catch((error) =>
    notice(
      `The item was ${item.archived ? "restored" : "archived"}, but the queue could not be refreshed. ${errorMessage(error)}`,
      true,
    ),
  );
}

function addMetadataRow(kind, label = "", value = "") {
  const row = element("div", undefined, "metadata-row");
  const rowId = ++metadataRowId;
  for (const [key, title, initial] of [
    ["label", "Label", label],
    ["value", kind === "resource" ? "URL" : "Value", value],
  ]) {
    const field = element("label", title);
    const input = element("input");
    input.id = `metadata-${rowId}-${key}`;
    input.name = `${kind}_${key}`;
    input.value = initial;
    input.required = true;
    input.maxLength = key === "label" ? 100 : kind === "resource" ? 2048 : 500;
    input.type = kind === "resource" && key === "value" ? "url" : "text";
    if (input.type === "url") input.placeholder = "https://";
    field.htmlFor = input.id;
    field.append(input);
    row.append(field);
  }
  const remove = element("button", "×", "quiet");
  remove.type = "button";
  remove.setAttribute(
    "aria-label",
    `Remove ${kind === "resource" ? "reference link" : "specification"}`,
  );
  remove.addEventListener("click", () => {
    row.remove();
    byId(kind === "resource" ? "add-resource" : "add-spec").focus();
  });
  row.append(remove);
  byId(kind === "resource" ? "resource-rows" : "spec-rows").append(row);
  return row;
}

function updateChecklistEditor() {
  const rows = [...byId("checklist-rows").children];
  rows.forEach((row, index) => {
    row.querySelector("label span").textContent = `Step ${index + 1}`;
    for (const direction of ["up", "down"]) {
      const button = row.querySelector(`[data-move="${direction}"]`);
      button.disabled =
        direction === "up" ? index === 0 : index === rows.length - 1;
      button.setAttribute("aria-label", `Move step ${index + 1} ${direction}`);
    }
    row
      .querySelector("[data-remove]")
      .setAttribute("aria-label", `Remove step ${index + 1}`);
  });
  byId("add-checklist-step").disabled = rows.length >= 100;
  byId("checklist-editor-status").textContent = rows.length
    ? `${rows.length} of 100 steps`
    : "No checklist steps yet.";
}

function addChecklistRow(step = { id: crypto.randomUUID(), summary: "" }) {
  if (byId("checklist-rows").children.length >= 100) return null;
  const row = element("div", undefined, "checklist-editor-row");
  row.dataset.stepId = step.id;
  const label = element("label");
  const input = element("input");
  input.id = `edit-checklist-${step.id}`;
  input.name = "checklist_summary";
  input.value = step.summary;
  input.required = true;
  input.maxLength = 200;
  label.htmlFor = input.id;
  label.append(element("span"), input);
  row.append(label);
  for (const [direction, text] of [
    ["up", "↑"],
    ["down", "↓"],
  ]) {
    const button = element("button", text, "quiet");
    button.type = "button";
    button.dataset.move = direction;
    button.addEventListener("click", () => {
      if (direction === "up" && row.previousElementSibling)
        row.previousElementSibling.before(row);
      if (direction === "down" && row.nextElementSibling)
        row.nextElementSibling.after(row);
      updateChecklistEditor();
      input.focus();
    });
    row.append(button);
  }
  const remove = element("button", "×", "quiet");
  remove.type = "button";
  remove.dataset.remove = "true";
  remove.addEventListener("click", () => {
    row.remove();
    updateChecklistEditor();
    byId("add-checklist-step").focus();
  });
  row.append(remove);
  byId("checklist-rows").append(row);
  updateChecklistEditor();
  return row;
}

function openEditor(item = null) {
  if (!isOwner) return;
  editing = item;
  createRequest = null;
  editorIdTouched = Boolean(item);
  const form = byId("edit-form");
  form.reset();
  byId("edit-title").textContent = item
    ? "Edit maintenance item"
    : "Add maintenance item";
  byId("save").textContent = item ? "Save changes" : "Add item";
  byId("save").disabled = false;
  byId("edit-error").textContent = "";
  byId("reload-edit").hidden = true;
  byId("spec-rows").replaceChildren();
  byId("resource-rows").replaceChildren();
  byId("checklist-rows").replaceChildren();
  for (const name of [
    "name",
    "summary",
    "description",
    "id",
    "location",
    "group",
    "timezone",
    "reorder_url",
  ])
    form.elements.namedItem(name).value = item?.[name] || "";
  const timezone =
    item?.timezone || Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
  form.elements.namedItem("timezone").value = timezone;
  form.elements.namedItem("due").value =
    item?.occurrence.due || "";
  form.elements.namedItem("instructions").value =
    item?.instructions.join("\n") || "";
  form.elements.namedItem("id").disabled = Boolean(item);
  for (const unit of ["years", "months", "days"])
    form.elements.namedItem(unit).value =
      item?.schedule.interval?.[unit] ?? 0;
  byId("schedule-kind").value = item?.schedule.kind ?? "once";
  defaultedDue = false;
  updateScheduleEditor();
  byId("days").setCustomValidity("");
  for (const spec of item?.specs || [])
    addMetadataRow("spec", spec.label, spec.value);
  for (const resource of item?.resources || [])
    addMetadataRow("resource", resource.label, resource.url);
  for (const step of item?.checklist || []) addChecklistRow(step);
  updateChecklistEditor();
  if (!byId("editor").open) byId("editor").showModal();
  byId("name").focus();
}

function metadataValues(kind) {
  const root = byId(kind === "resource" ? "resource-rows" : "spec-rows");
  return [...root.children].map((row) => {
    const label = row.querySelector(`[name="${kind}_label"]`).value.trim();
    const value = row.querySelector(`[name="${kind}_value"]`).value.trim();
    if (kind === "resource") {
      if (!safeLink(value, label))
        throw new Error("Reference links must start with https:// or http://.");
      return { label, url: value };
    }
    return { label, value };
  });
}

async function saveItem() {
  if (editorBusy) return;
  const form = byId("edit-form");
  const value = (name) => form.elements.namedItem(name).value.trim();
  const interval = {
    days: Number(value("days")),
    months: Number(value("months")),
    years: Number(value("years")),
  };
  byId("days").setCustomValidity(
    value("schedule-kind") !== "recurring" ||
      Object.values(interval).some((days) => days > 0)
      ? ""
      : "Choose a repeat interval of at least one day, month, or year.",
  );
  if (!form.reportValidity()) return;
  try {
    today(value("timezone"));
  } catch {
    byId("edit-error").textContent =
      "Enter a valid time zone, such as UTC or the zone reported by your system.";
    return;
  }
  const body = {
    name: value("name"),
    summary: value("summary"),
    description: value("description") || null,
    checklist: value("schedule-kind") === "instruction"
      ? []
      : [...byId("checklist-rows").children].map((row) => ({
          id: row.dataset.stepId,
          summary: row.querySelector("input").value.trim(),
        })),
    location: value("location") || null,
    group: value("group") || null,
    timezone: value("timezone"),
    due: value("schedule-kind") === "instruction" ? null : value("due") || null,
    schedule: value("schedule-kind") === "recurring"
      ? { kind: "recurring", interval } : { kind: value("schedule-kind") },
    instructions: value("instructions")
      .split("\n")
      .map((line) => line.trim())
      .filter(Boolean),
    specs: metadataValues("spec"),
    resources: metadataValues("resource"),
    reorder_url: value("schedule-kind") === "instruction"
      ? null : value("reorder_url") || null,
  };
  if (body.reorder_url && !safeLink(body.reorder_url, "Reorder")) {
    byId("edit-error").textContent =
      "The reorder link must start with https:// or http://.";
    return;
  }
  if (!editing) body.id = value("id");
  const current = editing;
  const payload = JSON.stringify(body);
  if (!current && createRequest?.payload !== payload)
    createRequest = { payload, key: crypto.randomUUID() };
  editorBusy = true;
  byId("item-fields").disabled = true;
  byId("save").disabled = true;
  byId("close-editor").disabled = true;
  byId("edit-error").textContent = "";
  let updated;
  try {
    updated = await api(
      current ? `v1/items/${encodeURIComponent(current.id)}` : "v1/items",
      {
        method: current ? "PATCH" : "POST",
        headers: current
          ? { "If-Match": `"${current.revision}"` }
          : { "Idempotency-Key": createRequest.key },
        body: payload,
      },
    );
  } catch (error) {
    if (error instanceof ApiError && error.status === 412) {
      byId("edit-error").textContent =
        "This item changed. Your draft is still here. Load the latest version to review its changes before editing again; this replaces your draft.";
      byId("reload-edit").hidden = false;
      byId("save").disabled = true;
    } else byId("edit-error").textContent = errorMessage(error);
    return;
  } finally {
    editorBusy = false;
    byId("item-fields").disabled = false;
    byId("save").disabled = !byId("reload-edit").hidden;
    byId("close-editor").disabled = false;
  }
  byId("editor").close();
  editing = null;
  createRequest = null;
  notice(current ? "Maintenance item updated." : "Maintenance item added.");
  const refreshes = await Promise.allSettled([load(), openDetail(updated.id)]);
  const failure = refreshes.find((result) => result.status === "rejected");
  if (failure)
    notice(
      `Your item was saved, but the display could not be refreshed. ${errorMessage(failure.reason)}`,
      true,
    );
}

function renderInstallation(data) {
  byId("installation-summary").textContent = data.integration_connected
    ? "Home Assistant connected"
    : "Pairing needed";
  const list = byId("installation-checks");
  list.replaceChildren();
  const backupLabels = {
    complete: "Complete",
    app_only: "App data only",
    missing: "Not included",
    outdated: "Out of date",
    unavailable: "Unavailable",
    standalone: "Managed outside Home Assistant",
  };
  for (const [label, value] of [
    ["Home Assistant", data.integration_connected ? "Connected" : "Not paired"],
    ["Backup coverage", backupLabels[data.backup.status] || "Unknown"],
    ["Version", data.version],
  ]) {
    const row = element("li");
    row.append(element("strong", label), element("span", value));
    list.append(row);
  }
  byId("backup-date").textContent = data.backup.last_backup
    ? `Last backup: ${timestampLabel(data.backup.last_backup * 1000)}`
    : "No backup has been recorded by the service.";
}

async function loadInstallation(check = false) {
  byId("check-installation").disabled = true;
  byId("installation-error").textContent = "";
  try {
    renderInstallation(
      await api(
        `v1/installation${check ? "/check" : ""}`,
        check ? { method: "POST" } : {},
      ),
    );
  } catch (error) {
    byId("installation-error").textContent = errorMessage(error);
  } finally {
    byId("check-installation").disabled = false;
  }
}

function renderSystem(system) {
  isOwner = system.credential_role === "owner";
  canManageInstructions = system.can_manage_instructions === true;
  byId("schedule-kind").querySelector('[value="instruction"]').disabled =
    !canManageInstructions;
  byId("add").hidden = !isOwner;
  byId("installation").hidden = !isOwner;
  byId("app-version").textContent = system.version;
  byId("service-status").textContent = system.integration_connected
    ? "Home Assistant connected"
    : "Home Assistant not paired";
  byId("app-footer").hidden = false;
}

async function connect() {
  renderSystem(await api("v1/system"));
  connected = true;
  byId("login").hidden = true;
  byId("dashboard").hidden = false;
  byId("token").value = "";
  notice("");
  await load();
  if (isOwner) await loadInstallation();
  const id = routeId();
  if (id) await openDetail(id, false);
}

byId("login").addEventListener(
  "submit",
  guarded(async () => {
    token = byId("token").value.trim();
    byId("connect").disabled = true;
    try {
      await connect();
    } catch (error) {
      if (!connected) token = "";
      throw error;
    } finally {
      byId("connect").disabled = false;
    }
  }),
);
function updateScheduleEditor() {
  const kind = byId("schedule-kind").value;
  const recurring = kind === "recurring";
  const instruction = kind === "instruction";
  const due = byId("due");
  due.required = recurring;
  due.disabled = instruction;
  due.closest("label").hidden = instruction;
  byId("due-label").textContent = recurring
    ? editing?.schedule.kind === "recurring" && editing.occurrence.due
      ? "Next due date" : "First due date"
    : "Due date (optional)";
  if (recurring && (!due.value || defaultedDue)) {
    try {
      due.value = today(byId("timezone").value.trim());
      defaultedDue = true;
    } catch {
      due.value = "";
    }
  }
  byId("checklist-editor-title").closest("section").hidden = instruction;
  byId("reorder-url").disabled = instruction;
  byId("reorder-url").closest("label").hidden = instruction;
  for (const field of byId("checklist-rows").querySelectorAll("input"))
    field.disabled = instruction;
  byId("interval-fields").hidden = !recurring;
  byId("schedule-help").textContent = {
    once: "Completion closes the task.",
    standing: "Completion starts another undated occurrence.",
    recurring: "Starts on the first due date, then repeats after completion.",
    instruction: "Always visible. No completion or due date. Only the owner can edit or remove it.",
  }[kind];
  for (const unit of ["years", "months", "days"])
    byId(unit).disabled = !recurring;
  byId("days").setCustomValidity("");
}

byId("schedule-kind").addEventListener("change", updateScheduleEditor);
byId("due").addEventListener("input", () => { defaultedDue = false; });
byId("timezone").addEventListener("change", () => {
  if (defaultedDue && byId("schedule-kind").value === "recurring") {
    byId("due").value = "";
    updateScheduleEditor();
  }
});
byId("add").addEventListener("click", () => openEditor());
byId("refresh").addEventListener(
  "click",
  guarded(async () => {
    notice("");
    await load();
  }),
);
byId("query").addEventListener("input", renderList);
byId("view").addEventListener(
  "change",
  guarded(async () => {
    if ((byId("view").value === "archived") !== loadedArchive) await load();
    else renderList();
  }),
);
byId("home").addEventListener("click", (event) => {
  if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
  event.preventDefault();
  byId("editor").close();
  closeDetail();
});
byId("close-detail").addEventListener("click", () => closeDetail());
byId("detail").addEventListener("cancel", (event) => {
  event.preventDefault();
  closeDetail();
});
byId("more-events").addEventListener("click", () => loadEvents());
byId("close-editor").addEventListener("click", () => {
  if (!editorBusy) byId("editor").close();
});
byId("editor").addEventListener("cancel", (event) => {
  if (editorBusy) event.preventDefault();
});
byId("edit-form").addEventListener("submit", guarded(saveItem, "edit-error"));
byId("reload-edit").addEventListener(
  "click",
  guarded(async () => {
    if (editing)
      openEditor(await api(`v1/items/${encodeURIComponent(editing.id)}`));
  }, "edit-error"),
);
byId("item-id").addEventListener("input", () => {
  editorIdTouched = true;
});
byId("name").addEventListener("input", () => {
  if (!editing && !editorIdTouched)
    byId("item-id").value = byId("name")
      .value.toLowerCase()
      .normalize("NFKD")
      .replace(/[\u0300-\u036f]/g, "")
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-|-$/g, "")
      .slice(0, 80)
      .replace(/-$/, "");
});
for (const unit of ["years", "months", "days"])
  byId(unit).addEventListener("input", () =>
    byId("days").setCustomValidity(""),
  );
byId("add-spec").addEventListener("click", () =>
  addMetadataRow("spec").querySelector("input").focus(),
);
byId("add-resource").addEventListener("click", () =>
  addMetadataRow("resource").querySelector("input").focus(),
);
byId("add-checklist-step").addEventListener("click", () =>
  addChecklistRow()?.querySelector("input").focus(),
);
byId("check-installation").addEventListener("click", () =>
  loadInstallation(true),
);
window.addEventListener(
  "popstate",
  guarded(async () => {
    if (!connected) return;
    byId("editor").close();
    const id = routeId();
    if (id) await openDetail(id, false);
    else closeDetail(false);
  }),
);

if (isEmbedded) {
  window.addEventListener(
    "message",
    guarded(async (event) => {
      if (
        event.source !== window.parent ||
        event.origin !== location.origin ||
        event.data?.type !== "home-assistant/properties"
      )
        return;
      const prefix = event.data.route?.prefix;
      if (typeof prefix === "string" && /^\/[a-zA-Z0-9/_-]+$/.test(prefix))
        parentPrefix = prefix.replace(/\/$/, "");
      const path = event.data.route?.path;
      if (
        typeof path !== "string" ||
        !/^\/(?:items\/[a-z0-9]+(?:-[a-z0-9]+)*)?\/?$/.test(path)
      )
        return;
      const next = new URL(path.replace(/^\//, ""), base);
      if (next.pathname === location.pathname) return;
      history.replaceState(null, "", next.pathname);
      if (!connected) return;
      const id = routeId();
      if (id) await openDetail(id, false);
      else closeDetail(false);
    }),
  );
  window.parent.postMessage(
    { type: "home-assistant/subscribe-properties" },
    location.origin,
  );
}

connect().catch((error) => {
  if (
    !connected &&
    error instanceof ApiError &&
    [401, 403].includes(error.status)
  ) {
    byId("login").hidden = false;
    byId("token").focus();
  } else notice(errorMessage(error), true);
});
