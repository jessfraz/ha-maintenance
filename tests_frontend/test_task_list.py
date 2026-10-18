"""Exercise the shipped task card's visibility and polling in Chromium."""

import asyncio
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

import pytest
from playwright.async_api import (
    Browser,
    Error,
    Locator,
    Page,
    Route,
    async_playwright,
    expect,
)

from custom_components.home_maintenance.card import CardItem, CardResponse
from custom_components.home_maintenance.models import ItemStatus

CARD_SCRIPT = (
    Path(__file__).parents[1]
    / "custom_components/home_maintenance/frontend/maintenance-task-list.js"
)
EMPTY: CardResponse = {
    "app_path": "/app/example_maintenance/",
    "can_open": True,
    "items": [],
    "total": 0,
}
ITEM: CardItem = {
    "id": "inspect-filter",
    "name": "Example appliance",
    "summary": "Inspect filter",
    "status": ItemStatus.NEEDS_ACTION,
    "due": "2026-10-30",
    "timezone": "UTC",
}
POPULATED: CardResponse = {**EMPTY, "items": [ITEM], "total": 1}


@dataclass
class CardRequest:
    """One real browser HTTP request, held until the test supplies its response."""

    route: Route
    response: asyncio.Future[tuple[int, CardResponse | dict[str, object]]]
    finished: asyncio.Event = field(default_factory=asyncio.Event)

    @property
    def query(self) -> dict[str, list[str]]:
        return parse_qs(urlsplit(self.route.request.url).query)

    async def reply(
        self, data: CardResponse | dict[str, object] = EMPTY, status: int = 200
    ) -> None:
        self.response.set_result((status, data))
        await self.finished.wait()


@dataclass
class TaskCard:
    page: Page
    requests: asyncio.Queue[CardRequest] = field(default_factory=asyncio.Queue)

    @property
    def host(self) -> Locator:
        return self.page.locator("maintenance-task-list")

    async def handle(self, route: Route) -> None:
        request = CardRequest(route, asyncio.get_running_loop().create_future())
        await self.requests.put(request)
        status, data = await request.response
        await route.fulfill(status=status, json=data)
        request.finished.set()

    async def next_request(self) -> CardRequest:
        return await asyncio.wait_for(self.requests.get(), timeout=5)

    async def mount(self, *, hide: bool | None = True, preview: bool = False) -> None:
        await self.page.evaluate(
            """({ hide, preview }) => {
              const card = document.createElement('maintenance-task-list');
              const config = { title: 'Work queue', view: 'upcoming', days: 30 };
              if (hide !== null) config.hide_when_empty = hide;
              card.setConfig(config);
              card.preview = preview;
              card.hass = {
                user: { id: 'example-user' }, locale: { language: 'en' },
                callApi: async (method, path) => {
                  const response = await fetch(`/api/${path}`, { method });
                  if (!response.ok) throw new Error(`HTTP ${response.status}`);
                  return response.json();
                },
              };
              document.querySelector('#queue').append(card);
            }""",
            {"hide": hide, "preview": preview},
        )


@pytest.fixture
async def browser(socket_enabled: None) -> AsyncIterator[Browser]:
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        yield browser
        await browser.close()


@pytest.fixture
async def card(browser: Browser) -> AsyncIterator[TaskCard]:
    page = await browser.new_page()
    await page.clock.install()
    await page.route(
        "http://maintenance.test/",
        lambda route: route.fulfill(
            content_type="text/html",
            body="""<!doctype html><style>
              body { margin: 0; } main { display: flex; flex-direction: column; }
              nav, aside { padding: 20px; } ha-card { display: block; }
            </style><main><nav>App navigation</nav><div id="queue"></div>
              <aside>Overdue warning</aside></main>""",
        ),
    )
    task_card = TaskCard(page)
    await page.route("**/api/home_maintenance/card?*", task_card.handle)
    await page.goto("http://maintenance.test/")
    await page.add_script_tag(path=CARD_SCRIPT)
    yield task_card
    await page.close()


async def test_loading_stays_visible_then_empty_removes_host_layout_space(
    card: TaskCard,
) -> None:
    await card.mount()
    request = await card.next_request()
    assert request.query == {"view": ["upcoming"], "days": ["30"], "limit": ["20"]}
    await expect(card.host.get_by_role("heading", name="Work queue")).to_be_visible()
    await expect(card.host.get_by_role("status")).to_have_text("Loading…")
    await request.reply()
    await expect(card.host).to_be_hidden()
    await expect(card.host).to_have_attribute("hidden", "")
    assert await card.host.bounding_box() is None
    assert await card.host.evaluate("card => card.getCardSize()") == 0
    assert await card.host.evaluate("card => card.connectedWhileHidden") is True
    nav = await card.page.get_by_role("navigation").bounding_box()
    warning = await card.page.get_by_text("Overdue warning").bounding_box()
    assert nav is not None and warning is not None
    assert warning["y"] == nav["y"] + nav["height"]
    await expect(card.page.get_by_text("App navigation")).to_be_visible()
    await expect(card.page.get_by_text("Overdue warning")).to_be_visible()


