// Prototype "init script" shim: rewrite requests that target dsh-app://app to the
// loopback app server. Installed through Tauri's `initialization_script`, i.e. the
// same hook the real bridge will use (runs before any page script).
//
// This file is a spike artifact, not production code: it records what it did in
// `window.__dshShim.log` so the page can report it back.
(function () {
  "use strict";

  var BASE = "http://127.0.0.1:__PORT__";
  var APP_ORIGIN = "dsh-app://app";

  var nativeFetch = window.fetch ? window.fetch.bind(window) : null;
  var NativeXHR = window.XMLHttpRequest;
  var NativeEventSource = window.EventSource;

  var log = [];
  function rec(entry) {
    try { log.push(entry); } catch (_) {}
  }

  // Return the loopback URL for an app-origin URL, or null when nothing must change.
  function rewrite(u) {
    try {
      var url = new URL(String(u), location.href);
      if (url.origin === APP_ORIGIN) {
        return BASE + url.pathname + url.search + url.hash;
      }
      return null;
    } catch (e) {
      rec({ api: "rewrite", input: String(u), error: String(e && e.name) + ": " + String(e && e.message) });
      return null;
    }
  }

  // ------------------------------------------------------------------ fetch
  // `Request` bodies in WKWebView can only be re-sent by buffering them: handing
  // the original ReadableStream to a new Request is rejected by WebKit
  // ("NotSupportedError: ReadableStream uploading is not supported") and, worse,
  // that attempt consumes the one-shot stream so no retry is possible afterwards.
  // So: no body -> rebuild directly; body -> clone() + arrayBuffer() first.
  function copyInit(input, entry) {
    var mode = input.mode;
    if (mode !== "cors") {
      // A rewritten URL is cross-origin, so 'same-origin' / 'no-cors' / 'navigate'
      // must become 'cors' or the rebuilt Request fails (and 'no-cors' would make
      // the response opaque, i.e. unreadable).
      entry.mode_overridden = mode + "->cors";
      mode = "cors";
    }
    var cache = input.cache === "only-if-cached" ? "default" : input.cache;
    return {
      method: input.method,
      headers: new Headers(input.headers),
      mode: mode,
      credentials: input.credentials,
      cache: cache,
      redirect: input.redirect,
      referrerPolicy: input.referrerPolicy,
      signal: input.signal,
    };
  }

  function hasBody(request) {
    return !(request.method === "GET" || request.method === "HEAD");
  }

  function rebuildRequest(input, target, init, entry) {
    var initCopy = copyInit(input, entry);
    if (!hasBody(input)) {
      entry.strategy = "direct-rebuild-no-body";
      return nativeFetch(new Request(target, initCopy), init);
    }
    var clone = input.clone();
    entry.strategy = "clone+arrayBuffer";
    return clone.arrayBuffer().then(function (buf) {
      entry.body_buffered_bytes = buf.byteLength;
      initCopy.body = buf;
      return nativeFetch(new Request(target, initCopy), init);
    });
  }

  function shimFetch(input, init) {
    var entry = { api: "fetch" };
    var result;
    try {
      if (typeof Request !== "undefined" && input instanceof Request) {
        entry.kind = "Request";
        entry.url = input.url;
        entry.method = input.method;
        entry.mode = input.mode;
        var target = rewrite(input.url);
        if (target === null) {
          entry.passthrough = true;
          result = nativeFetch(input, init);
        } else {
          entry.rewritten = target;
          result = rebuildRequest(input, target, init, entry);
        }
      } else {
        var raw = typeof input === "string" ? input : input && input.href ? input.href : String(input);
        entry.kind = typeof input === "string" ? "string" : input && input.href ? "URL" : typeof input;
        entry.url = raw;
        var rewritten = rewrite(raw);
        if (rewritten === null) {
          entry.passthrough = true;
          result = nativeFetch(input, init);
        } else {
          entry.rewritten = rewritten;
          entry.strategy = "string";
          result = nativeFetch(rewritten, init);
        }
      }
      return Promise.resolve(result).catch(function (err) {
        entry.async_error = String(err && err.name) + ": " + String(err && err.message);
        throw err;
      });
    } catch (e) {
      entry.error = String(e && e.name) + ": " + String(e && e.message);
      throw e;
    } finally {
      rec(entry);
    }
  }

  // -------------------------------------------------------------------- XHR
  // The native `open()` is deferred to `send()` so a relative URL can still be
  // resolved against `location.href`, which also means `setRequestHeader` must be
  // buffered and replayed after the real open.
  function ShimXHR() {
    var xhr = new NativeXHR();
    var nativeOpen = xhr.open;
    var nativeSetRequestHeader = xhr.setRequestHeader;
    var nativeSend = xhr.send;
    var pending = null;
    var pendingHeaders = [];
    var wrapped = false;

    try {
      xhr.open = function (method, url) {
        pending = { method: String(method), url: String(url), args: Array.prototype.slice.call(arguments) };
        rec({ api: "xhr.open", method: String(method), url: String(url) });
      };
      xhr.setRequestHeader = function (name, value) {
        pendingHeaders.push([String(name), String(value)]);
        rec({ api: "xhr.setRequestHeader", name: String(name), buffered: true });
      };
      xhr.send = function (body) {
        try {
          var args = pending ? pending.args.slice() : ["GET", pending ? pending.url : ""];
          var target = rewrite(args[1]);
          if (target !== null) {
            args[1] = target;
            rec({ api: "xhr.send", rewritten: target, body_kind: Object.prototype.toString.call(body) });
          } else {
            rec({ api: "xhr.send", passthrough: true, url: String(args[1]) });
          }
          nativeOpen.apply(xhr, args);
          for (var i = 0; i < pendingHeaders.length; i++) {
            nativeSetRequestHeader.call(xhr, pendingHeaders[i][0], pendingHeaders[i][1]);
          }
          rec({ api: "xhr.send", headers_replayed: pendingHeaders.length });
        } catch (e) {
          rec({ api: "xhr.send", error: String(e && e.name) + ": " + String(e && e.message) });
          throw e;
        }
        return nativeSend.call(xhr, body);
      };
      wrapped = xhr.open !== nativeOpen && xhr.send !== nativeSend;
      rec({ api: "xhr.wrap", instance_writable: wrapped, set_request_header_buffered: true });
    } catch (e) {
      rec({ api: "xhr.wrap", error: String(e && e.name) + ": " + String(e && e.message) });
    }
    return xhr;
  }
  try {
    window.XMLHttpRequest = ShimXHR;
  } catch (e) {
    rec({ api: "xhr.install", error: String(e && e.name) + ": " + String(e && e.message) });
  }

  // One-time probe: does WKWebView accept a ReadableStream request body at all?
  // (Recorded instead of attempted per request, because the attempt would consume
  // the caller's one-shot body stream.)
  try {
    var probe = new Request(BASE + "/__stream-body-probe", {
      method: "POST",
      body: new ReadableStream({ start: function () {} }),
      duplex: "half",
    });
    rec({ api: "probe.stream-body", constructed: true, note: "Request ctor accepted a ReadableStream body; fetch(=) may still reject" });
  } catch (probeErr) {
    rec({ api: "probe.stream-body", constructed: false, error: String(probeErr && probeErr.name) + ": " + String(probeErr && probeErr.message) });
  }

  if (nativeFetch) window.fetch = shimFetch;

  // Escape hatch: the page (and the future bridge) can always reach the native
  // APIs, e.g. to talk to the app's own scheme handler.
  window.__dshShim = {
    base: BASE,
    appOrigin: APP_ORIGIN,
    rewrite: rewrite,
    nativeFetch: nativeFetch,
    nativeXHR: NativeXHR,
    nativeEventSource: NativeEventSource,
    log: log,
    installedAtHref: location.href,
    installedAtProtocol: location.protocol,
    fetchReplaced: window.fetch === shimFetch,
    xhrReplaced: window.XMLHttpRequest === ShimXHR,
  };
  rec({
    api: "install",
    base: BASE,
    href: location.href,
    protocol: location.protocol,
    hasFetch: !!nativeFetch,
    hasEventSource: !!NativeEventSource,
    hasXHR: !!NativeXHR,
  });
})();
