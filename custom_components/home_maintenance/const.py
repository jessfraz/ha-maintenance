"""Constants for the Maintenance adapter."""

DOMAIN = "home_maintenance"
CONF_BACKEND_URL = "backend_url"
CONF_TOKEN = "token"
CONF_ADDON_SLUG = "addon_slug"
DEFAULT_BACKEND_URL = ""
MAX_REQUEST_BYTES = 64 * 1024
MAX_RESPONSE_BYTES = 1024 * 1024
MAX_REGISTRY_BYTES = 8 * 1024 * 1024
REQUEST_TIMEOUT_SECONDS = 15
SUMMARY_KEYS = (
    "active",
    "overdue",
    "due_soon",
    "completed",
)