async def test_hidden_card_polls_and_returns_when_tasks_enter_window(
    card: TaskCard,
) -> None:
    await card.mount()
    await (await card.next_request()).reply()
    await expect(card.host).to_be_hidden()
    await card.page.clock.fast_forward(60_000)
    poll = await card.next_request()
    await expect(card.host).to_be_visible()  # Pending refresh must be visible.
    await poll.reply(POPULATED)
    await expect(card.host.get_by_role("link", name="Inspect filter")).to_be_visible()
    assert await card.host.evaluate("card => card.getCardSize()") > 0
    await card.page.clock.fast_forward(60_000)
    await (await card.next_request()).reply()
    await expect(card.host).to_be_hidden()


@pytest.mark.parametrize(
    ("status", "payload"),
    [
        (503, EMPTY),
        (200, {**EMPTY, "items": [ITEM], "total": 0}),
        (200, {**EMPTY, "total": 1}),
    ],
    ids=["http-error", "invalid-response", "positive-total-without-items"],
)
async def test_failed_refresh_reveals_card_and_empty_recovery_hides_it(
    card: TaskCard, status: int, payload: CardResponse | dict[str, object]
) -> None:
    await card.mount()
    await (await card.next_request()).reply()
    await expect(card.host).to_be_hidden()
    await card.page.clock.fast_forward(60_000)
    await (await card.next_request()).reply(payload, status)
    await expect(card.host).to_be_visible()
    await expect(card.host.get_by_role("alert")).to_have_text(
        "Maintenance could not be refreshed."
    )
    await card.page.clock.fast_forward(60_000)
    await (await card.next_request()).reply()
    await expect(card.host).to_be_hidden()


@pytest.mark.parametrize(("hide", "preview"), [(None, False), (True, True)])
async def test_empty_default_and_editor_preview_remain_visible(
    card: TaskCard, hide: bool | None, preview: bool
) -> None:
    await card.mount(hide=hide, preview=preview)
    await (await card.next_request()).reply()
    await expect(card.host.get_by_text("No matching tasks.")).to_be_visible()
    await expect(card.host.get_by_role("heading", name="Work queue")).to_be_visible()
    assert await card.host.evaluate("card => card.getCardSize()") > 0


async def test_preview_visibility_uses_native_bubbling_card_event(
    card: TaskCard,
) -> None:
    await card.mount()
    await (await card.next_request()).reply()
    await expect(card.host).to_be_hidden()
    shown = await card.host.evaluate(
        """card => new Promise(resolve => {
          card.parentElement.addEventListener('card-visibility-changed', event => {
            resolve({ value: event.detail.value, composed: event.composed });
          }, { once: true });
          card.preview = true;
        })"""
    )
    assert shown == {"value": True, "composed": True}
    await expect(card.host.get_by_text("No matching tasks.")).to_be_visible()
    hidden = await card.host.evaluate(
        """card => new Promise(resolve => {
          card.addEventListener('card-visibility-changed', event => {
            resolve(event.detail.value);
          }, { once: true });
          card.preview = false;
        })"""
    )
    assert hidden is False
    await expect(card.host).to_be_hidden()


async def test_old_window_response_cannot_hide_current_window(card: TaskCard) -> None:
    await card.mount()
    old_request = await card.next_request()
    await card.host.evaluate(
        """card => card.setConfig({ title: 'Work queue', view: 'upcoming',
          days: 7, hide_when_empty: true })"""
    )
    current_request = await card.next_request()
    assert current_request.query["days"] == ["7"]
    await current_request.reply(POPULATED)
    await expect(card.host.get_by_role("link", name="Inspect filter")).to_be_visible()
    await old_request.reply()
    await card.page.wait_for_load_state("networkidle")
    await expect(card.host.get_by_role("link", name="Inspect filter")).to_be_visible()
    await expect(card.host).not_to_have_attribute("hidden", "")


async def test_disconnect_cleans_polling_and_reconnect_refreshes(
    card: TaskCard,
) -> None:
    await card.mount()
    await (await card.next_request()).reply()
    await expect(card.host).to_be_hidden()
    handle = await card.host.element_handle()
    assert handle is not None
    await handle.evaluate("card => card.remove()")
    await card.page.clock.fast_forward(180_000)
    await card.page.wait_for_load_state("networkidle")
    assert card.requests.empty()
    await handle.evaluate("card => document.querySelector('#queue').append(card)")
    request = await card.next_request()
    await expect(card.host).to_be_visible()
    await request.reply(POPULATED)
    await expect(card.host.get_by_role("link", name="Inspect filter")).to_be_visible()
    await card.page.clock.fast_forward(60_000)
    await (await card.next_request()).reply()
    await expect(card.host).to_be_hidden()
    await handle.dispose()


async def test_hide_when_empty_requires_a_boolean(card: TaskCard) -> None:
    await card.mount(hide=None)
    await (await card.next_request()).reply()
    await expect(card.host.get_by_text("No matching tasks.")).to_be_visible()
    with pytest.raises(Error, match="hide_when_empty must be a boolean"):
        await card.host.evaluate("card => card.setConfig({ hide_when_empty: 'true' })")
    await expect(card.host.get_by_text("No matching tasks.")).to_be_visible()
