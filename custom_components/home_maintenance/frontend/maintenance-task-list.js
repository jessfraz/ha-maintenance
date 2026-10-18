(() => {
  const VIEWS = new Set(["active", "upcoming", "overdue", "completed"]);

  function element(tag, text, className) {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (className) node.className = className;
    return node;
  }

  function configuration(value) {
    if (!value || typeof value !== "object" || Array.isArray(value))
      throw new Error("Maintenance card configuration must be an object.");
    const title = value.title ?? "Maintenance";
    const view = value.view ?? "upcoming";
    const days = value.days ?? 14;
    const limit = value.limit ?? 20;
    const hideWhenEmpty = value.hide_when_empty === undefined ? false : value.hide_when_empty;
    if (typeof title !== "string" || !title.trim() || title.length > 200)
      throw new Error("Maintenance title must contain 1–200 characters.");
    if (!VIEWS.has(view)) throw new Error("Unknown Maintenance view.");
    if (!Number.isInteger(days) || days < 1 || days > 365)
      throw new Error("Maintenance days must be between 1 and 365.");
    if (!Number.isInteger(limit) || limit < 1 || limit > 100)
      throw new Error("Maintenance limit must be between 1 and 100.");
    if (typeof hideWhenEmpty !== "boolean")
      throw new Error("Maintenance hide_when_empty must be a boolean.");
    return { title: title.trim(), view, days, limit, hideWhenEmpty };
  }

  function validateResponse(data) {
    if (!data || !Array.isArray(data.items) || data.items.length > 100 ||
      !Number.isSafeInteger(data.total) || data.total < data.items.length ||
      (data.total > 0 && data.items.length === 0) ||
      typeof data.can_open !== "boolean" ||
      !(data.app_path === null || /^\/app\/[a-zA-Z0-9_-]+\/$/.test(data.app_path)))
      throw new Error("Invalid Maintenance response.");
    const ids = new Set();
    for (const item of data.items) {
      if (!item || typeof item.id !== "string" ||
        !/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(item.id) || item.id.length > 80 ||
        ids.has(item.id) || typeof item.summary !== "string" ||
        typeof item.name !== "string" || !["needs_action", "completed"].includes(item.status) ||
        !(item.due === null || /^\d{4}-\d{2}-\d{2}$/.test(item.due)) ||
        typeof item.timezone !== "string")
        throw new Error("Invalid Maintenance item.");
      new Intl.DateTimeFormat("en", { timeZone: item.timezone });
      if (item.due) {
        const due = new Date(`${item.due}T12:00:00Z`);
        if (!Number.isFinite(due.getTime()) || due.toISOString().slice(0, 10) !== item.due)
          throw new Error("Invalid Maintenance due date.");
      }
      ids.add(item.id);
    }
    if (data.can_open && data.app_path === null)
      throw new Error("Maintenance app route is unavailable.");
    return data;
  }

  function itemStatus(item) {
    if (item.status === "completed") return "Completed";
    if (!item.due) return "Active";
    const parts = new Intl.DateTimeFormat("en", {
      timeZone: item.timezone, year: "numeric", month: "2-digit", day: "2-digit",
    }).formatToParts(new Date());
    const part = (type) => parts.find((value) => value.type === type).value;
    const today = `${part("year")}-${part("month")}-${part("day")}`;
    return item.due < today ? "Overdue" : item.due === today ? "Due today" : "Active";
  }

  const STYLES = `
    :host { display: block; }
    :host([hidden]) { display: none; }
    ha-card { overflow: hidden; }
    .heading { display: flex; align-items: center; justify-content: space-between;
      gap: 16px; padding: 20px 20px 12px; }
    h2 { margin: 0; font-size: 20px; font-weight: 500; color: var(--primary-text-color); }
    button { font: inherit; color: var(--primary-text-color); background: transparent;
      border: 1px solid var(--divider-color); border-radius: 6px; cursor: pointer;
      padding: 7px 10px; flex-shrink: 0; }
    button[aria-disabled="true"] { opacity: .5; cursor: wait; }
    a:focus-visible, button:focus-visible { outline: 2px solid var(--primary-color);
      outline-offset: -2px; }
    ul { list-style: none; margin: 0; padding: 0 20px; }
    li + li { border-top: 1px solid var(--divider-color); }
    .row { display: flex; align-items: center; gap: 14px; padding: 16px 0;
      color: var(--primary-text-color); text-decoration: none; min-height: 44px; }
    a.row:hover { background: var(--secondary-background-color); }
    .content { flex: 1; min-width: 0; overflow-wrap: anywhere; }
    .summary { display: block; line-height: 1.4; }
    .date { display: block; margin-top: 5px; color: var(--secondary-text-color);
      font-size: 12px; font-family: var(--code-font-family, ui-monospace, monospace); }
    .status { color: var(--secondary-text-color); font-size: 12px; text-align: right; }
    .overdue { color: var(--error-color); }
    .completed { color: var(--success-color, var(--primary-color)); }
    .arrow { font-size: 22px; color: var(--secondary-text-color); }
    .message { margin: 0; padding: 12px 20px 20px; color: var(--secondary-text-color); }
    .footer { padding: 12px 20px 16px; color: var(--secondary-text-color); font-size: 12px; }
    .footer a { margin-inline-start: 12px; color: inherit; }
  `;

  class MaintenanceTaskList extends HTMLElement {
    constructor() {
      super();
      this.attachShadow({ mode: "open" });
      this._config = configuration({});
      this._hass = null;
      this._data = null;
      this._error = false;
      this._request = 0;
      this._pending = null;
      this._timer = null;
      this._preview = false;
      // Home Assistant must keep polling cards connected when their layout slot hides.
      this.connectedWhileHidden = true;
      this._visibilityChanged = () => {
        if (!document.hidden) void this.refresh();
      };
    }

    static getStubConfig() {
      return { title: "Maintenance upcoming", view: "upcoming", days: 14 };
    }

    set preview(value) {
      this._preview = Boolean(value);
      this._render();
    }

    get preview() {
      return this._preview;
    }

    setConfig(value) {
      this._config = configuration(value);
      this._invalidate();
      this._render();
      void this.refresh();
    }

    set hass(value) {
      if (this._hass?.user?.id !== value.user?.id) this._invalidate();
      this._hass = value;
      if (!this._data && !this._error && this._pending === null) void this.refresh();
    }

    connectedCallback() {
      this._timer = window.setInterval(() => {
        if (!document.hidden) void this.refresh();
      }, 60_000);
      document.addEventListener("visibilitychange", this._visibilityChanged);
      this._render();
      void this.refresh();
    }

    disconnectedCallback() {
      window.clearInterval(this._timer);
      this._timer = null;
      document.removeEventListener("visibilitychange", this._visibilityChanged);
      this._invalidate();
    }

    getCardSize() {
      if (this.hidden) return 0;
      return 1 + Math.max(1, this._data?.items.length ?? 3);
    }

    _navigationLink(path) {
      const link = element("a");
      link.href = path;
      link.addEventListener("click", (event) => {
        if (event.button || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey)
          return;
        event.preventDefault();
        this.dispatchEvent(new CustomEvent("hass-action", {
          bubbles: true, composed: true,
          detail: { action: "tap", config: {
            tap_action: { action: "navigate", navigation_path: path },
          } },
        }));
      });
      return link;
    }

    _invalidate() {
      this._request++;
      this._pending = null;
      this._data = null;
      this._error = false;
    }

    async refresh() {
      if (!this.isConnected || !this._hass || this._pending !== null) return;
      const request = ++this._request;
      this._pending = request;
      this._render();
      const query = new URLSearchParams({
        view: this._config.view,
        days: String(this._config.days),
        limit: String(this._config.limit),
      });
      try {
        const result = await this._hass.callApi("GET", `home_maintenance/card?${query}`);
        if (request !== this._request || !this.isConnected) return;
        this._data = validateResponse(result);
        this._error = false;
      } catch {
        if (request !== this._request || !this.isConnected) return;
        this._data = null;
        this._error = true;
      } finally {
        if (request === this._request && this.isConnected) {
          this._pending = null;
          this._render();
        }
      }
    }

    _render() {
      const hide = this._config.hideWhenEmpty && !this._preview &&
        this._pending === null && !this._error && this._data?.total === 0;
      if (this.hidden !== hide) {
        this.hidden = hide;
        // Hide HA's wrapper as well, so section grids and stacks leave no gap.
        this.dispatchEvent(new CustomEvent("card-visibility-changed", {
          bubbles: true, composed: true, detail: { value: !hide },
        }));
      }
      const focused = this.shadowRoot.activeElement;
      const focusedItem = focused?.dataset.itemId;
      const refreshFocused = focused?.dataset.refresh === "true";
      const style = element("style", STYLES);
      const card = element("ha-card");
      card.setAttribute("aria-busy", String(this._pending !== null));
      const heading = element("div", undefined, "heading");
      heading.append(element("h2", this._config.title));
      const refresh = element("button", "Refresh");
      refresh.type = "button";
      refresh.dataset.refresh = "true";
      refresh.setAttribute("aria-disabled", String(this._pending !== null));
      refresh.addEventListener("click", () => { void this.refresh(); });
      heading.append(refresh);
      card.append(heading);
      if (this._error || !this._data) {
        const message = element("p", this._error ? "Maintenance could not be refreshed." : "Loading…", "message");
        message.setAttribute("role", this._error ? "alert" : "status");
        card.append(message);
      } else {
        const data = this._data;
        if (!data.can_open) card.append(element("p", data.app_path
          ? "Administrator access is required to open the Maintenance app."
          : "Connect the Maintenance app to enable task links.", "message"));
        const list = element("ul");
        const locale = this._hass?.locale?.language || navigator.language;
        const date = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeZone: "UTC" });
        for (const item of data.items) {
          const row = element("li");
          const link = data.can_open
            ? this._navigationLink(`${data.app_path}items/${encodeURIComponent(item.id)}`)
            : element("div");
          link.className = "row";
          if (data.can_open) {
            link.dataset.itemId = item.id;
          }
          const content = element("div", undefined, "content");
          content.append(element("span", item.summary, "summary"));
          const due = item.due
            ? `Due ${date.format(new Date(`${item.due}T12:00:00Z`))}`
            : "No due date";
          content.append(element("span", due, "date"));
          const status = itemStatus(item);
          link.append(content, element("span", status,
            `status${status === "Overdue" ? " overdue" : status === "Completed" ? " completed" : ""}`));
          if (data.can_open) link.append(element("span", "›", "arrow"));
          row.append(link);
          list.append(row);
        }
        card.append(list);
        if (!data.items.length) card.append(element("p", "No matching tasks.", "message"));
        if (data.total > data.items.length) {
          const footer = element("div", `${data.items.length} of ${data.total} tasks`, "footer");
          if (data.can_open) {
            const all = this._navigationLink(data.app_path);
            all.textContent = "View all";
            footer.append(all);
          }
          card.append(footer);
        }
      }
      this.shadowRoot.replaceChildren(style, card);
      if (focusedItem)
        this.shadowRoot.querySelector(`[data-item-id="${focusedItem}"]`)?.focus({ preventScroll: true });
      else if (refreshFocused) refresh.focus({ preventScroll: true });
    }
  }
  if (!customElements.get("maintenance-task-list"))
    customElements.define("maintenance-task-list", MaintenanceTaskList);

  window.customCards ??= [];
  if (!window.customCards.some((card) => card.type === "maintenance-task-list")) {
    window.customCards.push({
      type: "maintenance-task-list",
      name: "Maintenance task list",
      description: "Open maintenance tasks directly in the Maintenance app.",
      preview: true,
    });
  }
})();
