/*
 * IncentiveSwift Mail — the mail client (B205 / B206 / B210 / B211).
 *
 * WHY IT LIVES ON ITS OWN HOST
 * David's decision (B211): the private-mail client is its own front door, inbox.<domain>, while the
 * CRM keeps ticketing. Both read the SAME endpoints and the SAME database — this file is a
 * presentation layer, never a second backend.
 *
 * WHAT IT MUST DO, in his words:
 *   setup page (plain English, numbered, copy buttons) -> LOG IN -> choose a mailbox -> inbox;
 *   folders Inbox/Unread/Favorites/Drafts/Sent/Archive/Spam/Trash with three-dot menus that differ
 *   per folder; labels with a colour and a three-dot menu; Compose with a From selector; a reader
 *   with reply/reply-all/forward/star/archive/delete/assign/summarise; an address filter including
 *   MISC; and mail is ACCOUNT-level, never workspace-level.
 *
 * NO OPTION APPEARS UNLESS IT WORKS. Anything the server cannot do (AI summaries with no provider
 * key, an inbox on a domain that is not receiving yet) is not drawn as a live control.
 */
(function () {
  'use strict';

  var TOKEN_KEY = 'is_mail_token';
  // The day/night state this page is PAINTING (set by the boot script in index.html before the
  // stylesheet loads). Read at render time so the rail's switch always states the truth.
  function isDarkTheme() {
    return document.documentElement.getAttribute('data-theme') === 'dark';
  }
  // This app mounts its API under /api/v1 (measured 2026-11-12: /api/auth/login answers 404 while
  // /api/v1/auth/login answers 200 through the same host). The client was ported from CoreSwift,
  // which serves /api, so every call here 404'd and the mailbox could not even be signed into.
  var API_BASE = '/api/v1';
  // The CRM console, where Plans & Billing lives. The storage banner's "See plans" door needs an
  // address that actually opens that screen: `?tab=billing` is honoured by the console's boot.
  var CRM_APP_URL = "https://app.incentiveswift.com";
  var S = {
    token: localStorage.getItem(TOKEN_KEY) || '',
    me: null,
    boxes: [], domains: [], folders: { system: [], custom: [] }, counts: {},
    labels: [], messages: [], addresses: [], misc: 0, members: [],
    folder: 'inbox', address: 'all', label: '', q: '', sel: null, view: 'mail', err: '', busy: false,
    // Free-text search for the list header. The server has always accepted `q` (subject, sender
    // and opening text, case-insensitive); until now no client ever sent it, so a customer
    // with a full inbox had no way to find a message. Kept in S so the box survives the
    // re-render every action triggers, and cleared whenever the view changes.
    focusSearch: false,
    // Which mailbox the user picked on the chooser. Signing in never drops someone into one inbox:
    // the account's mailboxes are listed and the user picks (David 2026-11-11, B205). A LOG IN press
    // on the CRM's Inbox tab arrives as ?address=, which IS the choice, so that skips the list.
    picked: false
  };

  // ── Session handoff from the console (2026-11-20) ────────────────────────────────────────────
  // The CRM console's "Open the mail client" door promises "nothing to sign in to twice": it hands
  // this client the SAME session in the URL FRAGMENT (#token=...), which a browser never sends to a
  // server, so the token cannot land in an nginx access log or a Referer header. Take it into this
  // client's own session store, then strip the fragment so a reload or a bookmark does not carry it.
  // An expired/invalid handoff is harmless: `boot()` answers 401 and the login screen renders.
  (function takeHandoff() {
    var m = /(?:^#|&)token=([^&]+)/.exec(location.hash || '');
    if (!m) return;
    var t = '';
    try { t = decodeURIComponent(m[1]); } catch (e) { t = ''; }
    if (t) { S.token = t; try { localStorage.setItem(TOKEN_KEY, t); } catch (e) {} }
    try { history.replaceState(null, '', location.pathname + location.search); } catch (e) { location.hash = ''; }
  })();

  var $ = function (id) { return document.getElementById(id); };
  function esc(s) {
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }
  function fmt(d) {
    if (!d) return '';
    try {
      var t = new Date(d), now = new Date();
      if (t.toDateString() === now.toDateString()) return t.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
      return t.toLocaleDateString([], { month: 'short', day: 'numeric' });
    } catch (e) { return String(d); }
  }
  function bytes(n) {
    n = Number(n || 0);
    if (n >= 1073741824) return (n / 1073741824).toFixed(1) + ' GB';
    if (n >= 1048576) return (n / 1048576).toFixed(1) + ' MB';
    if (n >= 1024) return (n / 1024).toFixed(0) + ' KB';
    return n + ' B';
  }
  function fail(e) { return (e && (e.message || e.error)) || String(e); }
  function toast(msg, kind) {
    var t = document.createElement('div');
    t.textContent = msg;
    t.style.cssText = 'position:fixed;left:50%;transform:translateX(-50%);bottom:22px;z-index:99;padding:10px 16px;border-radius:10px;'
      + 'font-size:13px;color:#fff;background:' + (kind === 'error' ? '#b91c1c' : '#111827') + ';box-shadow:0 6px 24px rgba(0,0,0,.18)';
    document.body.appendChild(t);
    setTimeout(function () { t.remove(); }, 3600);
  }

  // ── API ────────────────────────────────────────────────────────────────────────────────────────
  async function api(method, path, body) {
    var headers = { 'Content-Type': 'application/json' };
    if (S.token) headers.Authorization = 'Bearer ' + S.token;
    var res = await fetch(API_BASE + path, {
      method: method, headers: headers, body: body ? JSON.stringify(body) : undefined
    });
    var text = await res.text();
    var data = null;
    try { data = text ? JSON.parse(text) : null; } catch (e) { data = { message: text }; }
    if (res.status === 401) {
      signOut(true);
      throw new Error('Your session ended. Sign in again.');
    }
    if (!res.ok) {
      var msg = (data && (data.message || data.error)) || ('Request failed (' + res.status + ')');
      throw new Error(msg);
    }
    return data;
  }

  // ── Auth ──────────────────────────────────────────────────────────────────────────────────────
  function signOut(quiet) {
    S.token = ''; localStorage.removeItem(TOKEN_KEY);
    S.me = null; S.boxes = []; S.domains = []; S.messages = []; S.sel = null; S.view = 'setup';
    S.picked = false; CHOSEN_ADDRESS = '';
    render();
    if (!quiet) toast('Signed out');
  }

  async function doLogin(email, password) {
    var r = await api('POST', '/auth/login', { email: email, password: password });
    S.token = r.token || r.access_token;
    localStorage.setItem(TOKEN_KEY, S.token);
    S.me = r.user || r.team_member || null;
    await boot();
  }

  // ── Boot / data ───────────────────────────────────────────────────────────────────────────────
  async function boot() {
    try {
      var rs = await Promise.all([
        api('GET', '/private-email/boxes').catch(function () { return []; }),
        api('GET', '/private-email/domains').catch(function () { return []; }),
        api('GET', '/private-email/folders').catch(function () { return { system: [], custom: [] }; }),
        api('GET', '/private-email/messages/counts').catch(function () { return {}; }),
        api('GET', '/private-email/labels').catch(function () { return { labels: [] }; }),
        api('GET', '/private-email/members').catch(function () { return { members: [] }; }),
        api('GET', '/private-email/addresses').catch(function () { return {}; })
      ]);
      S.boxes = Array.isArray(rs[0]) ? rs[0] : (rs[0].boxes || []);
      S.domains = Array.isArray(rs[1]) ? rs[1] : (rs[1].domains || []);
      S.folders = rs[2] || { system: [], custom: [] };
      S.counts = rs[3] || {};
      S.labels = (rs[4] && rs[4].labels) || [];
      S.members = (rs[5] && rs[5].members) || [];
      S.addresses = (rs[6] && rs[6].addresses) || [];
      S.misc = (rs[6] && rs[6].misc && rs[6].misc.total) || 0;
      S.err = '';
      // A mailbox chosen from the CRM's LOG IN button arrives as ?address=. With no inboxes at all
      // there is nothing to read, so the user lands on the setup page instead. With inboxes but no
      // choice yet, the user gets the MAILBOX CHOOSER — signing in never picks an inbox for them.
      if (!S.boxes.length) S.view = 'setup';
      else if (CHOSEN_ADDRESS) { S.address = CHOSEN_ADDRESS; S.picked = true; S.view = 'mail'; }
      else if (S.view !== 'mail' || !S.picked) S.view = 'choose';
      if (S.view === 'mail') await loadMessages();
    } catch (e) {
      S.err = fail(e);
      S.view = 'setup';
    }
    render();
  }

  // Every view the rail offers is one GET: the server's eight own views, plus the path of a folder
  // this account made — a made path is a view of its own (the exact path and everything filed below
  // it), so nothing is filtered here. Before IncentiveSwift's message query learned that, this client
  // paged the widest view and kept the rows sitting in the path, which capped a folder at 1,000
  // rows; that workaround went with the server fix (kanban t_d43ee278).
  async function loadMessages() {
    try {
      var q = '/private-email/messages?folder=' + encodeURIComponent(S.folder) + '&address=' + encodeURIComponent(S.address)
        + '&label=' + encodeURIComponent(S.label || '') + '&q=' + encodeURIComponent(S.q || '') + '&limit=100';
      var r = await api('GET', q);
      S.messages = (r && r.messages) || [];
    } catch (e) {
      S.messages = [];
      S.err = fail(e);
    }
  }

  // ── Render ───────────────────────────────────────────────────────────────────────────────────
  function render() {
    var app = $('app');
    if (!S.token) { app.innerHTML = setupPage(); bindSetup(); return; }
    if (S.view === 'setup') { app.innerHTML = setupPage(); bindSetup(); return; }
    if (S.view === 'choose') { app.innerHTML = chooserPage(); bindChooser(); return; }
    app.innerHTML = mailPage();
    bindMail();
  }

  // Setup page: readable signed out (B205), and the place a signed-in account without mailboxes
  // finishes its setup.
  /* Create a mailbox from the screen that tells you to create one.
   *
   * kanban 2026-11-13: until now this page said "Mailboxes are created on your mail screen" and the
   * mail screen could not create one either — the picker's empty card pointed back at the setup page,
   * so an account with a verified domain and no mailbox had NO reachable door. The route has always
   * existed (POST /private-email/boxes). The server refuses a mailbox on a domain that is not
   * receiving mail yet and its sentence is shown here unchanged, because it names the step to take.
   */
  function newBoxFormHtml() {
    var opts = S.domains.map(function (d) {
      return '<option value="' + esc(d.id) + '">' + esc(d.domain)
        + (d.verified ? '' : ' — waiting for its records') + '</option>';
    }).join('');
    var pick = S.domains.length > 1
      ? '<label class="f">Your domain</label><select id="smBoxDomain">' + opts + '</select>'
      : '<input type="hidden" id="smBoxDomain" value="' + esc(S.domains[0].id) + '">'
        + '<div class="rec"><strong>' + esc(S.domains[0].domain) + '</strong><span>'
        + (S.domains[0].verified ? 'Active — receiving mail' : 'Waiting for its records')
        + '</span><span></span></div>';
    return '<div style="margin-top:14px;border-top:1px solid var(--line);padding-top:12px">'
      + pick
      + '<label class="f">The address people will write to</label>'
      + '<div style="display:flex;gap:8px;align-items:center">'
      + '<input id="smBoxLocal" placeholder="support" style="flex:1" autocapitalize="none" spellcheck="false">'
      + '<span style="color:var(--muted);white-space:nowrap">@' + esc(S.domains[0].domain) + '</span>'
      + '</div>'
      + '<div style="margin-top:10px"><button class="btn btn-primary" id="smAddBox">Create this mailbox</button>'
      + ' <span id="smBoxMsg" class="ok"></span></div>'
      + '<p style="color:var(--muted);font-size:12px;margin:8px 0 0">Letters, digits, dot, dash and '
      + 'underscore only. Your 30 free days start with the first mailbox you create.</p>'
      + '</div>';
  }

  function setupPage() {
    var signedIn = !!S.token;
    var domain = S.domains[0] || null;
    var step3 = '';
    if (signedIn && domain) {
      step3 = '<div class="card"><h3>3. Add these records where your domain lives</h3>'
        + '<p style="color:var(--muted);margin-top:0">Your domain stays at your own provider. These exact values prove your mail is really yours. Press <strong>Show my records</strong>.</p>'
        + '<button class="btn" id="smRecords">Show my records</button> <span class="ok" id="smRecMsg"></span>'
        + '<div id="smRecList"></div></div>';
    } else {
      step3 = '<div class="card"><h3>3. Add these records where your domain lives</h3>'
        + '<p style="color:var(--muted);margin-top:0">Your domain stays at your own provider (Cloudflare, your registrar, anywhere). Three groups of record are needed, and every value gets a copy button once you sign in:</p>'
        + '<div class="rec"><strong>MX</strong><span><code>mxa.mailgun.org</code><br><span style="color:var(--muted)">Where the internet delivers your mail.</span></span><span></span></div>'
        + '<div class="rec"><strong>MX</strong><span><code>mxb.mailgun.org</code><br><span style="color:var(--muted)">The second delivery server, used if the first is busy.</span></span><span></span></div>'
        + '<div class="rec"><strong>TXT</strong><span><code>SPF and DKIM values</code><br><span style="color:var(--muted)">SPF proves you are allowed to send; DKIM signs your mail so it is not marked as spam.</span></span><span></span></div>'
        + '<p style="color:var(--muted);font-size:12px;margin:8px 0 0">A branded address of your own (<code>inbox.yourbusiness.com</code>) is not offered yet. For now your mail is read and answered here, at <strong>inbox.incentiveswift.com</strong>.</p>'
        + '<p style="color:var(--muted);font-size:12px;margin:8px 0 0">Receiving through <strong>SendGrid</strong> instead of Mailgun? Then the delivery record is its own (<code>MX &rarr; mx.sendgrid.net</code>), and the exact Inbound Parse address to paste into SendGrid is shown once you sign in — nothing about your sending changes.</p>'
        + '<p style="color:var(--muted);font-size:12px;margin:8px 0 0">Nothing is charged and no mailbox exists until these records verify. Until then this page is the whole product.</p></div>';
    }

    return '<div class="setup">'
      + '<div style="display:flex;justify-content:space-between;align-items:center;gap:12px;flex-wrap:wrap">'
      + '<div><div style="font-weight:700;font-size:18px">IncentiveSwift Mail</div>'
      + '<div style="color:var(--muted);font-size:13px">Your business mail, read and answered here.</div></div>'
      + '<div style="display:flex;gap:6px;align-items:center;flex-wrap:wrap">'
      // The day/night switch sits on the header every visitor sees, signed in or not, so the skin can
      // be changed without hunting a menu (2026-11-22). Same shared cookie as the console.
      + '<button class="btn theme-btn" id="sThemeToggle" type="button" onclick="iswToggleTheme()" aria-pressed="' + (isDarkTheme() ? 'true' : 'false') + '" title="' + (isDarkTheme() ? 'Switch to day mode' : 'Switch to night mode') + '">'
      + '<span data-theme-icon>' + (isDarkTheme() ? '\u2600\uFE0F' : '\uD83C\uDF19') + '</span>'
      + '<span data-theme-label>' + (isDarkTheme() ? 'Day mode' : 'Night mode') + '</span>'
      + '</button>'
      + (signedIn
        ? '<button class="btn" id="smSignOut">Sign out</button>'
          + (S.boxes.length ? '<button class="btn btn-primary" id="smOpen">Open my mail</button>' : '')
        : '<button class="btn btn-primary" id="smLogin">Log In</button>')
      + '</div>'
      + '</div>'
      + (S.err ? '<div class="card err">' + esc(S.err) + '</div>' : '')
      + '<div class="card"><h3>How your mail gets here</h3>'
      + '<p style="margin:0;color:var(--muted)">Six short steps. Nothing here needs a technician, and nothing is charged until you have a mailbox that receives.</p></div>'
      + '<div class="card"><h3>1. Add your domain</h3>'
      + '<p style="color:var(--muted);margin-top:0">The domain you receive mail on — for example <strong>yourbusiness.com</strong>. It stays yours; we never take it over.</p>'
      + (signedIn
        ? (S.domains.length
          ? '<div>' + S.domains.map(function (d) {
            // Which service receives mail for this domain is a real, separate answer — say it, so a
            // customer who chose SendGrid is never told their Mailgun account is the thing to check.
            var recv = d.inbound_provider === 'sendgrid' ? ' · receiving through SendGrid'
              : (d.inbound_provider === 'none' ? ' · send only' : ' · receiving through Mailgun');
            return '<div class="rec"><strong>' + esc(d.domain) + '</strong><span>'
              + (d.verified ? '<span class="ok">Active — receiving mail</span>' : '<span style="color:var(--warn)">Waiting for its records</span>')
              + '<span style="color:var(--muted);font-size:12px">' + esc(recv) + '</span>'
              + '</span><button class="btn btn-ghost" onclick="csmCheckDomain(\'' + esc(d.id) + '\')">Check again</button></div>';
          }).join('') + '</div>'
          : '<label class="f">Your domain</label><input id="smDomain" placeholder="yourbusiness.com">'
            + '<label class="f">How should your mail leave? (sending)</label>'
            + '<select id="smSending" onchange="csmChoicesChanged()">'
            + '<option value="mailgun">Mailgun — my own account</option>'
            + '<option value="smtp">Any SMTP service — Amazon SES, SendGrid, Postmark, Google Workspace</option></select>'
            + '<div id="smSmtpBox" style="display:none">'
            + '<label class="f">Mail server</label><input id="smSmtpHost" placeholder="email-smtp.us-east-1.amazonaws.com">'
            + '<label class="f">Port</label><input id="smSmtpPort" placeholder="587">'
            + '<label class="f">Username</label><input id="smSmtpUser">'
            + '<label class="f">Password</label><input id="smSmtpPass" type="password">'
            + '</div>'
            + '<label class="f">How should your mail arrive? (receiving)</label>'
            + '<select id="smReceiving" onchange="csmChoicesChanged()">'
            + '<option value="mailgun">Mailgun — my own account</option>'
            + '<option value="sendgrid">SendGrid — Inbound Parse</option>'
            + '<option value="none">Nowhere — this domain only sends</option></select>'
            + '<p id="smRecvNote" style="color:var(--muted);font-size:12px;margin:4px 0"></p>'
            + '<div id="smKeyBox">'
            + '<label class="f">Your Mailgun key</label><input id="smKey" placeholder="key-…" type="password"></div>'
            + '<div id="smHookBox">'
            + '<label class="f">Your Mailgun webhook signing key</label><input id="smHook" placeholder="from Mailgun → Settings → Webhooks" type="password"></div>'
            + '<div style="margin-top:10px"><button class="btn btn-primary" id="smAddDomain">Add my domain</button> <span id="smDomMsg" class="ok"></span></div>'
            + '<p style="color:var(--muted);font-size:12px">Two separate choices: you can send through Amazon SES and receive through Mailgun, or the other way round. The Mailgun key is in Settings → API keys; it is stored encrypted and used only for your own mail.</p>')
        : '<p style="color:var(--muted)">Press <strong>Log In</strong> to connect your domain.</p>')
      + '</div>'
      + '<div class="card"><h3>2. Choose how you send and how you receive</h3>'
      + '<p style="margin:0;color:var(--muted)">These are two separate choices. <strong>Sending</strong> can go through Mailgun or through any SMTP service — Amazon SES, SendGrid, Postmark, Google Workspace. <strong>Receiving</strong> comes through Mailgun or SendGrid. Mix them freely: sending through Amazon SES while receiving through Mailgun is a normal setup, and nothing you send has to move for that.</p></div>'
      + step3
      + '<div class="card"><h3>4. Wait until it shows Active, then press Check again</h3>'
      + '<p style="margin:0;color:var(--muted)">Records take a few minutes to spread. When your domain says <strong>Active</strong>, you can create mailboxes. Nothing is charged until then, and no mailbox exists until then.</p></div>'
      + '<div class="card"><h3>5. Create your mailboxes</h3>'
      + '<p style="color:var(--muted);margin-top:0">An address people can write to. Your 30 free days start at the first mailbox you create — not at signup, so the days you spend waiting on DNS cost you nothing:</p>'
      + '<ul style="margin:0 0 10px 18px;color:var(--muted)"><li><strong>info@</strong> — enquiries from the public</li>'
      + '<li><strong>support@</strong> — customers who need help</li>'
      + '<li>a personal one, such as your own name</li></ul>'
      + (signedIn && S.boxes.length
        ? '<div>' + S.boxes.map(function (b) {
          return '<div class="rec"><strong>' + esc(b.email_address) + '</strong><span>' + (b.creates_ticket ? 'also opens a ticket' : 'inbox only') + '</span><span></span></div>';
        }).join('') + '</div>'
        : '<p style="margin:0;color:var(--muted)">Mailboxes are created here, once your domain is Active.</p>')
      + ((signedIn && S.domains.length) ? newBoxFormHtml() : '')
      + '</div>'
      + '<div class="card"><h3>6. Turn on catch-all if you want one inbox to receive every address</h3>'
      + '<p style="margin:0;color:var(--muted)">With catch-all on, mail to any address at your domain that you have not created still arrives — and it is shown under <strong>Everything else</strong> in the address filter so you can see it.</p>'
      + (signedIn && S.domains.length
        ? '<label style="display:flex;gap:8px;align-items:center;margin-top:10px;cursor:pointer"><input type="checkbox" id="smCatchAll" style="width:auto" '
          + (S.domains[0].catch_all_enabled ? 'checked' : '') + '> Catch-all is on for ' + esc(S.domains[0].domain) + '</label>'
        : '')
      + '</div>'
      + '<p style="color:var(--muted);font-size:12px">'
      + (S.counts.storage
          ? ('Storage on this plan: ' + esc(S.counts.storage.quota_human || '') + ' — you are using '
             + esc(S.counts.storage.used_human || '0 B') + ' (' + (S.counts.storage.percent || 0) + '%). ')
          : 'Storage is included with your plan and grows with it. ')
      + 'Incoming mail is always accepted and nothing is deleted to make room. '
      + ((S.counts.retention && S.counts.retention.sentence) ? esc(S.counts.retention.sentence) : '')
      + '</p>'
      + '</div>';
  }

  // Show only the fields the two choices need, and say what each receiving choice means.
  window.csmChoicesChanged = function () {
    var sEl = $('smSending'), rEl = $('smReceiving');
    if (!sEl || !rEl) return;
    var sending = sEl.value, receiving = rEl.value;
    var show = function (id, on) { var el = $(id); if (el) el.style.display = on ? '' : 'none'; };
    show('smSmtpBox', sending === 'smtp');
    show('smKeyBox', sending === 'mailgun' || receiving === 'mailgun');
    show('smHookBox', receiving === 'mailgun');
    var n = $('smRecvNote');
    if (n) {
      n.textContent = receiving === 'sendgrid'
        ? 'SendGrid posts each message to an address we give you after this step — paste that URL into SendGrid → Settings → Inbound Parse. No extra key is needed.'
        : (receiving === 'none'
          ? 'Mail addressed to this domain will not arrive in IncentiveSwift. Sending still works.'
          : 'Mailgun delivers your mail to us, so we need its webhook signing key to prove each message really came from your account.');
    }
  };

  function bindSetup() {
    if (typeof window.csmChoicesChanged === 'function') window.csmChoicesChanged();
    var ab = $('smAddBox');
    if (ab) ab.onclick = async function () {
      var local = (($('smBoxLocal') || {}).value || '').trim().toLowerCase();
      var domId = (($('smBoxDomain') || {}).value || '');
      var msg = $('smBoxMsg');
      if (!local) { if (msg) { msg.className = 'err'; msg.textContent = 'Type the name in front of the @ first.'; } return; }
      ab.disabled = true;
      if (msg) { msg.className = 'ok'; msg.textContent = 'Creating…'; }
      try {
        var r = await api('POST', '/private-email/boxes', { domain_id: domId, local_part: local });
        var addr = (r && (r.email_address || (r.box && r.box.email_address))) || (local + '@' + ((S.domains[0] || {}).domain || ''));
        var boxes = await api('GET', '/private-email/boxes').catch(function () { return null; });
        if (boxes) S.boxes = Array.isArray(boxes) ? boxes : (boxes.boxes || []);
        var st = $('smOpen');
        if (st) st.style.display = '';
        render();
        var again = $('smBoxMsg');
        if (again) { again.className = 'ok'; again.textContent = addr + ' is ready — open it from "Open my mail".'; }
      } catch (e) {
        if (msg) { msg.className = 'err'; msg.textContent = fail(e); }
        ab.disabled = false;
      }
    };
    var lg = $('smLogin');
    if (lg) lg.onclick = loginModal;
    var so = $('smSignOut');
    if (so) so.onclick = function () { signOut(); };
    var op = $('smOpen');
    if (op) op.onclick = function () { S.view = 'choose'; S.picked = false; render(); };
    var ad = $('smAddDomain');
    if (ad) ad.onclick = async function () {
      var d = (($('smDomain') || {}).value || '').trim().toLowerCase();
      var k = ($('smKey') || {}).value || '';
      var hook = ($('smHook') || {}).value || '';
      var sending = ($('smSending') || {}).value || 'mailgun';
      var receiving = ($('smReceiving') || {}).value || 'mailgun';
      var m = $('smDomMsg');
      if (!d) { if (m) { m.className = 'err'; m.textContent = 'Type your domain first.'; } return; }
      if ((sending === 'mailgun' || receiving === 'mailgun') && !k.trim()) {
        if (m) { m.className = 'err'; m.textContent = 'Paste your Mailgun key — it is what lets us send and read this domain.'; } return;
      }
      if (receiving === 'mailgun' && !hook.trim()) {
        if (m) { m.className = 'err'; m.textContent = 'Paste your Mailgun webhook signing key — without it incoming mail is refused.'; } return;
      }
      if (sending === 'smtp' && (!($('smSmtpHost') || {}).value || !($('smSmtpUser') || {}).value || !($('smSmtpPass') || {}).value)) {
        if (m) { m.className = 'err'; m.textContent = 'Fill in your mail server, username and password.'; } return;
      }
      if (m) { m.className = 'ok'; m.textContent = 'Adding…'; }
      var payload = { domain: d, sending_provider: sending, receiving_provider: receiving };
      if (sending === 'mailgun' || receiving === 'mailgun') payload.mailgun_api_key = k.trim();
      if (receiving === 'mailgun') payload.webhook_signing_key = hook.trim();
      if (sending === 'smtp') {
        payload.smtp_host = (($('smSmtpHost') || {}).value || '').trim();
        payload.smtp_port = parseInt((($('smSmtpPort') || {}).value || '587'), 10) || 587;
        payload.smtp_username = (($('smSmtpUser') || {}).value || '').trim();
        payload.smtp_password = ($('smSmtpPass') || {}).value || '';
      }
      try {
        await api('POST', '/private-email/domains', payload);
        await boot();
      } catch (e) { if (m) { m.className = 'err'; m.textContent = fail(e); } }
    };
    var rb = $('smRecords');
    if (rb) rb.onclick = async function () {
      var m = $('smRecMsg'), box = $('smRecList');
      m.textContent = 'Reading from Mailgun…';
      try {
        var r = await api('GET', '/private-email/domains/' + S.domains[0].id + '/records');
        m.textContent = '';
        var status = function (x) {
          if (x.valid === true) return '<span class="ok">Active</span>';
          if (x.attention) return '<span style="color:var(--warn)">Needs attention</span>';
          if (x.valid === false) return '<span style="color:var(--warn)">Waiting</span>';
          return '<span style="color:var(--muted)">Add this</span>';
        };
        box.innerHTML = r.records.length
          ? r.records.map(function (x) {
            return '<div class="rec"><strong>' + esc(x.type) + '</strong><span><code>' + esc(x.name) + '</code><br>'
              + '<code>' + esc(x.value) + '</code>'
              + (x.priority != null ? ' <span style="color:var(--muted)">priority ' + esc(x.priority) + '</span>' : '')
              + '<br><span style="color:var(--muted)">' + esc(x.purpose) + '</span>'
              + (x.note ? '<br><span style="color:var(--warn)">' + esc(x.note) + '</span>' : '')
              + ' ' + status(x)
              + '</span>'
              + '<button class="btn btn-ghost" onclick="csmCopy(this,\'' + esc(x.value) + '\')">Copy</button></div>';
          }).join('')
          : '<p class="err">Mailgun returned no records for this domain.</p>'
            + '<p style="color:var(--muted);font-size:12px;margin:6px 0 0">Add the domain in your own Mailgun account first — that is where these values come from.</p>'
            + '<button class="btn" onclick="csmCheckDomain(\'' + esc(S.domains[0].id) + '\')">Check again</button>';
      } catch (e) { m.textContent = fail(e); }
    };
    var ca = $('smCatchAll');
    if (ca) ca.onchange = async function () {
      var want = this.checked, box = this;
      box.disabled = true;
      try { await api('PATCH', '/private-email/domains/' + S.domains[0].id, { catch_all_enabled: want }); toast('Saved'); }
      catch (e) { box.checked = !want; toast(fail(e), 'error'); }
      box.disabled = false;
    };
  }

  // A copy button that reports what it did — a silent clipboard write reads as a dead button.
  window.csmCopy = function (btn, text) {
    var done = function () { btn.textContent = 'Copied'; setTimeout(function () { btn.textContent = 'Copy'; }, 1600); };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, function () { fallback(); });
    } else { fallback(); }
    function fallback() {
      var t = document.createElement('textarea');
      t.value = text; document.body.appendChild(t); t.select();
      try { document.execCommand('copy'); done(); } catch (e) { toast('Copy failed — select the value and copy it', 'error'); }
      t.remove();
    }
  };

  window.csmCheckDomain = async function (id) {
    try {
      var r = await api('POST', '/private-email/domains/' + id + '/verify', {});
      toast(r.verified ? 'Domain is active — receiving mail' : ('Still ' + (r.mailgun_state || 'waiting')), r.verified ? 'ok' : 'error');
      await boot();
    } catch (e) { toast(fail(e), 'error'); }
  };

  function loginModal() {
    modal('Log in', '<label class="f">Email</label><input id="lmEmail" autocomplete="username">'
      + '<label class="f">Password</label><input id="lmPass" type="password" autocomplete="current-password">'
      + '<div class="err" id="lmErr"></div>',
      '<button class="btn" onclick="csmCloseModal()">Cancel</button><button class="btn btn-primary" id="lmGo">Log In</button>',
      function () {
        $('lmGo').onclick = async function () {
          var btn = this; btn.disabled = true;
          try { await doLogin(($('lmEmail').value || '').trim(), $('lmPass').value || ''); closeModal(); }
          catch (e) { $('lmErr').textContent = fail(e); btn.disabled = false; }
        };
      });
  }

  // ── The mailbox chooser (David 2026-11-11, B205) ─────────────────────────────────────────────
  // Logging in lists the mailboxes this ACCOUNT has. The user picks one; nothing is opened behind
  // their back, and "everything in one list" is offered for an account that wants a combined view.
  function chooserPage() {
    var cards = S.boxes.map(function (b) {
      var a = (S.addresses || []).filter(function (x) { return x.address === b.email_address; })[0] || {};
      var dom = (S.domains || []).filter(function (d) { return d.id === b.domain_id; })[0] || {};
      var branded = dom.branded === 'active';
      return '<button class="card" data-pick="' + esc(b.email_address) + '" style="display:flex;justify-content:space-between;'
        + 'align-items:center;width:100%;text-align:left;margin:0 0 10px;cursor:pointer">'
        + '<span><strong style="font-size:15px">' + esc(b.email_address) + '</strong>'
        + (b.creates_ticket ? ' <span style="font-size:11px;color:var(--accent)">also opens a ticket</span>' : '')
        + '<br><span style="color:var(--muted);font-size:12px">'
        + (a.total || 0) + ' message' + ((a.total || 0) === 1 ? '' : 's') + ' in this mailbox'
        + (branded ? ' · ' + esc('inbox.' + dom.domain) + ' works' : '')
        + '</span></span>'
        + '<span class="btn btn-primary">Open</span></button>';
    }).join('');
    var head = '<div style="display:flex;justify-content:space-between;align-items:center;gap:12px;margin-bottom:14px">'
      + '<div><div style="font-weight:700;font-size:18px">Choose a mailbox</div>'
      + '<div style="color:var(--muted);font-size:13px">Same login, same account. Pick the address you want to read.</div></div>'
      + '<div><button class="btn" id="smSignOut2">Sign out</button></div></div>';
    var empty = '<div class="card"><p style="margin:0;color:var(--muted)">No mailboxes yet on this account. '
      + 'Press <strong>Set up my mail</strong> to add your domain and create one.</p>'
      + '<div style="margin-top:10px"><button class="btn btn-primary" id="smSetup">Set up my mail</button></div></div>';
    return '<div class="setup">' + head
      + (S.boxes.length ? cards
        + '<button class="btn" id="smAll" style="margin-top:4px">See everything in one list</button>'
        : empty)
      + '</div>';
  }

  function bindChooser() {
    var so = $('smSignOut2');
    if (so) so.onclick = function () { signOut(); };
    var st = $('smSetup');
    if (st) st.onclick = function () { S.view = 'setup'; render(); };
    Array.prototype.forEach.call(document.querySelectorAll('[data-pick]'), function (b) {
      b.onclick = async function () {
        S.address = b.getAttribute('data-pick');
        S.picked = true;
        S.view = 'mail';
        await loadMessages();
        render();
      };
    });
    var all = $('smAll');
    if (all) all.onclick = async function () {
      S.address = 'all';
      S.picked = true;
      S.view = 'mail';
      await loadMessages();
      render();
    };
  }

  // ── The mail screen ──────────────────────────────────────────────────────────────────────────
  function railHtml() {
    var sys = S.folders.system || [];
    var custom = S.folders.custom || [];
    var rows = sys.map(function (f) {
      return '<div class="folder' + (S.folder === f.id ? ' active' : '') + '" data-folder="' + esc(f.id) + '">'
        + '<span>' + esc(f.name) + '</span>'
        + '<span style="display:flex;gap:4px;align-items:center">'
        + (f.count ? '<span class="count">' + f.count + '</span>' : '')
        + '<button class="btn-ghost" data-menu="' + esc(f.id) + '" title="Folder actions">&#8942;</button>'
        + '</span></div>';
    }).join('');
    var customRows = custom.map(function (f) {
      return '<div class="folder' + (S.folder === f.path ? ' active' : '') + '" data-folder="' + esc(f.path) + '">'
        + '<span style="padding-left:12px">' + esc(f.name) + '</span>'
        + '<span class="count">' + (f.count || '') + '</span></div>';
    }).join('');
    var labelRows = S.labels.map(function (l) {
      return '<div class="folder' + (S.label === l.id ? ' active' : '') + '" data-label="' + esc(l.id) + '">'
        + '<span><i style="display:inline-block;width:9px;height:9px;border-radius:999px;margin-right:6px;background:' + esc(l.color) + '"></i>' + esc(l.name) + '</span>'
        + '<button class="btn-ghost" data-labelmenu="' + esc(l.id) + '" title="Label actions">&#8942;</button></div>';
    }).join('');

    return '<div class="rail">'
      + '<div class="brand">IncentiveSwift Mail</div>'
      // The day/night switch (2026-11-22). Rendered from the CURRENT data-theme so its label is true
      // on the first paint, and it calls the global in index.html that writes the shared cookie back.
      + '<button class="btn btn-ghost theme-btn" id="mThemeToggle" type="button" onclick="iswToggleTheme()" aria-pressed="' + (isDarkTheme() ? 'true' : 'false') + '" title="' + (isDarkTheme() ? 'Switch to day mode' : 'Switch to night mode') + '">'
      + '<span data-theme-icon>' + (isDarkTheme() ? '\u2600\uFE0F' : '\uD83C\uDF19') + '</span>'
      + '<span data-theme-label>' + (isDarkTheme() ? 'Day mode' : 'Night mode') + '</span>'
      + '</button>'
      + '<button class="btn btn-primary" id="mCompose" style="width:100%">Compose</button>'
      + '<button class="btn" id="mSignature" style="width:100%;margin-top:8px">Signature</button>'
      + '<div style="margin-top:12px">' + rows + '</div>'
      + '<h5>Folders you made</h5>' + (customRows || '<div style="color:var(--muted);font-size:12px;padding:0 9px">None yet — use a folder\'s menu.</div>')
      + '<h5>Labels <button class="btn-ghost" id="mNewLabel" title="Create label">+</button></h5>'
      + (labelRows || '<div style="color:var(--muted);font-size:12px;padding:0 9px">None yet.</div>')
      + storageMeterHtml()
      + '</div>';
  }

  /// The address filter, built from the SERVER's own answer (GET /private-email/addresses): every
  /// address this account actually received mail on, plus ALL and MISC. It used to be built from
  /// the mailbox list, so an address that had mail but no active mailbox row (a deleted inbox)
  /// could not be chosen at all. No figures are shown beside the names: the endpoint's counts are
  /// account-wide (they include Sent and Drafts), so printing them beside a folder view would state
  /// a number the list behind them does not match.
  function addressOptions() {
    var seen = {}, rows = [];
    (S.addresses || []).forEach(function (a) {
      // An address nobody created is the catch-all case: it belongs under MISC, never as an
      // address of its own, or the same mail would appear behind two options.
      if (!a || !a.created || seen[a.address]) return;
      seen[a.address] = 1;
      rows.push(a);
    });
    (S.boxes || []).forEach(function (b) {
      if (!b || seen[b.email_address]) return;
      seen[b.email_address] = 1;
      rows.push({ address: b.email_address, created: true });
    });
    rows.sort(function (a, b) { return String(a.address).localeCompare(String(b.address)); });

    var opts = ['<option value="all"' + (S.address === 'all' ? ' selected' : '') + '>All addresses</option>'];
    rows.forEach(function (a) {
      opts.push('<option value="' + esc(a.address) + '"' + (S.address === a.address ? ' selected' : '') + '>'
        + esc(a.address) + '</option>');
    });
    if (S.misc > 0) {
      opts.push('<option value="misc"' + (S.address === 'misc' ? ' selected' : '') + '>Everything else (MISC)</option>');
    }
    return opts.join('');
  }

  function listHtml() {
    var msgs = S.messages || [];
    var head = '<div class="list-head">'
      + '<select id="mAddress" style="width:auto;flex:1">' + addressOptions() + '</select>'
      + '<button class="btn btn-ghost" id="mRefresh" title="Refresh">Refresh</button>'
      + '<input id="mSearch" type="search" value="' + esc(S.q) + '" placeholder="Search subject, sender or text"'
      + ' aria-label="Search mail" autocomplete="off" style="flex:1 1 100%;box-sizing:border-box;padding:7px 10px;'
      + 'border:1px solid var(--line);border-radius:8px;font:inherit;background:var(--panel)">'
      + (S.q ? '<button class="btn btn-ghost" id="mSearchClear" title="Clear the search">Clear search</button>' : '')
      + '</div>';
    if (!msgs.length) {
      // A search that finds nothing is not an empty folder, and saying so is the difference
      // between "your mail is gone" and "try another word".
      return head + (S.q
        ? '<div class="empty">No mail matches &ldquo;' + esc(S.q) + '&rdquo;.<br><span style="font-size:12px">The search looks at the subject, the sender and the opening text of every message in this view. <button class="btn btn-ghost" id="mSearchClear2">Clear the search</button></span></div>'
        : '<div class="empty">No mail in this folder.<br><span style="font-size:12px">Every message that arrives appears here, including mail to addresses that do not open tickets.</span></div>');
    }
    var rows = msgs.map(function (m) {
      var chips = (m.folder === 'drafts' ? '<span class="chip draft">draft</span>' : '')
        + (m.is_ticket ? '<span class="chip ticket">ticket</span>' : '');
      return '<div class="row' + (m.is_read ? '' : ' unread') + (S.sel && S.sel.id === m.id ? ' sel' : '') + '" data-msg="' + esc(m.id) + '">'
        + '<span data-star="' + esc(m.id) + '" style="cursor:pointer;color:' + (m.is_starred ? '#b45309' : '#c7cbd1') + '">' + (m.is_starred ? '★' : '☆') + '</span>'
        + '<span style="min-width:0"><span class="subj">' + esc(m.subject || '(no subject)') + chips + '</span>'
        + '<span class="from">' + esc(m.from_name || m.from_address) + ' · ' + esc(m.address || '') + '</span></span>'
        + '<span class="when">' + esc(fmt(m.received_at)) + '</span></div>';
    }).join('');
    return head + '<div class="rows">' + rows + '</div>';
  }

  // ─────────────────────────────────────────────────────────────────────────────────────────────
  // STORAGE (B206, David 2026-11-11)
  //
  // The meter and the banner are ONE read: GET /private-email/messages/counts returns
  // `storage.banner` computed from the same `storage::usage` the server refuses a new file with, so
  // this screen cannot promise room the server would refuse, or warn about a number it did not
  // measure. The banner appears at 50 / 75 / 99 / 100 and STAYS until space is freed — it is derived
  // from live usage, not from a dismissible flag.
  // ─────────────────────────────────────────────────────────────────────────────────────────────
  function storageState() {
    var st = S.counts && S.counts.storage;
    return (st && typeof st === 'object') ? st : null;
  }

  function storageBannerHtml() {
    var st = storageState();
    if (!st || !st.banner) return '';
    var b = st.banner;
    var colours = b.level >= 100
      ? ['#fee2e2', '#991b1b']
      : (b.level >= 75 ? ['#fef3c7', '#92400e'] : ['#e0e7ff', '#1e3a8a']);
    return '<div class="storage-banner" style="background:' + colours[0] + ';color:' + colours[1] + '">'
      + '<div style="min-width:0"><strong>' + esc(b.headline) + '</strong>'
      + '<div style="font-size:12.5px;margin-top:3px">' + esc(b.action) + '</div></div>'
      + '<a class="btn btn-ghost" style="color:inherit;white-space:nowrap" href="'
      + esc(CRM_APP_URL + '/?tab=billing') + '" target="_blank" rel="noopener">'
      + (b.level >= 100 ? 'Get more storage' : 'See plans') + '</a></div>';
  }

  /// The permanent meter in the rail: the ACTUAL figures, never just "storage".
  function storageMeterHtml() {
    var st = storageState();
    if (!st) return '';
    var pct = Math.max(0, Math.min(100, Number(st.percent || 0)));
    var colour = pct >= 100 ? '#b91c1c' : (pct >= 75 ? '#b45309' : (pct >= 50 ? '#1d4ed8' : '#1d4ed8'));
    var ret = (S.counts && S.counts.retention && S.counts.retention.sentence) || '';
    var paused = (st.attachments_accepted === false)
      ? '<div style="font-size:11px;color:var(--danger);margin-top:4px">New attachments and files are paused until space is freed. Mail still arrives.</div>'
      : '';
    return '<div class="storage" style="margin-top:auto;padding-top:12px">'
      + '<div style="font-size:11px;color:var(--muted)">' + esc(st.used_human || '') + ' of '
      + esc(st.quota_human || '') + ' used (' + pct + '%)</div>'
      + '<div class="meter"><i style="width:' + pct + '%;background:' + colour + '"></i></div>'
      + paused
      + '<div style="font-size:11px;color:var(--muted);margin-top:4px">Incoming mail is never refused and nothing is deleted to make room.</div>'
      + (ret ? '<div style="font-size:10.5px;color:var(--muted);margin-top:4px">' + esc(ret) + '</div>' : '')
      + '</div>';
  }

  // The files a message carried (kanban t_f919924f). The backend stores each part and answers
  // GET /private-email/messages/:id/attachments/:attachment_id, which needs the bearer token — so a
  // plain <a href> cannot open it; the bytes are fetched and handed to the browser as a blob.
  function attachmentsHtml(m) {
    var files = (m && m.attachments) || [];
    if (!files.length) return '';
    return '<div class="attachments" style="margin:10px 0 4px">'
      + '<div style="font-size:11px;color:var(--muted);margin-bottom:4px">Attachments</div>'
      + files.map(function (f) {
        return '<button class="btn btn-ghost" data-att="' + esc(f.id) + '" data-attname="'
          + esc(f.filename || 'attachment') + '" style="margin:0 6px 6px 0">'
          + '\u{1F4CE} ' + esc(f.filename || 'attachment')
          + ' <span style="color:var(--muted);font-size:11px">(' + humanSize(f.size_bytes) + ')</span>'
          + '</button>';
      }).join('')
      + '</div>';
  }

  function humanSize(n) {
    n = Number(n || 0);
    if (n < 1024) return n + ' B';
    if (n < 1024 * 1024) return (n / 1024).toFixed(1) + ' KB';
    return (n / (1024 * 1024)).toFixed(1) + ' MB';
  }

  async function downloadAttachment(messageId, attachmentId, filename) {
    try {
      var res = await fetch(API_BASE + '/private-email/messages/' + messageId
        + '/attachments/' + attachmentId, {
        headers: S.token ? { Authorization: 'Bearer ' + S.token } : {}
      });
      if (res.status === 401) { signOut(true); throw new Error('Your session ended. Sign in again.'); }
      if (!res.ok) throw new Error('That file could not be opened (' + res.status + ').');
      var blob = await res.blob();
      var url = URL.createObjectURL(blob);
      var a = document.createElement('a');
      a.href = url;
      a.download = filename || 'attachment';
      document.body.appendChild(a);
      a.click();
      a.remove();
      setTimeout(function () { URL.revokeObjectURL(url); }, 4000);
    } catch (e) { toast(fail(e), 'error'); }
  }

  function readerHtml() {
    if (!S.sel) return '<div class="empty">No mail selected</div>';
    var m = S.sel;
    if (m.folder === 'drafts') {
      return '<div><h2>Draft</h2>'
        + '<div class="meta">To ' + esc(m.to_address || '(nobody yet)') + ' · ' + esc(fmt(m.received_at)) + '</div>'
        + '<div class="body">' + esc(m.body_plain || '(empty)') + '</div>'
        + '<div class="actions"><button class="btn btn-primary" data-act="continue">Continue writing</button>'
        + '<button class="btn btn-ghost" data-act="move">Move to…</button>'
        + '<button class="btn btn-ghost" data-act="delete">Discard</button></div></div>';
    }
    var assignOpts = '<option value="">Assign to…</option>' + S.members.map(function (u) {
      return '<option value="' + esc(u.id) + '"' + (m.assigned_user_id === u.id ? ' selected' : '') + '>' + esc(u.name || u.email) + '</option>';
    }).join('');
    var spam = m.folder === 'spam', trash = m.folder === 'trash';
    return '<div>'
      + '<h2>' + esc(m.subject || '(no subject)') + '</h2>'
      + '<div class="meta">From <strong>' + esc(m.from_name || m.from_address) + '</strong>'
      + ' &lt;' + esc(m.from_address) + '&gt; · to ' + esc(m.address || '')
      + ' · ' + esc(new Date(m.received_at).toLocaleString()) + '</div>'
      + '<div class="actions">'
      + '<button class="btn" data-act="reply">Reply</button>'
      + '<button class="btn" data-act="reply_all">Reply all</button>'
      + '<button class="btn" data-act="forward">Forward</button>'
      + '<button class="btn btn-ghost" data-act="star">' + (m.is_starred ? '★ Starred' : '☆ Star') + '</button>'
      + '<button class="btn btn-ghost" data-act="archive">Archive</button>'
      + '<button class="btn btn-ghost" data-act="move">Move to…</button>'
      + '<button class="btn btn-ghost" data-act="delete">' + (spam || trash ? 'Delete forever' : 'Delete') + '</button>'
      + (S.counts.ai_summary ? '<button class="btn btn-ghost" data-act="summarize">Summarise</button>' : '')
      + '<select id="mAssign" style="width:auto">' + assignOpts + '</select>'
      + '</div>'
      + (m.is_ticket
        ? '<div class="badge ticket"><span>In your CRM work queue as a ticket.</span></div>'
        : '<div class="badge open"><span>In the inbox only — nobody is working this yet.</span>'
          + '<button class="btn" data-act="ticket">Turn into a ticket</button></div>')
      + '<div id="mSummary"></div>'
      + '<div style="margin:6px 0 10px"><div style="font-size:11px;color:var(--muted);margin-bottom:4px">Labels</div><div id="mLabelBox"></div></div>'
      + '<div class="body">' + esc(m.body_plain || '(no text)') + '</div>'
      + attachmentsHtml(m)
      + '<div id="mReplyBox"></div>'
      + '</div>';
  }

  function mailPage() {
    return '<div class="mail">' + railHtml()
      + '<div class="main">' + storageBannerHtml()
      + '<div class="list">' + listHtml() + '</div>'
      + '<div class="reader">' + readerHtml() + '</div></div></div>';
  }

  // ── Bindings ────────────────────────────────────────────────────────────────────────────────
  function bindMail() {
    var c = $('mCompose');
    // Called with the CLICK EVENT if passed directly, which made `mode` an Event and silently
    // suppressed the draft button; name the mode explicitly.
    if (c) c.onclick = function () { composeModal('new'); };
    var sg = $('mSignature');
    if (sg) sg.onclick = function () { signatureModal(); };
    var nl = $('mNewLabel');
    if (nl) nl.onclick = function () { labelModal(); };
    var ad = $('mAddress');
    if (ad) ad.onchange = async function () {
      // The open message may not belong to the address now being read, so it is cleared with the
      // filter instead of sitting in the reader as if it matched.
      S.address = this.value; S.sel = null;
      await loadMessages(); render();
    };
    var rf = $('mRefresh');
    if (rf) rf.onclick = async function () { await boot(); };
    // SEARCH (the server's `q`). Type and pause, or press Enter. The list re-renders on every
    // result, so the caret is put back where the user left it: losing focus mid-word is how a
    // search box becomes unusable.
    var si = $('mSearch');
    if (si) {
      var runSearch = function (val) {
        var next = String(val == null ? '' : val).trim();
        if (next === S.q) return;
        S.q = next; S.sel = null; S.focusSearch = true;
        loadMessages().then(render);
      };
      var searchTimer = null;
      si.oninput = function () {
        var v = this.value;
        clearTimeout(searchTimer);
        searchTimer = setTimeout(function () { runSearch(v); }, 350);
      };
      si.onkeydown = function (e) {
        if (e.key === 'Enter') { e.preventDefault(); clearTimeout(searchTimer); runSearch(this.value); }
      };
      if (S.focusSearch) {
        var caret = si.value.length;
        try { si.focus(); si.setSelectionRange(caret, caret); } catch (err) {}
        S.focusSearch = false;
      }
    }
    // BOTH clear buttons get the handler: the header one and the one inside the no-match card.
    // `a || b` bound only whichever existed first, leaving the in-place button dead in exactly
    // the state that needs it (measured on ADASwift 2026-11-13: clicked, no request).
    var clearSearch = function () { S.q = ''; S.focusSearch = true; loadMessages().then(render); };
    var sc = $('mSearchClear');
    if (sc) sc.onclick = clearSearch;
    var sc2 = $('mSearchClear2');
    if (sc2) sc2.onclick = clearSearch;

    Array.prototype.forEach.call(document.querySelectorAll('[data-folder]'), function (el) {
      el.onclick = function (e) {
        if (e.target.hasAttribute('data-menu')) return;
        S.folder = el.getAttribute('data-folder'); S.label = ''; S.q = ''; S.sel = null;
        loadMessages().then(render);
      };
    });
    Array.prototype.forEach.call(document.querySelectorAll('[data-menu]'), function (el) {
      el.onclick = function (e) { e.stopPropagation(); folderMenu(el.getAttribute('data-menu')); };
    });
    Array.prototype.forEach.call(document.querySelectorAll('[data-labelmenu]'), function (el) {
      el.onclick = function (e) { e.stopPropagation(); labelMenu(el.getAttribute('data-labelmenu')); };
    });
    Array.prototype.forEach.call(document.querySelectorAll('[data-label]'), function (el) {
      el.onclick = function (e) {
        if (e.target.hasAttribute('data-labelmenu')) return;
        S.label = el.getAttribute('data-label'); S.q = ''; S.sel = null;
        loadMessages().then(render);
      };
    });
    Array.prototype.forEach.call(document.querySelectorAll('[data-msg]'), function (el) {
      el.onclick = function (e) {
        if (e.target.hasAttribute('data-star')) return;
        openMessage(el.getAttribute('data-msg'));
      };
    });
    Array.prototype.forEach.call(document.querySelectorAll('[data-star]'), function (el) {
      el.onclick = async function (e) {
        e.stopPropagation();
        var id = el.getAttribute('data-star');
        var m = (S.messages.filter(function (x) { return x.id === id; })[0] || {});
        try {
          await api('PATCH', '/private-email/messages/' + id, { is_starred: !m.is_starred });
          // The open message, when it is this one, carries the new flag too — otherwise the
          // reader keeps saying "☆ Star" after the row beside it says starred.
          if (S.sel && S.sel.id === id) S.sel.is_starred = !m.is_starred;
          await boot();
        }
        catch (err) { toast(fail(err), 'error'); }
      };
    });
    var sel = $('mAssign');
    if (sel) sel.onchange = async function () {
      if (!this.value || !S.sel) return;
      try {
        await api('PATCH', '/private-email/messages/' + S.sel.id, { assigned_user_id: this.value });
        toast('Assigned');
        openMessage(S.sel.id);
      } catch (e) { toast(fail(e), 'error'); }
    };
    Array.prototype.forEach.call(document.querySelectorAll('[data-act]'), function (b) {
      b.onclick = function () { messageAction(b.getAttribute('data-act')); };
    });
    Array.prototype.forEach.call(document.querySelectorAll('[data-att]'), function (b) {
      b.onclick = function () {
        if (!S.sel) return;
        downloadAttachment(S.sel.id, b.getAttribute('data-att'), b.getAttribute('data-attname'));
      };
    });
    if (S.sel) applyLabelControl();
  }

  async function openMessage(id) {
    try {
      S.sel = await api('GET', '/private-email/messages/' + id);
      // Reading it marks it read on the server; refresh the counts and the row without moving focus.
      api('GET', '/private-email/messages/counts').then(function (c) { S.counts = c || S.counts; }).catch(function () {});
      render();
    } catch (e) { toast(fail(e), 'error'); }
  }

  async function messageAction(act) {
    var m = S.sel;
    if (!m) return;
    try {
      if (act === 'star') { await api('PATCH', '/private-email/messages/' + m.id, { is_starred: !m.is_starred }); return openMessage(m.id); }
      if (act === 'archive') { await api('PATCH', '/private-email/messages/' + m.id, { folder: 'archive' }); S.sel = null; await boot(); toast('Archived'); return; }
      if (act === 'move') { moveModal(); return; }
      if (act === 'delete') {
        var forever = m.folder === 'spam' || m.folder === 'trash';
        if (!confirm(forever ? 'This message will be destroyed for good. Delete it?' : 'Move this message to Trash? It can be restored from there.')) return;
        await api('DELETE', '/private-email/messages/' + m.id);
        S.sel = null; await boot(); toast(forever ? 'Deleted' : 'Moved to Trash'); return;
      }
      if (act === 'ticket') {
        var t = await api('POST', '/private-email/messages/' + m.id + '/ticket', {});
        toast(t.created ? 'Ticket created' : 'This message already had a ticket');
        return openMessage(m.id);
      }
      if (act === 'summarize') {
        var box = $('mSummary');
        if (box) box.innerHTML = '<div class="badge open">Summarising…</div>';
        try {
          var s = await api('POST', '/private-email/messages/' + m.id + '/summarize', {});
          if (box) box.innerHTML = '<div class="badge" style="background:#eef2ff;color:#1e3a8a;display:block">'
            + '<strong>Summary</strong><div style="margin-top:4px;white-space:pre-wrap">' + esc(s.summary) + '</div></div>';
        } catch (e) { if (box) box.innerHTML = '<div class="badge open">' + esc(fail(e)) + '</div>'; }
        return;
      }
      if (act === 'continue') { composeModal('draft'); return; }
      if (act === 'reply' || act === 'reply_all' || act === 'forward') { composeModal(act); return; }
    } catch (e) { toast(fail(e), 'error'); }
  }

  // ── Compose ─────────────────────────────────────────────────────────────────────────────────
  // THE SIGNATURE IS PART OF COMPOSE (kanban t_b671a0f7). The block under the message is rendered by
  // the SERVER (GET /private-email/signature?mailbox=) with the same renderer the sender uses, so the
  // writer sees exactly what leaves. It starts ticked — the owner's rule is "compose opens with both
  // already in place, the user types subject and body only" — and unticking it drops the signature for
  // this one message. Picking a different From reloads the block, because the signature is per
  // mailbox with a workspace default.
  function composeModal(mode) {
    mode = mode || 'new';
    var m = S.sel;
    var fromOpts = S.boxes.map(function (b) {
      var sel = (m && m.address === b.email_address) || (!m && S.address !== 'all' && S.address === b.email_address);
      return '<option value="' + esc(b.email_address) + '"' + (sel ? ' selected' : '') + '>' + esc(b.email_address) + '</option>';
    }).join('');
    if (!fromOpts) { toast('Create an inbox before sending mail', 'error'); return; }

    var to = '', subject = '', body = '';
    if (mode === 'new') { subject = ''; }
    // Continuing a draft opens the SAME draft's own text. It used to open an empty message under a
    // "Continue writing" button, so pressing it threw the letter away.
    if (mode === 'draft' && m) {
      to = m.to_address || '';
      subject = m.subject || '';
      body = m.body_plain || '';
    }
    if (mode === 'reply' || mode === 'reply_all') {
      to = (m && m.from_address) || '';
      subject = /^re:/i.test((m && m.subject) || '') ? m.subject : 'Re: ' + ((m && m.subject) || '');
      body = '\n\n— ' + ((m && m.from_name) || '') + ' wrote:\n' + ((m && m.body_plain) || '');
    }
    if (mode === 'forward') {
      subject = 'Fwd: ' + ((m && m.subject) || '');
      body = '\n\n— forwarded —\nFrom: ' + ((m && m.from_address) || '') + '\n' + ((m && m.body_plain) || '');
    }

    // What the SERVER says this mailbox's signature is. `include` starts at `has` so a mailbox set to
    // send without one never shows a ticked box that does nothing.
    var sig = { html: '', has: false, include: true, detail: '', loading: true };

    function paintSig() {
      var wrap = $('cSigWrap'), row = $('cSigRow'), on = $('cSigOn');
      if (!wrap) return;
      if (sig.loading) {
        wrap.innerHTML = '<span style="color:var(--muted);font-size:12px">Loading the signature…</span>';
        return;
      }
      if (!sig.has) {
        wrap.innerHTML = '<div style="color:var(--muted);font-size:12px">' + esc(sig.detail || 'No signature is set for this address.') + '</div>';
        if (row) row.style.display = 'none';
        return;
      }
      wrap.innerHTML = sig.html;
      wrap.style.opacity = sig.include ? '1' : '.4';
      if (row) row.style.display = 'flex';
      if (on) on.checked = sig.include;
    }

    async function loadSig() {
      var from = $('cFrom');
      if (!from) return;
      sig.loading = true; paintSig();
      try {
        var d = await api('GET', '/private-email/signature?mailbox=' + encodeURIComponent(from.value));
        sig.html = (d && d.html) || '';
        sig.has = !!(d && d.has_signature);
        sig.detail = (d && d.detail) || '';
        sig.include = sig.has;
      } catch (e) {
        sig.has = false;
        sig.detail = 'Could not load the signature: ' + fail(e);
      }
      sig.loading = false; paintSig();
    }

    modal(mode === 'new' ? 'New message' : (mode === 'draft' ? 'Continue draft' : (mode === 'forward' ? 'Forward' : 'Reply')),
      '<label class="f">From</label><select id="cFrom">' + fromOpts + '</select>'
      + '<label class="f">To</label><input id="cTo" value="' + esc(to) + '" placeholder="someone@example.com">'
      + '<div style="display:grid;grid-template-columns:1fr 1fr;gap:8px">'
      + '<div><label class="f">Cc</label><input id="cCc" placeholder="optional"></div>'
      + '<div><label class="f">Bcc</label><input id="cBcc" placeholder="optional"></div></div>'
      + '<label class="f">Subject</label><input id="cSubject" value="' + esc(subject) + '">'
      + '<label class="f">Message</label><textarea id="cBody" rows="8">' + esc(body) + '</textarea>'
      + '<label class="f">Signature</label>'
      + '<div id="cSigWrap" style="border:1px solid var(--line);border-radius:8px;padding:12px;background:var(--panel);max-height:200px;overflow:auto"></div>'
      + '<label id="cSigRow" style="display:flex;gap:8px;align-items:flex-start;margin:10px 0 0;font-size:13px;line-height:1.35">'
      + '<input type="checkbox" id="cSigOn" checked style="width:auto;margin:2px 0 0">'
      + '<span>Include the signature on this message</span></label>'
      + '<div class="err" id="cErr"></div>',
      '<button class="btn" onclick="csmCloseModal()">Cancel</button>'
      + (mode === 'new' || mode === 'draft' || (m && m.folder === 'drafts') ? '<button class="btn" id="cDraft">Save draft</button>' : '')
      + '<button class="btn btn-primary" id="cSend">Send</button>',
      function () {
        var db = $('cDraft');
        if (db) db.onclick = async function () {
          try {
            await api('POST', '/private-email/messages/drafts', {
              id: (m && m.folder === 'drafts') ? m.id : undefined,
              from_address: $('cFrom').value,
              to: $('cTo').value || '',
              subject: $('cSubject').value || '',
              body: $('cBody').value || ''
            });
            closeModal(); toast('Draft saved'); await boot();
          } catch (e) { $('cErr').textContent = fail(e); }
        };
        var fromSel = $('cFrom');
        if (fromSel) fromSel.onchange = loadSig;
        var on = $('cSigOn');
        if (on) on.onchange = function () { sig.include = on.checked; paintSig(); };
        loadSig();
        $('cSend').onclick = async function () {
          var btn = this;
          var to2 = ($('cTo').value || '').trim();
          if (!to2) { $('cErr').textContent = 'Add who this goes to.'; return; }
          btn.disabled = true;
          try {
            var signed = sig.include && sig.has;
            // A reply, reply-all or forward goes through the REPLY route so it carries the thread and
            // the same one sender; a new message goes through /send. Both append the signature in the
            // same server function, so the toggle means the same thing either way.
            if (m && (mode === 'reply' || mode === 'reply_all' || mode === 'forward')) {
              await api('POST', '/private-email/messages/' + m.id + '/reply', {
                from_address: $('cFrom').value,
                to: to2,
                subject: $('cSubject').value || '',
                body: $('cBody').value || '',
                mode: mode,
                include_signature: signed
              });
            } else {
              await api('POST', '/private-email/send', {
                from_address: $('cFrom').value,
                to: to2,
                cc: splitAddrs($('cCc').value),
                bcc: splitAddrs($('cBcc').value),
                subject: $('cSubject').value || '',
                body: $('cBody').value || '',
                include_signature: signed
              });
            }
            // A draft that has been sent is no longer a draft: the row goes with the send, so the
            // Drafts folder never accumulates unsent-looking copies of mail that already left.
            if (m && m.folder === 'drafts') {
              try { await api('DELETE', '/private-email/messages/' + m.id); } catch (e) {}
            }
            closeModal(); toast('Sent'); S.sel = null; await boot();
          } catch (e) { $('cErr').textContent = fail(e); btn.disabled = false; }
        };
      });
  }

  // ── Signature editor (kanban t_b671a0f7) ─────────────────────────────────────────────────────
  // The LOGO and the workspace default are set in the CRM's Email Branding screen (one place in the
  // workspace). This editor is the per-mailbox half: give one address its own wording, or turn the
  // signature off for it. Everything shown here is read from the server's own answer, and the fields
  // below are the address's own values — a blank field falls back to the workspace default, which the
  // hint says in plain words so nobody wonders why a blank box still prints.
  function signatureModal() {
    if (!S.boxes || !S.boxes.length) { toast('Create an inbox first — a signature belongs to an address', 'error'); return; }
    var opts = S.boxes.map(function (b) {
      return '<option value="' + esc(b.email_address) + '"' + (S.address === b.email_address ? ' selected' : '') + '>' + esc(b.email_address) + '</option>';
    }).join('');

    function field(id, label, value, placeholder) {
      return '<label class="f">' + label + '</label><input id="' + id + '" value="' + esc(value || '') + '" placeholder="' + esc(placeholder || '') + '">';
    }

    modal('Signature',
      '<p style="margin:0 0 12px;font-size:13px;color:var(--muted)">Your company logo and the workspace signature are set in Email Branding in the CRM. Here you choose what one address does.</p>'
      + '<label class="f">Address</label><select id="sMailbox">' + opts + '</select>'
      + '<label class="f">This address sends</label><select id="sMode">'
      + '<option value="inherit">With the workspace signature</option>'
      + '<option value="custom">With its own signature below</option>'
      + '<option value="off">Without a signature</option></select>'
      + '<div id="sFields"></div>'
      + '<div style="margin-top:10px;font-size:12px;color:var(--muted)" id="sHint"></div>'
      + '<div id="sPreview" style="margin-top:10px"></div>'
      + '<div class="err" id="sErr"></div>',
      '<button class="btn" onclick="csmCloseModal()">Close</button>'
      + '<button class="btn btn-primary" id="sSave">Save</button>',
      function () {
        var own = { full_name: '', job_title: '', company: '', phone: '', line: '' };
        function fieldsHtml(v) {
          return '<div style="margin-top:4px">'
            + field('sName', 'Name', v.full_name, 'who the mail is from')
            + field('sTitle', 'Title', v.job_title, 'e.g. Managing Partner')
            + field('sCompany', 'Company', v.company, 'e.g. Giraudy Capital')
            + field('sPhone', 'Phone', v.phone, 'e.g. +1 305 555 0100')
            + field('sLine', 'Line', v.line, 'one short line under the details')
            + '</div>';
        }
        function paint() {
          var mode = $('sMode').value;
          var f = $('sFields');
          if (mode === 'custom') {
            if (!f.innerHTML) f.innerHTML = fieldsHtml(own);
            f.style.display = 'block';
          } else {
            f.style.display = 'none';
          }
        }
        async function load() {
          var err = $('sErr'); if (err) err.textContent = '';
          try {
            var d = await api('GET', '/private-email/signature?mailbox=' + encodeURIComponent($('sMailbox').value));
            own = (d && d.mailbox && d.mailbox.own) || own;
            $('sMode').value = (d && d.mode) || 'inherit';
            $('sFields').innerHTML = fieldsHtml(own);
            var hint = $('sHint');
            hint.textContent = (d && d.mode === 'custom')
              ? 'A blank field uses the workspace value for that field. The workspace signature is set in Email Branding in the CRM.'
              : 'The workspace signature and logo are set in Email Branding in the CRM. This address does not have its own wording.';
            var prev = $('sPreview');
            prev.innerHTML = (d && d.html)
              ? '<div style="font-size:11px;color:var(--muted);margin-bottom:4px">What goes under the message today</div><div style="border:1px solid var(--line);border-radius:8px;padding:12px;background:var(--panel)">' + d.html + '</div>'
              : '<div style="font-size:12px;color:var(--muted)">' + esc((d && d.detail) || 'Nothing is added to messages from this address.') + '</div>';
            paint();
          } catch (e) { $('sErr').textContent = fail(e); }
        }
        $('sMailbox').onchange = load;
        $('sMode').onchange = paint;
        $('sSave').onclick = async function () {
          var btn = this; btn.disabled = true;
          var mode = $('sMode').value;
          try {
            var d = await api('PUT', '/private-email/signature', {
              mailbox: $('sMailbox').value,
              mode: mode,
              full_name: mode === 'custom' ? ($('sName') || {}).value : '',
              job_title: mode === 'custom' ? ($('sTitle') || {}).value : '',
              company: mode === 'custom' ? ($('sCompany') || {}).value : '',
              phone: mode === 'custom' ? ($('sPhone') || {}).value : '',
              line: mode === 'custom' ? ($('sLine') || {}).value : ''
            });
            toast((d && d.detail) || 'Saved');
            await load();
          } catch (e) { $('sErr').textContent = fail(e); }
          btn.disabled = false;
        };
        load();
      });
  }

  function splitAddrs(v) {
    return String(v || '').split(/[,;]/).map(function (s) { return s.trim(); }).filter(Boolean);
  }

  // ── Folder menus (per-folder, exactly as B205 lists them) ────────────────────────────────────
  function folderMenu(folder) {
    var items = [['subfolder', 'Create subfolder'], ['read', 'Mark all as read'], ['unread', 'Mark all as unread']];
    if (folder === 'archive') {
      items.push(['to_inbox', 'Move all to Inbox']);
      items.push(['trash_all', 'Delete all messages']);
    }
    if (folder === 'spam' || folder === 'trash') items.push(['empty', 'Empty folder']);
    items.push(['sort', 'Sort alphabetically']);

    var html = '<div>' + items.map(function (it) {
      return '<button class="btn" style="display:block;width:100%;text-align:left;margin:6px 0" data-fact="' + it[0] + '">' + it[1] + '</button>';
    }).join('') + '<div class="err" id="fmErr"></div></div>';

    modal('Folder: ' + folder, html, '<button class="btn" onclick="csmCloseModal()">Close</button>', function () {
      Array.prototype.forEach.call(document.querySelectorAll('[data-fact]'), function (b) {
        b.onclick = function () { folderAction(folder, b.getAttribute('data-fact')); };
      });
    });
  }

  async function folderAction(folder, act) {
    var err = $('fmErr');
    try {
      if (act === 'subfolder') {
        var name = prompt('Name for the subfolder inside "' + folder + '"?');
        if (!name || !name.trim()) return;
        // Lower case, because the server stores every message's folder path lower-cased: a folder
        // made as "Clients" would never match (or count) the mail filed into it.
        await api('POST', '/private-email/folders', { parent: folder, name: name.trim().toLowerCase() });
        closeModal(); toast('Folder created'); await boot(); return;
      }
      if (act === 'sort') {
        S.messages.sort(function (a, b) { return String(a.subject || '').localeCompare(String(b.subject || '')); });
        closeModal(); render(); return;
      }
      if (act === 'empty') {
        var n = (S.folders.system.filter(function (f) { return f.id === folder; })[0] || {}).count || 0;
        if (!n) { closeModal(); toast('That folder is already empty'); return; }
        if (!confirm('Destroy all ' + n + ' messages in ' + folder + '? This cannot be undone.')) return;
      }
      var r = await api('POST', '/private-email/folders/action', { folder: folder, action: act });
      closeModal();
      toast(act === 'empty' ? ('Destroyed ' + (r.destroyed || 0)) : 'Done');
      await boot();
    } catch (e) { if (err) err.textContent = fail(e); else toast(fail(e), 'error'); }
  }

  // ── Move a message into a folder ────────────────────────────────────────────────────────────
  // The server accepts any system folder or any subfolder path (PATCH /messages/:id {folder}), and
  // the rail lists the account's own subfolders — but nothing in this client could put a message
  // INTO one, so "Folders you made" could only ever be filled by mail arriving on a filter.
  var SYSTEM_ORDER = [['inbox', 'Inbox'], ['archive', 'Archive'], ['spam', 'Spam'], ['trash', 'Trash'], ['drafts', 'Drafts']];

  function moveModal() {
    var m = S.sel;
    if (!m) return;
    var custom = (S.folders.custom || []).filter(function (f) { return f.path !== m.folder; });
    var sys = SYSTEM_ORDER.filter(function (f) { return f[0] !== m.folder; });
    var html = '<p style="margin:0 0 8px;color:var(--muted)">Where should this message go?</p>'
      + '<div style="display:flex;flex-wrap:wrap;gap:6px">'
      + sys.map(function (f) { return '<button class="btn" data-mto="' + f[0] + '">' + f[1] + '</button>'; }).join('')
      + '</div>'
      + (custom.length
        ? '<div style="margin-top:12px"><div style="font-size:11px;color:var(--muted);margin-bottom:4px">Folders you made</div>'
          + '<div style="display:flex;flex-wrap:wrap;gap:6px">'
          + custom.map(function (f) {
              return '<button class="btn" data-mto="' + esc(f.path) + '">' + esc(f.name || f.path) + '</button>';
            }).join('')
          + '</div></div>'
        : '<div style="margin-top:12px;font-size:12px;color:var(--muted)">You have no folders of your own yet — make one below and the message goes straight in.</div>')
      + '<label class="f" style="margin-top:14px">New folder name</label>'
      + '<input id="mvNewName" placeholder="clients">'
      + '<div style="font-size:11px;color:var(--muted);margin-top:4px">Stored in lower case, so the folder and the mail filed in it always agree.</div>'
      + '<div class="err" id="mvErr"></div>';

    modal('Move to folder', html,
      '<button class="btn" onclick="csmCloseModal()">Cancel</button><button class="btn btn-primary" id="mvNewGo">Create and move</button>',
      function () {
        Array.prototype.forEach.call(document.querySelectorAll('[data-mto]'), function (b) {
          b.onclick = function () { moveTo(b.getAttribute('data-mto')); };
        });
        $('mvNewGo').onclick = async function () {
          var name = ($('mvNewName').value || '').trim().toLowerCase();
          if (!name) { $('mvErr').textContent = 'Type a name for the new folder first.'; return; }
          if (name.indexOf('/') !== -1) { $('mvErr').textContent = 'A folder name cannot contain a slash.'; return; }
          try {
            var f = await api('POST', '/private-email/folders', { parent: 'archive', name: name });
            await moveTo(f.path, true);
          } catch (e) { $('mvErr').textContent = fail(e); }
        };
      });
  }

  async function moveTo(path, made) {
    var m = S.sel;
    if (!m) { closeModal(); return; }
    try {
      await api('PATCH', '/private-email/messages/' + m.id, { folder: path });
      closeModal();
      S.sel = null;
      await boot();
      toast(made ? ('Folder made and message moved to ' + path) : ('Moved to ' + path));
    } catch (e) {
      var err = $('mvErr');
      if (err) err.textContent = fail(e); else toast(fail(e), 'error');
    }
  }

  // ── Labels ──────────────────────────────────────────────────────────────────────────────────
  var COLORS = ['#ef4444', '#f59e0b', '#10b981', '#3b82f6', '#8b5cf6', '#ec4899', '#64748b'];

  function labelModal(label) {
    var chosen = (label && label.color) || COLORS[3];
    modal(label ? 'Edit label' : 'Create label',
      '<label class="f">Label name</label><input id="lbName" value="' + esc(label ? label.name : '') + '" placeholder="Label name">'
      + '<label class="f">Colour</label><div class="swatches" id="lbSw">'
      + COLORS.map(function (c) { return '<span class="sw' + (c === chosen ? ' on' : '') + '" data-color="' + c + '" style="background:' + c + '"></span>'; }).join('')
      + '</div><div class="err" id="lbErr"></div>',
      '<button class="btn" onclick="csmCloseModal()">Cancel</button><button class="btn btn-primary" id="lbSave">' + (label ? 'Save' : 'Create') + '</button>',
      function () {
        var pick = chosen;
        Array.prototype.forEach.call(document.querySelectorAll('[data-color]'), function (s) {
          s.onclick = function () {
            pick = s.getAttribute('data-color');
            Array.prototype.forEach.call(document.querySelectorAll('[data-color]'), function (x) { x.classList.remove('on'); });
            s.classList.add('on');
          };
        });
        $('lbSave').onclick = async function () {
          var name = ($('lbName').value || '').trim();
          if (!name) { $('lbErr').textContent = 'Give the label a name.'; return; }
          try {
            if (label) await api('PATCH', '/private-email/labels/' + label.id, { name: name, color: pick });
            else await api('POST', '/private-email/labels', { name: name, color: pick });
            closeModal(); toast('Saved'); await boot();
            if (S.sel) applyLabelControl();
          } catch (e) { $('lbErr').textContent = fail(e); }
        };
      });
  }

  function labelMenu(id) {
    var l = S.labels.filter(function (x) { return x.id === id; })[0] || {};
    modal('Label: ' + (l.name || ''), '<p style="color:var(--muted);margin-top:0">A label is attached to individual messages, so this only changes the label itself.</p>',
      '<button class="btn btn-danger" id="lbDel">Delete</button><button class="btn" onclick="csmCloseModal()">Close</button><button class="btn btn-primary" id="lbEdit">Edit</button>',
      function () {
        $('lbEdit').onclick = function () { closeModal(); labelModal(l); };
        $('lbDel').onclick = async function () {
          if (!confirm('Delete the label "' + (l.name || '') + '"? Messages are not touched.')) return;
          try { await api('DELETE', '/private-email/labels/' + id); closeModal(); toast('Label deleted'); await boot(); }
          catch (e) { toast(fail(e), 'error'); }
        };
      });
  }

  // Applying a label to the OPEN message: choose one, it attaches; it can be removed the same way.
  function applyLabelControl() {
    var box = $('mLabelBox');
    if (!box || !S.sel) return;
    var attached = (S.sel.labels || []).map(function (l) { return l.id; });
    box.innerHTML = S.labels.length
      ? S.labels.map(function (l) {
        var on = attached.indexOf(l.id) >= 0;
        return '<button class="btn" data-toggle="' + esc(l.id) + '" style="margin:2px 4px 2px 0">'
          + '<i style="display:inline-block;width:9px;height:9px;border-radius:999px;margin-right:6px;background:' + esc(l.color) + '"></i>'
          + esc(l.name) + (on ? ' ✓' : '') + '</button>';
      }).join('')
      : '<span style="color:var(--muted);font-size:12px">No labels yet — create one in the sidebar.</span>';
    Array.prototype.forEach.call(box.querySelectorAll('[data-toggle]'), function (b) {
      b.onclick = async function () {
        var id = b.getAttribute('data-toggle');
        var on = attached.indexOf(id) >= 0;
        try {
          if (on) await api('DELETE', '/private-email/labels/' + id + '/apply', { message_ids: [S.sel.id] });
          else await api('POST', '/private-email/labels/' + id + '/apply', { message_ids: [S.sel.id] });
          await openMessage(S.sel.id);
        } catch (e) { toast(fail(e), 'error'); }
      };
    });
  }

  // ── Modal ───────────────────────────────────────────────────────────────────────────────────
  function modal(title, body, footer, after) {
    closeModal();
    var bg = document.createElement('div');
    bg.className = 'modal-bg';
    bg.innerHTML = '<div class="modal"><h3>' + esc(title) + '</h3>' + body + '<div class="modal-foot">' + (footer || '') + '</div></div>';
    document.body.appendChild(bg);
    bg.addEventListener('click', function (e) { if (e.target === bg) closeModal(); });
    if (after) after();
  }
  function closeModal() {
    Array.prototype.forEach.call(document.querySelectorAll('.modal-bg'), function (n) { n.remove(); });
  }
  window.csmCloseModal = closeModal;

  // ── Start ───────────────────────────────────────────────────────────────────────────────────
  // A mailbox chosen on the CRM's Inbox tab arrives as ?address=, so the user lands where they
  // pressed instead of on a chooser they already answered (B211).
  var params = new URLSearchParams(location.search);
  var CHOSEN_ADDRESS = params.get('address') || '';
  if (CHOSEN_ADDRESS) S.address = CHOSEN_ADDRESS;

  if (S.token) { boot(); } else { render(); }
})();
