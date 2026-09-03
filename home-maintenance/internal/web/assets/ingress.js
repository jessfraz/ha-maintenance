(() => {
  "use strict";

  const root = document.documentElement;
  const ingressPath = (root.dataset.ingressPath || "").replace(/\/$/, "");
  let routePrefix = "";

  const innerRoute = () => {
    let path = window.location.pathname;
    if (ingressPath && path.startsWith(ingressPath)) {
      path = path.slice(ingressPath.length);
    }
    return path || "/";
  };

  if (window.parent !== window) {
    window.addEventListener("message", (event) => {
      if (event.source !== window.parent || event.data?.type !== "home-assistant/properties") {
        return;
      }
      routePrefix = event.data.route?.prefix || "";
      const requestedRoute = event.data.route?.path || "/";
      if (requestedRoute !== innerRoute()) {
        window.location.replace(`${ingressPath}${requestedRoute}`);
      }
    });

    window.parent.postMessage({ type: "home-assistant/subscribe-properties" }, "*");
  }

  document.addEventListener("click", (event) => {
    const link = event.target.closest("a[data-route]");
    if (!link || window.parent === window || !routePrefix) {
      return;
    }
    event.preventDefault();
    window.parent.postMessage(
      { type: "home-assistant/navigate", path: `${routePrefix}${link.dataset.route}` },
      "*",
    );
  });
})();
