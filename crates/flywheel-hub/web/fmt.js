// Shared number formats: token counts and dollar amounts read the same in
// the header's usage meter, the launcher, the sidebar, and the Flywheel panel.
"use strict";

window.FwHubFmt = (() => {
  // 950, "1.5k", "42k", "2.00M".
  function tokens(n) {
    n = n || 0;
    if (n < 1000) return String(n);
    if (n < 1e6) return `${(n / 1e3).toFixed(n < 1e4 ? 1 : 0)}k`;
    return `${(n / 1e6).toFixed(2)}M`;
  }

  // "$0", "$0.0040" (sub-cent spend stays visible), "$0.42", "$120".
  function dollars(usd) {
    if (!usd) return "$0";
    if (usd < 0.01) return `$${usd.toFixed(4)}`;
    if (usd < 100) return `$${usd.toFixed(2)}`;
    return `$${Math.round(usd)}`;
  }

  return { tokens, dollars };
})();
