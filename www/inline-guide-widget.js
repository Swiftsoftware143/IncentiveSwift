/**
 * Inline Guide Widget v1.0 — per-app contextual help panel.
 *
 * Usage:
 *   1. Include this script in your app's HTML: <script src="/inline-guide-widget.js"></script>
 *   2. Define window.appGuidePages as a map of pageKey → { title, desc, actions }
 *      BEFORE this script loads, or in a preceding <script> block.
 *
 * Page key matching:
 *   - First: exact match on `pathname + search` (with trailing slash stripped)
 *   - Then:  exact match on `pathname` only
 *   - Then:  prefix match on `pathname` (longest prefix wins)
 *   - For SPA hash routes, also matches against the hash value
 *
 * Each guide is per-app only — no cross-references between apps.
 */

(function(){
  'use strict';

  if (window.__inlineGuideLoaded) return;
  window.__inlineGuideLoaded = true;

  // ── config defaults ────────────────────────────────────────────
  if (typeof window.appGuidePages === 'undefined') window.appGuidePages = {};
  var LS_KEY = 'inline_guide_closed';

  // ── helpers ────────────────────────────────────────────────────
  function getClosed() {
    try { return JSON.parse(localStorage.getItem(LS_KEY) || '{}'); } catch(e) { return {}; }
  }
  function setClosed(key, val) {
    var o = getClosed();
    o[key] = val;
    try { localStorage.setItem(LS_KEY, JSON.stringify(o)); } catch(e) {}
  }
  function esc(s) {
    if (!s) return '';
    return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
  }

  // ── resolve guide entry ────────────────────────────────────────
  function getCurrentEntry() {
    var pages = window.appGuidePages || {};
    if (!Object.keys(pages).length) return null;

    var path = window.location.pathname.replace(/\/+$/, '') + window.location.search;
    var pathOnly = window.location.pathname.replace(/\/+$/, '');
    var hash = window.location.hash.replace(/^#/, '');

    // 1. exact match on path+query
    if (pages[path]) return pages[path];
    // 2. exact match on path only
    if (pages[pathOnly]) return pages[pathOnly];
    // 3. exact match on hash
    if (hash && pages[hash]) return pages[hash];
    // 4. prefix match on path (longest prefix)
    var candidates = Object.keys(pages).sort(function(a,b){ return b.length - a.length; });
    for (var i = 0; i < candidates.length; i++) {
      var k = candidates[i].replace(/\/+$/, '');
      if (pathOnly === k || pathOnly.startsWith(k + '/') || pathOnly.startsWith(k + '?')) return pages[candidates[i]];
    }
    // 5. prefix match on hash
    if (hash) {
      for (var i = 0; i < candidates.length; i++) {
        if (hash === candidates[i] || hash.startsWith(candidates[i] + '/') || hash.startsWith(candidates[i] + '?')) return pages[candidates[i]];
      }
    }
    return null;
  }

  // ── build widget DOM ───────────────────────────────────────────
  var widgetCreated = false;
  var toggle, panel;

  function createWidget() {
    if (widgetCreated) return;
    widgetCreated = true;

    widget = document.createElement('div');
    widget.id = 'inline-guide-widget';
    widget.innerHTML =
      '<style>' +
      '#inline-guide-widget{position:fixed;bottom:24px;right:24px;z-index:99999;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}' +
      '#ig-toggle{width:48px;height:48px;border-radius:50%;border:none;background:#4f46e5;color:#fff;font-size:22px;font-weight:700;cursor:pointer;box-shadow:0 4px 12px rgba(79,70,229,0.4);transition:transform .15s,box-shadow .15s;display:flex;align-items:center;justify-content:center;line-height:1}' +
      '#ig-toggle:hover{transform:scale(1.08);box-shadow:0 6px 18px rgba(79,70,229,0.5)}' +
      '#ig-panel{position:fixed;bottom:80px;right:24px;width:360px;max-width:calc(100vw - 48px);background:#fff;border-radius:16px;box-shadow:0 8px 32px rgba(0,0,0,0.18);border:1px solid #e5e7eb;overflow:hidden;display:none;max-height:60vh;overflow-y:auto}' +
      '#ig-panel.open{display:block}' +
      '#ig-header{padding:16px 18px 12px;background:linear-gradient(135deg,#4f46e5,#3730a3);color:#fff;display:flex;align-items:center;justify-content:space-between}' +
      '#ig-header h3{margin:0;font-size:15px;font-weight:600}' +
      '#ig-close{background:rgba(255,255,255,0.2);border:none;color:#fff;width:28px;height:28px;border-radius:8px;cursor:pointer;font-size:16px;display:flex;align-items:center;justify-content:center;padding:0;line-height:1}' +
      '#ig-close:hover{background:rgba(255,255,255,0.35)}' +
      '#ig-body{padding:14px 18px 16px}' +
      '#ig-desc{font-size:14px;color:#374151;line-height:1.55;margin:0 0 12px}' +
      '#ig-actions-title{font-size:12px;font-weight:600;color:#6b7280;text-transform:uppercase;letter-spacing:.5px;margin:0 0 6px}' +
      '#ig-actions{list-style:none;padding:0;margin:0}' +
      '#ig-actions li{font-size:13px;color:#374151;padding:5px 0 5px 20px;position:relative}' +
      '#ig-actions li::before{content:"\2192";position:absolute;left:2px;color:#4f46e5;font-weight:700}' +
      '#ig-dismiss{display:block;margin-top:12px;padding:8px 14px;border:1px solid #d1d5db;border-radius:8px;background:#fff;color:#6b7280;font-size:13px;cursor:pointer;text-align:center;width:100%}' +
      '#ig-dismiss:hover{background:#f9fafb}' +
      '</style>' +
      '<button id="ig-toggle" aria-label="Help" title="Guide">?</button>' +
      '<div id="ig-panel">' +
        '<div id="ig-header"><h3 id="ig-title"></h3><button id="ig-close" aria-label="Close guide">\u2715</button></div>' +
        '<div id="ig-body">' +
          '<p id="ig-desc"></p>' +
          '<p id="ig-actions-title" style="display:none">What you can do here</p>' +
          '<ul id="ig-actions"></ul>' +
          '<button id="ig-dismiss">Don\'t show this again</button>' +
        '</div>' +
      '</div>';
    document.body.appendChild(widget);

    toggle = document.getElementById('ig-toggle');
    panel = document.getElementById('ig-panel');
    var closeBtn = document.getElementById('ig-close');
    var dismissBtn = document.getElementById('ig-dismiss');

    toggle.addEventListener('click', function(){ panel.classList.toggle('open'); });
    closeBtn.addEventListener('click', function(){ panel.classList.remove('open'); });
    dismissBtn.addEventListener('click', function(){
      setClosed(getPageKey(), true);
      panel.classList.remove('open');
      toggle.style.display = 'none';
    });
  }

  function getPageKey() {
    var path = window.location.pathname.replace(/\/+$/, '') + window.location.search;
    var hash = window.location.hash.replace(/^#/, '');
    return hash || path;
  }

  function updateGuide() {
    createWidget();
    var entry = getCurrentEntry();
    var key = getPageKey();
    var closed = getClosed()[key];

    if (!entry) {
      toggle.style.display = 'none';
      panel.classList.remove('open');
      return;
    }

    toggle.style.display = 'flex';
    document.getElementById('ig-title').textContent = entry.title || 'Guide';
    document.getElementById('ig-desc').textContent = entry.desc || '';

    var actionsEl = document.getElementById('ig-actions');
    var actionsTitle = document.getElementById('ig-actions-title');
    if (entry.actions && entry.actions.length) {
      actionsTitle.style.display = 'block';
      actionsEl.innerHTML = entry.actions.map(function(a){ return '<li>' + esc(a) + '</li>'; }).join('');
    } else {
      actionsTitle.style.display = 'none';
      actionsEl.innerHTML = '';
    }

    // Show panel if not dismissed
    if (closed === true) {
      panel.classList.remove('open');
    } else {
      panel.classList.add('open');
    }
  }

  // ── init ──────────────────────────────────────────────────────
  // Wait a tick for SPA to bootstrap
  setTimeout(updateGuide, 100);

  // Re-run on hash change (SPA navigation)
  window.addEventListener('hashchange', function(){
    setTimeout(updateGuide, 50);
  });

  // Also observe DOM mutations in case the SPA replaces content without hash change
  var observer = new MutationObserver(function(){
    var entry = getCurrentEntry();
    if (entry) {
      updateGuide();
    }
  });
  if (document.getElementById('app')) {
    observer.observe(document.getElementById('app'), { childList: true, subtree: true });
  }
})();
