// Sign-in page helpers. Public (served without the token), so it holds no
// app code: it only keeps the `#session=` deep link across signing in.
(function () {
  "use strict";
  var hash = window.location.hash;
  if (document.documentElement.hasAttribute("data-signed-in")) {
    // Signed in: replace this page (and the spent code or token URL in
    // history) with the hub.
    window.location.replace("/" + hash);
    return;
  }
  var form = document.querySelector("form[action='/login']");
  if (form && hash) form.setAttribute("action", "/login" + hash);
})();
