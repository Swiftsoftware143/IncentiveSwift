use serde_json::json;

use crate::branding::{bind_branding, effective_branding, with_branding_header};

/// The product's own identity — what the `{{app_name}}` merge field carries.
pub(crate) const APP_NAME: &str = "IncentiveSwift";
/// Public app origin — what the `{{login_url}}` merge field carries, and the base of the
/// public campaign link the lifecycle sender binds as `{{share_link}}`/`{{referral_link}}`.
pub(crate) const APP_URL: &str = "https://app.incentiveswift.com";

/// The mail client's own front door (the private-inbox host, ported from CoreSwift).
/// Every message a customer sends about an inbox, and every link a notification points at,
/// resolves to this ONE host so a second copy of the address can never appear in a mail.
pub(crate) const APP_URL_MAIL: &str = "https://inbox.incentiveswift.com";

// The app's own wordmark and the identity every message falls back to when its account has
// configured none live in `crate::branding` (2026-10-24, kanban t_bf8a2c58): the QUEUED/lifecycle
// funnel paints the same header through the same helpers, so the two cannot drift.

/// Render a template string by replacing {{key}} placeholders with values from `vars`.
///
/// The vocabulary is double braces ONLY, which is what both sides of the admin surface
/// advertise (`GET /api/v1/email-templates/merge-fields` and the served console's merge-field
/// buttons). `{{#if key}}…{{/if}}` sections are resolved first, so a row can carry an optional
/// clause (kanban t_cdc24339). Anything left over is logged by name — kanban t_e43521d2: the
/// shipped `welcome` row spoke single braces, so `Welcome to {app_name}!` was mailed out verbatim
/// and nothing said so.
fn render_template(template: &str, vars: &serde_json::Value) -> String {
    let mut result = crate::template_render::resolve_sections(template, vars);
    if let Some(obj) = vars.as_object() {
        for (key, value) in obj {
            let placeholder = format!("{{{{{}}}}}", key);
            let replacement = value.as_str().unwrap_or("");
            result = result.replace(&placeholder, replacement);
        }
    }
    crate::template_render::warn_unsubstituted(&result, "email template");
    result
}

/// Bind the app-wide merge fields the template surface advertises to EVERY type.
///
/// The console's merge-field list offers `{{app_name}}`, `{{login_url}}` and `{{app_url}}` for every
/// template, but until 2026-10-13 only the SUBJECT default and the inline fallback ever saw them —
/// a DB-row template that used them mailed the braces verbatim (measured live: the `password_reset`
/// row rendered "Open {{login_url}} and click …" to a customer). Bound at the single funnel every
/// transactional type passes through (kanban t_2349e6ce), and now also by the QUEUED/lifecycle
/// funnel through the same function (kanban t_bf8a2c58) — a queued template that prints them must
/// never mail braces either. A caller that already set a key keeps its own value.
pub(crate) fn bind_app_fields(vars: &mut serde_json::Value) {
    if !vars.is_object() {
        *vars = json!({});
    }
    if let Some(obj) = vars.as_object_mut() {
        obj.entry("app_name".to_string())
            .or_insert_with(|| json!(APP_NAME));
        obj.entry("login_url".to_string())
            .or_insert_with(|| json!(APP_URL));
        obj.entry("app_url".to_string())
            .or_insert_with(|| json!(APP_URL));
    }
}

/// Send a templated email using database-stored templates.
/// Falls back to old inline methods when no template found.
///
/// `account_id` is the account the mail is being sent FOR (`None` = no account in hand, which
/// degrades to the fleet-wide default). It is REQUIRED for tenant scope: this function used to
/// take no account at all and pick its row with `(aid IS NULL OR is_default = true)
/// ORDER BY is_default ASC, created_at DESC`, so one tenant's template became every tenant's
/// template — kanban t_0fb81177. The selection is now `delivery::sender::load_template_by_type`,
/// the app's single answer to "which template for this account" (see that function for the
/// measured shapes).
pub async fn send_template_email(
    pool: &sqlx::PgPool,
    account_id: Option<uuid::Uuid>,
    to: &str,
    template_type: &str,
    vars: &serde_json::Value,
) -> Result<(), String> {
    let rendered = render_email(pool, account_id, to, template_type, vars).await?;
    send_email_request(pool, to, &rendered.subject, &rendered.text, &rendered.html).await
}

/// The rendered parts of a transactional message — the exact bytes the mail path emits, before
/// they are handed to the queue.
pub struct RenderedEmail {
    pub subject: String,
    pub text: String,
    pub html: String,
}

/// Select the account's template, bind the merge fields and paint the branded header — WITHOUT
/// sending. Split out of [`send_template_email`] (cron 2026-10-29) so the console can show the
/// exact mail an account's branding produces, byte for byte, instead of a drawing of it.
pub async fn render_email(
    pool: &sqlx::PgPool,
    account_id: Option<uuid::Uuid>,
    to: &str,
    template_type: &str,
    vars: &serde_json::Value,
) -> Result<RenderedEmail, String> {
    let app_name = APP_NAME;
    let app_url = APP_URL;

    // Per-account email branding (kanban t_feab8aff). Loaded ONCE here, at the single funnel every
    // transactional type passes through, so a template added later inherits it for free. The merge
    // fields are bound into the render map (per-account, so they cannot be static defaults) and the
    // header block is applied to the rendered parts below.
    //
    // Two values, deliberately (2026-10-24): `stored_branding` is what the ACCOUNT configured, and
    // it is what the merge fields and `logo_is_own` follow; `branding` is what the HEADER is
    // painted with, and an account that configured nothing gets this app's own name and wordmark
    // instead of no header at all. Before this, the credentials mail a brand-new customer
    // receives — the first thing this product ever shows them — arrived with no wordmark, and the
    // Email branding card's own preview described a header that was not there.
    let stored_branding = match account_id {
        Some(id) => crate::branding::load(pool, id).await,
        None => None,
    };
    let branding = effective_branding(stored_branding.clone());
    let mut vars = vars.clone();
    // The app-wide merge fields (`{{app_name}}`, `{{login_url}}`, `{{app_url}}`) are bound by the
    // ONE helper the queued funnel calls too (kanban t_bf8a2c58). A caller that already set a key
    // keeps its own value.
    bind_app_fields(&mut vars);
    bind_branding(&mut vars, stored_branding.as_ref());
    let vars = &vars;

    let template = crate::delivery::sender::load_template_by_type(pool, account_id, template_type)
        .await
        .map_err(|e| format!("DB error loading template for '{template_type}': {e}"))?;

    let (subject, text_body, html_body) = match template {
        Some(t) => {
            let subject = render_template(
                &t.subject
                    .unwrap_or_else(|| get_default_subject(template_type, app_name)),
                vars,
            );
            let html_body = t
                .html_body
                .as_ref()
                .map(|h| render_template(h, vars))
                .unwrap_or_default();
            let text_body = render_template(&t.body.unwrap_or_default(), vars);
            let use_html = t.html_body.is_some();
            (
                subject,
                text_body,
                if use_html { html_body } else { String::new() },
            )
        }
        None => send_inline(to, template_type, vars, app_name, app_url).await?,
    };

    // The account's branding — or, when they have configured none, this app's own identity — opens
    // both parts of the message.
    let (text_body, html_body) = with_branding_header(Some(&branding), text_body, html_body);
    Ok(RenderedEmail {
        subject,
        text: text_body,
        html: html_body,
    })
}

/// What the console's "Show the exact email" arm answers.
pub struct BrandingPreview {
    pub subject: String,
    pub text: String,
    pub html: String,
    pub branded: bool,
    pub own_logo: bool,
}

/// The email this account's branding produces, RENDERED AND RETURNED, never sent (cron
/// 2026-10-29, parity with FunnelSwift and MissedCall Respondr).
///
/// The card's "What your recipients see" block is drawn by the browser, so it can drift from the
/// renderer. This runs the real path — the same template selection, the same merge fields, the same
/// [`with_branding_header`] — and hands back the bytes, so an operator judges the REAL header
/// (uploaded logo, display name, colour rule) without waiting for an inbox and without a send.
/// No address is taken from the caller's request: both recipient fields describe the CALLER.
pub async fn render_branding_preview(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    name: &str,
    to: &str,
) -> Result<BrandingPreview, String> {
    let vars = json!({ "name": name, "email": to });
    let rendered = render_email(pool, Some(account_id), to, "branding_preview", &vars).await?;
    // Read AFTER the render so the flags describe the same branding rows the render used.
    let stored = crate::branding::load(pool, account_id).await;
    let own_logo = stored
        .as_ref()
        .and_then(|b| b.resolve_logo_url(APP_URL))
        .is_some();
    Ok(BrandingPreview {
        subject: rendered.subject,
        text: rendered.text,
        html: rendered.html,
        branded: stored.is_some(),
        own_logo,
    })
}

fn get_default_subject(template_type: &str, app_name: &str) -> String {
    match template_type {
        // `welcome_credentials` is the checkout/onboarding flavour of the same mail (see
        // `send_welcome_email`), so it shares the subject.
        "welcome" | "welcome_credentials" => format!("Welcome to {}!", app_name),
        "purchase_confirmed" => "Payment Received — Thank You!".to_string(),
        "password_reset" => "Password Reset Request".to_string(),
        // The sign-in address change (kanban t_6b2702e3). Named for what the recipient must DO,
        // because the account has not moved yet — the link is the only thing that moves it.
        "email_change" => "Confirm your new sign-in email".to_string(),
        // The customer's own plan request / billing ticket was answered (2026-10-19). Named for
        // the thing the customer asked about, so it reads as an answer and not as a system notice.
        "plan_request_update" => format!("About your request — {app_name}"),
        // The customer's workspace was MOVED onto a plan by a person, not by a card payment
        // (2026-11-12). This is the only upgrade path that works while an install has no payment
        // provider, and until now it was silent: the tier changed and nobody told the customer.
        "plan_changed" => format!("Your plan has changed — {app_name}"),
        // The answer to a message filed from /contact.html by someone with NO account (2026-11-07).
        // Named as an answer, not a system notice, because the recipient asked a question and this
        // is the reply. Kept separate from `plan_request_update` above: that arm tells the reader to
        // open their dashboard, which this reader does not have.
        "public_support_reply" => format!("About your message — {app_name}"),
        // The moment the customer FILES the request (cron 2026-10-20). Named so it is obvious in an
        // inbox that it is the acknowledgement of the thing they just did, not the answer to it.
        "support_request_receipt" => format!("We have your request — {app_name}"),
        // The OPERATOR's copy of a plan request (2026-11-09). Named for the decision it asks for,
        // because the recipient is the person who can switch the workspace, not the customer.
        "plan_request_operator_alert" => format!("A customer asked to buy a plan — {app_name}"),
        // The OPERATOR's copy of an ORDINARY help request (2026-11-09). Measured live: the
        // contact-page door answered 201 "Your message is with our team" and queued ZERO mail, and
        // the signed-in door only ever alerted for a PLAN request — so a locked-out customer wrote
        // for help and the person who answers it was told nothing. Named for the action it needs.
        "support_request_operator_alert" => {
            format!("A support request needs an answer — {app_name}")
        }
        // The OPERATOR's copy of a NEW SIGNUP (2026-11-09, parity with FunnelSwift and
        // WorkflowSwift). Named for the event, because the recipient is the person who runs the
        // install and a new workspace is the one arrival they most want to notice.
        "signup_operator_alert" => format!("New workspace signed up — {app_name}"),
        // The customer's own "show me my branding in a real inbox" sample (cron 2026-10-23).
        // Named as a preview so it can never be mistaken for a system notice.
        "branding_preview" => format!("Your email branding — a preview from {app_name}"),
        // The 50/75/99 storage reminder (kanban t_82551415). The INLINE arm states the real figures
        // in this subject; this default is only reached when a stored row exists with no subject of
        // its own, so it says the one thing that is always true.
        "storage_warning" => format!("Your mailbox is getting full — {app_name}"),
        _ => format!("{} Notification", app_name),
    }
}

/// HTML-escape a value that a CUSTOMER typed before it goes into an operator's mail body.
/// The operator's copy of a help request carries the asker's own subject and message, so without
/// this a visitor could put markup (or a fake link) into the operator's inbox.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn send_inline(
    to: &str,
    template_type: &str,
    vars: &serde_json::Value,
    app_name: &str,
    app_url: &str,
) -> Result<(String, String, String), String> {
    let name = vars.get("name").and_then(|v| v.as_str()).unwrap_or("there");
    let email = vars.get("email").and_then(|v| v.as_str()).unwrap_or("");
    let password = vars.get("password").and_then(|v| v.as_str()).unwrap_or("");
    let token = vars.get("token").and_then(|v| v.as_str()).unwrap_or("");
    let plan_name_val = vars
        .get("plan_name")
        .and_then(|v| v.as_str())
        .unwrap_or("a plan");

    match template_type {
        // The inline safety net. `welcome_credentials` shares the body because it is the same
        // mail for the flow that MINTS the password; it is reached only when the DB row for
        // that type is absent (see `send_welcome_email`).
        "welcome" | "welcome_credentials" => {
            let body = format!(
                "Welcome to {}, {}!\n\nYour account has been created successfully.\n\nHere are your login credentials:\n\nEmail: {}\nPassword: {}\n\nLogin at: {}/login\n\nYou can now:\n- Create loyalty programs\n- Manage customer rewards\n- Track engagement metrics\n\nFor help, send us a message at https://app.incentiveswift.com/contact.html (no sign-in needed)\n\nBest regards,\nThe {} Team",
                app_name, name, email, password, app_url, app_name
            );
            Ok((format!("Welcome to {}!", app_name), body, String::new()))
        }
        "purchase_confirmed" => {
            let body = format!(
                "Hi {},\n\nThank you for your purchase! Your payment for the {} plan has been received successfully.\n\nYou can access your dashboard at: {}/dashboard\n\nIf you have any questions, send us a message at https://app.incentiveswift.com/contact.html\n\nBest regards,\nThe {} Team",
                name, plan_name_val, app_url, app_name
            );
            Ok((
                "Payment Received - Thank You!".to_string(),
                body,
                String::new(),
            ))
        }
        "password_reset" => {
            // The customer must be TOLD where to enter the code, and the mail must carry THIS app's
            // name. It used to hand over a 36-character code with no destination AND sign itself
            // "SwiftSoftware" — a company the customer has never heard of, on a mail that otherwise
            // belongs to IncentiveSwift (fixed 2026-10-13, cron `default`).
            // CORRECTED 2026-10-16 (cron `default`): the destination it was given was `/login`, and
            // that page has NO code field — its "Forgot password?" link goes to /forgot-password.html,
            // which asks for the ADDRESS and mails ANOTHER code. The page that accepts the code is
            // /reset-password (input id="t", prefilled from `?token=`). The 1-hour validity stated
            // here is correct for THIS app (auth_handler::forgot_password mints Duration::hours(1)),
            // so it is left alone.
            let body = format!(
                "Your password reset code is: {}\n\nOpen {}/reset-password?token={} to choose a new password - the code is filled in for you. If the form does not open, go to {}/reset-password and paste the code above.\n\nThis code expires in 1 hour.\n\nIf you did not request this password reset, please ignore this email.\n\n- The {} Team",
                token, app_url, token, app_url, app_name
            );
            Ok(("Password Reset Request".to_string(), body, String::new()))
        }
        // The inline safety net for the sign-in address change (kanban t_6b2702e3) — reached when
        // the account has no `email_change` template row. Two things the customer must be told, and
        // both were learned from the reset mail's own history: WHERE to click (a link, not a code),
        // and that NOTHING HAS CHANGED YET (the old address stays valid until the link is opened).
        "email_change" => {
            let new_email = vars
                .get("new_email")
                .and_then(|v| v.as_str())
                .unwrap_or("(the new address)");
            let link = vars.get("link").and_then(|v| v.as_str()).unwrap_or(app_url);
            let body = format!(
                "Hi {},\n\nWe received a request to change the email address you sign in to {} with, to:\n\n    {}\n\nTo confirm, open this link:\n\n    {}\n\nThe link works once and expires in 2 hours. Until you open it, nothing changes: your current address still works and your password stays the same. If you did not ask for this, ignore this email and nothing will happen.\n\n- The {} Team",
                name, app_name, new_email, link, app_name
            );
            Ok((
                "Confirm your new sign-in email".to_string(),
                body,
                String::new(),
            ))
        }
        // The inline safety net for the operator's manual switch (2026-11-12) — reached when the
        // account has no `plan_changed` template row, which is every install today. It says the
        // three things a customer needs: WHICH plan they are on now, that NOTHING WAS CHARGED here
        // (a person made the switch), and where to see what the plan allows.
        "plan_changed" => {
            let plan = vars
                .get("plan")
                .and_then(|v| v.as_str())
                .unwrap_or("a new plan");
            let body = format!(
                "Hi {},\n\nYour workspace is now on the {} plan.\n\nNothing was charged here: a person on our team made the switch for you. Everything the plan allows is on your dashboard, and the Plan section shows the allowances you are on: {}/dashboard\n\nIf this is not what you asked for, reply to this email and we will put it back.\n\n- The {} Team",
                name, plan, app_url, app_name
            );
            Ok((
                format!("You're now on {} — {}", plan, app_name),
                body,
                String::new(),
            ))
        }
        // The inline safety net for the customer's own answered request (2026-10-19) — reached when
        // the account has no `plan_request_update` template row. It says the three things the
        // customer needs: WHICH request, its state now, and where to read the team's note again.
        "plan_request_update" => {
            let request = vars
                .get("request")
                .and_then(|v| v.as_str())
                .unwrap_or("your request");
            let status_word = vars
                .get("status_word")
                .and_then(|v| v.as_str())
                .unwrap_or("updated");
            let reply = vars.get("reply").and_then(|v| v.as_str()).unwrap_or("");
            let note = if reply.trim().is_empty() {
                String::new()
            } else {
                format!("\n\nThe team's note:\n\n{}\n", reply.trim())
            };
            let body = format!(
                "Hi {},\n\nHere is an update on \"{}\" — it is now: {}.{}\n\nYou can read this on your dashboard's Plan & billing screen at {}/dashboard/ at any time.\n\n- The {} Team",
                name, request, status_word, note, app_url, app_name
            );
            Ok((
                format!("About your request — {}", app_name),
                body,
                String::new(),
            ))
        }
        // THE ANSWER TO A MESSAGE FROM /contact.html (2026-11-07). This arm exists because the
        // `plan_request_update` body above was being reused for a CONTACT-PAGE VISITOR, and it
        // tells the reader to open `{app_url}/dashboard/` — a door someone with no account cannot
        // unlock, and the reference they were handed at filing time appeared nowhere in the mail.
        // The one route that DOES work (contact.html plus reference plus the address they wrote
        // from) was never named. So name it, and never mention a dashboard here.
        "public_support_reply" => {
            let asked = vars
                .get("request")
                .and_then(|v| v.as_str())
                .unwrap_or("your message");
            let status_word = vars
                .get("status_word")
                .and_then(|v| v.as_str())
                .unwrap_or("answered");
            let reply = vars.get("reply").and_then(|v| v.as_str()).unwrap_or("");
            let reference = vars
                .get("reference")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let body = format!(
                "Hi {},\n\nYou wrote to us about \"{}\". Our answer is below.\n\n{}\n\n\
                 Your message now reads {}.\n\n\
                 You do not need an account to read this again: open {}/contact.html and enter your \
                 reference {} together with the email address you wrote from. Keep this reference — \
                 it is only usable together with that address.\n\n\
                 You can reply to this email and it reaches the same team, so you never have to file \
                 a second message.\n\n- The {} Team",
                name,
                asked,
                reply.trim(),
                status_word,
                app_url,
                reference,
                app_name
            );
            Ok((
                format!("About your message — {}", app_name),
                body,
                String::new(),
            ))
        }
        // The inline safety net for the receipt the filer gets the moment the ticket row is written
        // (cron 2026-10-20) — reached when the account has no `support_request_receipt` template row.
        // On an install with no payment provider this ticket IS the buying path, so a silent receipt
        // is a customer who asked to hand over money and heard nothing. It names their request back,
        // says a person answers by email, and says plainly that nothing has been charged.
        "support_request_receipt" => {
            let request = vars
                .get("request")
                .and_then(|v| v.as_str())
                .unwrap_or("your request");
            let body = format!(
                "Hi {},\n\nThank you — we have your request:\n\n    {}\n\nA person, not a robot, reads this and replies by email. Nothing has been charged, and nothing will be charged until you agree to it.\n\nYou can follow its state on your dashboard's Plan & billing screen at {}/dashboard/ at any time; it updates as the team moves it along.\n\n- The {} Team",
                name, request, app_url, app_name
            );
            Ok((
                format!("We have your request — {}", app_name),
                body,
                String::new(),
            ))
        }
        // The OPERATOR's copy of a plan request (2026-11-09). The customer gets the receipt above;
        // this arm is the other half of the same event, and without it an ask nobody is told about
        // is a sale nobody makes (FunnelSwift, WorkflowSwift and ADASwift closed this in 2026-11-09).
        "plan_request_operator_alert" => {
            let workspace = vars
                .get("workspace")
                .and_then(|v| v.as_str())
                .unwrap_or("A workspace");
            let plan = vars
                .get("plan")
                .and_then(|v| v.as_str())
                .unwrap_or("a plan");
            let requester = vars
                .get("requester")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let admin_url = vars
                .get("admin_url")
                .and_then(|v| v.as_str())
                .unwrap_or("https://admin.incentiveswift.com");
            let subject = format!("A customer asked to buy {plan}");
            let text = format!(
                "{workspace} asked to be moved onto {plan}.\n\nRequested by: {requester}\nPlan: {plan}\n\nThe request is waiting in the admin console (Plans -> Plan requests). Moving it to done puts that workspace on the plan: {admin_url}\n\nNothing has been charged. This message is sent once per request.\n\n- {app_name}"
            );
            let html = format!(
                "<html><body style=\"font-family:Arial,sans-serif;line-height:1.6;color:#333\">\
                 <p><strong>{workspace}</strong> asked to be moved onto <strong>{plan}</strong>.</p>\
                 <p>Requested by: <strong>{requester}</strong></p>\
                 <p>The request is waiting in the admin console (Plans &rarr; Plan requests). Moving it to <em>done</em> puts that workspace on the plan.</p>\
                 <p><a href=\"{admin_url}\" style=\"color:#4f46e5\">Open the admin console</a></p>\
                 <p style=\"font-size:13px;color:#9ca3af\">Nothing has been charged. This message is sent once per request.<br>- {app_name}</p></body></html>"
            );
            Ok((subject, text, html))
        }
        // The OPERATOR's copy of an ordinary help request (2026-11-09) — reached when no stored
        // template row exists for it, which is true of every install today. Carries the words the
        // asker typed and the address a reply must go to, because the operator answers by email.
        "support_request_operator_alert" => {
            let who = vars
                .get("who")
                .and_then(|v| v.as_str())
                .unwrap_or("A customer");
            let reply_to = vars
                .get("reply_to")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let ask = vars
                .get("subject_line")
                .and_then(|v| v.as_str())
                .unwrap_or("a support request");
            let message = vars.get("message").and_then(|v| v.as_str()).unwrap_or("");
            let admin_url = vars
                .get("admin_url")
                .and_then(|v| v.as_str())
                .unwrap_or("https://admin.incentiveswift.com");
            let subject = format!("A support request needs an answer: {ask}");
            let text = format!(
                "{who} wrote to support.\n\nSubject: {ask}\nReply to: {reply_to}\n\n{message}\n\n                 Answer it in the admin console (Support -> Support requests) and the asker is mailed                  your reply: {admin_url}\n\n- {app_name}"
            );
            let html = format!(
                "<html><body style=\"font-family:Arial,sans-serif;line-height:1.6;color:#333\">\
                 <p><strong>{}</strong> wrote to support.</p>\
                 <p>Subject: <strong>{}</strong></p>\
                 <p>Reply to: <strong>{}</strong></p>\
                 <pre style=\"white-space:pre-wrap;font-family:inherit\">{}</pre>\
                 <p>Answer it in the admin console (Support &rarr; Support requests) and the asker is                  mailed your reply.</p>\
                 <p><a href=\"{}\" style=\"color:#4f46e5\">Open the admin console</a></p>\
                 <p style=\"font-size:13px;color:#9ca3af\">- {}</p></body></html>",
                esc(who),
                esc(ask),
                esc(reply_to),
                esc(message),
                admin_url,
                app_name,
            );
            Ok((subject, text, html))
        }
        // The OPERATOR's copy of a NEW SIGNUP (2026-11-09, parity with FunnelSwift and
        // WorkflowSwift). Reached when no stored template row exists for the key, which is true of
        // every install today. Until this arm existed a new customer arrived in complete silence:
        // the account, its free tier and the credentials mail were all written, and the arrival
        // only appeared in a console nobody had been told to open.
        "signup_operator_alert" => {
            let workspace = vars
                .get("workspace")
                .and_then(|v| v.as_str())
                .unwrap_or("A workspace");
            let who = vars
                .get("who")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let plan = vars
                .get("plan")
                .and_then(|v| v.as_str())
                .unwrap_or("no plan attached");
            let source = vars
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let admin_url = vars
                .get("admin_url")
                .and_then(|v| v.as_str())
                .unwrap_or("https://admin.incentiveswift.com");
            let subject = format!("New workspace signed up: {workspace}");
            let text = format!(
                "A new workspace signed up.\n\nWorkspace: {workspace}\nOwner: {who}\nPlan: {plan}\nSource: {source}\n\nThe account is live. It is in the admin console (Accounts) and on the dashboard: {admin_url}\n\n- {app_name}"
            );
            let html = format!(
                "<html><body style=\"font-family:Arial,sans-serif;line-height:1.6;color:#333\">\
                 <p>A new workspace signed up.</p>\
                 <p><strong>{}</strong><br>\
                 Owner: {}<br>\
                 Plan: {}<br>\
                 Source: {}</p>\
                 <p>The account is live. It is in the admin console (Accounts) and on the dashboard.</p>\
                 <p><a href=\"{}\" style=\"color:#4f46e5\">Open the admin console</a></p>\
                 <p style=\"font-size:13px;color:#9ca3af\">- {}</p></body></html>",
                esc(workspace),
                esc(who),
                esc(plan),
                esc(source),
                admin_url,
                app_name,
            );
            Ok((subject, text, html))
        }
        // The customer's own branding sample (cron 2026-10-23): the header above this body shows
        // what every mail from this workspace carries. Reached when the account has no
        // `branding_preview` template row — true of every account today.
        //
        // 2026-10-24: `logo_url` is ALWAYS bound now (this app's wordmark is the fallback), so the
        // copy reads the two flags instead of assuming a header means the account's own: an upload
        // they never made must not be described as one, and a header this app supplied must not be
        // called "yours". THREE states, because an account can have a display name and no logo —
        // its header is then its own name with no image.
        "branding_preview" => {
            let own_logo = vars.get("logo_is_own").and_then(|v| v.as_str()) == Some("yes");
            let own_brand = vars.get("brand_is_own").and_then(|v| v.as_str()) == Some("yes");
            let header_line = match (own_logo, own_brand) {
                (true, _) => "The header above this line is YOUR branding: your logo and your display name. If it is right here, it is right on the sign-in details a new user receives and on every password reset.",
                (false, true) => "The name above this line is yours. No logo of your own is set yet — upload one on the Email branding card and it will appear beside your name on every message your customers receive.",
                (false, false) => "No logo or display name of your own is set yet, so this message carries the IncentiveSwift wordmark and name. Set a display name or upload a logo on the Email branding card, save it, and send yourself another preview to see your own.",
            };
            let body = format!(
                "Hi {},\n\nThis is what the top of every email from your workspace looks like.\n\n{}\n\nNothing else about your mail changes: each message's wording comes from its own template; only the header comes from your branding.\n\nTo change it, open the Email branding card on your dashboard at {}/dashboard/ .\n\n- The {} Team",
                name, header_line, app_url, app_name
            );
            Ok((
                format!("Your email branding — a preview from {}", app_name),
                body,
                String::new(),
            ))
        }
        // THE STORAGE RUNG REMINDER (kanban t_82551415; the arm proved on WorkflowSwift 8d009c9).
        // This app carries NO `storage_warning` row in `email_templates`, so without this branch the
        // 50/75/99 mail that `evaluate_and_notify` claims each month reached the customer as the
        // generic "{app} Notification:" body — for the one message whose entire value is its
        // numbers. Plain sentences on purpose: the subject names the real percentage because a
        // customer reads that first, and the body NEVER says mail will be deleted to make room,
        // because keeping that promise is what the whole storage rule exists for.
        "storage_warning" => {
            let used = vars.get("used").and_then(|v| v.as_str()).unwrap_or("some");
            let quota = vars
                .get("quota")
                .and_then(|v| v.as_str())
                .unwrap_or("your plan allowance");
            let percent = vars.get("percent").and_then(|v| v.as_i64()).unwrap_or(0);
            let threshold = vars.get("threshold").and_then(|v| v.as_i64()).unwrap_or(50);
            let inbox_url = vars
                .get("inbox_url")
                .and_then(|v| v.as_str())
                .unwrap_or(APP_URL_MAIL);
            let plans_url = vars
                .get("plans_url")
                .and_then(|v| v.as_str())
                .unwrap_or(app_url);
            let retention = vars.get("retention").and_then(|v| v.as_str()).unwrap_or("");
            let subject = format!("Your mailbox is {percent}% full — {used} of {quota} used");
            let action = if percent >= 100 {
                "Your mailbox is full. Mail is still arriving and nothing has been deleted, but new \
                 attachments and files are paused until you clear space or upgrade."
            } else if threshold >= 99 {
                "Your mailbox is almost full. Plain mail still arrives and nothing is deleted, but \
                 once the allowance is reached new attachments and files are paused."
            } else {
                "You have room for now. Clearing out mail you no longer need (Trash and Spam count \
                 too) keeps the meter down, and a bigger plan raises the allowance."
            };
            let text_body = format!(
                "Hi {name},\n\nThis is a storage update for your mailbox: you are using {used} of \
                 {quota} ({percent}%).\n\n{action}\n\nTwo things that are ALWAYS true here: mail sent \
                 to you keeps arriving, and nothing is deleted to make room for new mail. The only \
                 thing a full mailbox pauses is adding new attachments or files.\n\nOpen your mail and \
                 clear some space:\n    {inbox_url}\n\nSee the plans with more storage:\n    \
                 {plans_url}\n\n{retention}\n\nYou will get at most one of these messages per storage \
                 level per month.\n\n- The {app_name} Team"
            );
            let safe_name = esc(name);
            let safe_action = esc(action);
            let safe_retention = esc(retention);
            let html_body = format!(
                "<html><body style=\"font-family:Arial,sans-serif;line-height:1.6;color:#333\">\
                 <p>Hi {safe_name},</p>\
                 <p>This is a storage update for your mailbox: you are using <strong>{used} of {quota} ({percent}%)</strong>.</p>\
                 <p>{safe_action}</p>\
                 <p>Two things that are <strong>always</strong> true here: mail sent to you keeps arriving, and <strong>nothing is deleted</strong> to make room for new mail. The only thing a full mailbox pauses is adding new attachments or files.</p>\
                 <p><a href=\"{inbox_url}\" style=\"color:#4f46e5\">Open your mail and clear some space</a></p>\
                 <p><a href=\"{plans_url}\" style=\"color:#4f46e5\">See the plans with more storage</a></p>\
                 <p style=\"font-size:13px;color:#9ca3af\">{safe_retention}<br>You will get at most one of these messages per storage level per month.<br>- The {app_name} Team</p></body></html>"
            );
            Ok((subject, text_body, html_body))
        }
        _ => {
            let body = format!("{} Notification:\n\n{}", app_name, vars);
            Ok((format!("{} Notification", app_name), body, String::new()))
        }
    }
}

/// Keep original functions for backward compatibility — now use DB templates
pub async fn send_welcome_email(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    to: &str,
    name: &str,
    password: &str,
) -> Result<(), String> {
    // The CREDENTIALS flavour of the welcome mail, and the reason it has its own
    // `template_type` (kanban t_e43521d2).
    //
    // The shared `welcome` row is selected by the SELF-SIGNUP producer
    // (`handlers::auth_handler::register`), where the account holder typed their own password
    // seconds earlier — so that row cannot honestly carry a `Password: {{password}}` line:
    // there is no password for the sender to bind, and mailing the one the user just chose is
    // exactly what the fleet's other credential mails avoid (WorkflowSwift
    // `checkout_handler.rs`, missedcallrespondr `checkout_handler.rs` and FunnelSwift
    // `admin_handler.rs` all bind a GENERATED password, never a user-chosen one).
    //
    // This producer is the Stripe/checkout onboarding: it MINTS the password
    // (`generate_temp_password()` in `billing::webhooks`), so it is the one flow that owes the
    // recipient the credential. `welcome_credentials` is seeded with that row by
    // `migrations/20260926_email_template_brace_vocabulary.sql`, which copies the original
    // `welcome` text — password line included — so no content was rewritten or lost, only
    // relocated to the template_type that can bind all of it.
    let vars = json!({
        "name": name,
        "email": to,
        "password": password,
        "app_name": APP_NAME,
        "login_url": APP_URL,
    });
    send_template_email(pool, Some(account_id), to, "welcome_credentials", &vars).await
}

pub async fn send_purchase_confirmed_email(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    to: &str,
    name: &str,
    plan_name: &str,
) -> Result<(), String> {
    let vars = json!({
        "name": name,
        "plan_name": plan_name,
        "app_url": "https://app.incentiveswift.com",
    });
    send_template_email(pool, Some(account_id), to, "purchase_confirmed", &vars).await
}

pub async fn send_reset_email(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    to: &str,
    token: &str,
    name: &str,
) -> Result<(), String> {
    let vars = json!({
        "token": token,
        // The customer's own name, not the "there" placeholder the reset mail used to greet every
        // recipient with (fixed 2026-10-13, cron `default`).
        "name": if name.trim().is_empty() { "there" } else { name.trim() },
        "app_url": "https://app.incentiveswift.com",
    });
    send_template_email(pool, Some(account_id), to, "password_reset", &vars).await
}

/// Tell a customer their own request moved (2026-10-19).
///
/// Until this existed the loop ended silently: a workspace asked for a paid plan from the Plan &
/// billing screen, the team answered (or moved the state) in the operator console, and the customer
/// was never told — the only way to learn the answer was to keep re-opening the dashboard. FunnelSwift
/// (`plan_request_update`) and WorkflowSwift ("the customer is told when their request is handled")
/// already mail on this event; this is the same mail for this app's own ticket queue.
///
/// `account_id` is the REQUESTER's account, so the mail renders that workspace's email branding.
pub async fn send_plan_request_update_email(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    to: &str,
    name: &str,
    request: &str,
    status_word: &str,
    reply: &str,
) -> Result<(), String> {
    let vars = json!({
        "name": if name.trim().is_empty() { "there" } else { name.trim() },
        "request": request,
        "status_word": status_word.replace('_', " "),
        "reply": reply,
        "app_name": APP_NAME,
        "login_url": APP_URL,
        "app_url": APP_URL,
    });
    send_template_email(pool, Some(account_id), to, "plan_request_update", &vars).await
}

/// Tell the filer we have their request, the moment it is filed (cron 2026-10-20).
///
/// On this install `payment_providers` is empty, so `POST /api/v1/checkout/upgrade` refuses every
/// purchase and the dashboard's "Request the plan" button is what a customer who wants to pay
/// actually does. Until this existed `POST /api/v1/support-tickets` wrote the row, returned 201, and
/// the filer's inbox stayed empty: they asked to hand over money and heard nothing at all until an
/// operator happened to answer. ADASwift, FunnelSwift, WorkflowSwift and MissedCall Respondr all
/// send this receipt; this app was the last one silent.
///
/// `account_id` is the FILER's account, so the mail renders that workspace's own branding.
pub async fn send_support_request_receipt_email(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    to: &str,
    name: &str,
    request: &str,
) -> Result<(), String> {
    let vars = json!({
        "name": if name.trim().is_empty() { "there" } else { name.trim() },
        "request": request,
        "app_name": APP_NAME,
        "login_url": APP_URL,
        "app_url": APP_URL,
    });
    send_template_email(pool, Some(account_id), to, "support_request_receipt", &vars).await
}

/// Tell the OPERATOR that a workspace asked to buy a plan (2026-11-09).
///
/// The customer already gets a receipt ([`send_support_request_receipt_email`]); this is the other
/// half of the same event. Until it existed, `POST /api/v1/support-tickets` with
/// `source='plan_request'` (and the signup door's plan pick) wrote the row and told the customer,
/// while the person who can actually move the row was told nothing. On this install no payment
/// provider is stored, so the request IS the buying path: an ask nobody is told about is a sale
/// nobody makes. FunnelSwift, WorkflowSwift and ADASwift closed the same gap on 2026-11-09.
///
/// The destination is the operator's own address, stored beside the system mail settings
/// (`admin_settings.email` key `operator_alert_email`, set from the console's Email Provider card).
/// An unset address sends nothing — this code never invents a mailbox. Sent through the SYSTEM mail
/// configuration (`None`), not the asking workspace's, so it arrives even when that workspace's own
/// mail server is the thing that is broken.
///
/// Best effort by design: the request row is already written and the operator's queue reads it, so
/// a mail failure must never turn a recorded request into an error. Returns whether a send was
/// attempted and accepted by the send funnel, which is what the caller logs.
pub async fn send_plan_request_operator_alert(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    plan_name: &str,
) -> bool {
    let row: Option<serde_json::Value> =
        match sqlx::query_scalar("SELECT value FROM admin_settings WHERE key = 'email'")
            .fetch_optional(pool)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "plan-request operator alert: could not read the mail settings: {e}"
                );
                return false;
            }
        };
    let Some(to) = row
        .as_ref()
        .and_then(|v| v.get("operator_alert_email"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        // Nothing configured is a normal state, not a failure: no mailbox is invented.
        return false;
    };
    let who: Option<(String, String)> = sqlx::query_as(
        "SELECT COALESCE(NULLIF(btrim(name), ''), ''), COALESCE(email, '') \
           FROM accounts WHERE id = $1",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);
    let (name, email) = who.unwrap_or_default();
    // A FLEET PROBE MUST NEVER RING THE OPERATOR (measured 2026-11-11). One night of harness runs
    // against the contact door arrived as 19 "A support request needs an answer: Loyalty plan
    // request: ..." messages in the operator's inbox, every one of them filed with a reserved
    // `@example.invalid` address. The ticket row is the record and still lands in the console
    // queue; only the alert to a person is withheld, exactly as the signup and reset paths withhold
    // mail that can only bounce. A real customer's address is untouched.
    // The predicate is on the REQUESTER, not the route (kanban t_f17b5802): a reserved domain, a
    // `probe-`/`canary-`/`bounce+`/`verify` local part, or a harness-shaped workspace name.
    if let Some(reason) = crate::security::probe_addr::synthetic_requester(&email, &name) {
        tracing::info!(
            reason = %reason,
            "plan-request operator alert WITHHELD — the requester is a synthetic identity; \
             the request row is still in the operator's queue"
        );
        return false;
    }
    let workspace = if name.trim().is_empty() {
        if email.trim().is_empty() {
            "A workspace".to_string()
        } else {
            email.clone()
        }
    } else {
        name
    };
    let vars = json!({
        "workspace": workspace,
        "plan": plan_name,
        "requester": email,
        "admin_url": "https://admin.incentiveswift.com",
        "app_name": APP_NAME,
    });
    match send_template_email(pool, None, &to, "plan_request_operator_alert", &vars).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(account = %account_id, "plan-request operator alert to {to} failed: {e}");
            false
        }
    }
}

/// Tell the OPERATOR that somebody wrote to support (2026-11-09).
///
/// WHY THIS EXISTS (measured live today on this app): `POST /api/v1/public/support-request` — the
/// contact page's form, the door a LOCKED-OUT customer has — answered 201 with "Your message is
/// with our team" and queued **zero** mail. The signed-in door was nearly as silent: it mailed the
/// operator only when the request was a PLAN request ([`send_plan_request_operator_alert`]), so any
/// ordinary "I cannot sign in" ticket waited in a console nobody was told to open. FunnelSwift
/// closed the identical gap at 10:59 today.
///
/// Destination = the operator's own `operator_alert_email` (`admin_settings.email`, set from the
/// console's Email Provider card). An unset address sends nothing: this code never invents a
/// mailbox. Sent through the SYSTEM mail configuration (`None`), never the asking workspace's, so
/// it still arrives when that workspace's own mail server is the broken thing.
///
/// Best effort by design: the row is already written and the console's queue reads it, so a mail
/// failure must never turn a filed request into a 500. Returns whether a send was attempted.
pub async fn send_support_request_operator_alert(
    pool: &sqlx::PgPool,
    who: &str,
    reply_to: &str,
    subject_line: &str,
    message: &str,
) -> bool {
    // A FLEET PROBE MUST NEVER RING THE OPERATOR (measured 2026-11-11). One night of harness runs
    // against the contact door arrived as 19 "A support request needs an answer: Loyalty plan
    // request: ..." messages in the operator's inbox, every one of them filed with a reserved
    // `@example.invalid` address. The ticket row is the record and still lands in the console
    // queue; only the alert to a person is withheld, exactly as the signup and reset paths withhold
    // mail that can only bounce. A real customer's address is untouched.
    if let Some(reason) = crate::security::probe_addr::synthetic_requester(reply_to, who) {
        tracing::info!(
            reason = %reason,
            "support operator alert WITHHELD —the filer is a synthetic identity; \
             the ticket row is still in the operator's queue"
        );
        return false;
    }
    let row: Option<serde_json::Value> =
        match sqlx::query_scalar("SELECT value FROM admin_settings WHERE key = 'email'")
            .fetch_optional(pool)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("support operator alert: could not read the mail settings: {e}");
                return false;
            }
        };
    let Some(to) = row
        .as_ref()
        .and_then(|v| v.get("operator_alert_email"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        // Nothing configured is a normal state, not a failure: no mailbox is invented.
        return false;
    };
    let vars = json!({
        "who": if who.trim().is_empty() { "A customer" } else { who.trim() },
        "reply_to": reply_to,
        "subject_line": subject_line,
        "message": message,
        "admin_url": "https://admin.incentiveswift.com",
        "app_name": APP_NAME,
    });
    match send_template_email(pool, None, &to, "support_request_operator_alert", &vars).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("support operator alert to {to} failed: {e}");
            false
        }
    }
}

/// Tell the OPERATOR that a new workspace just signed up (2026-11-09, parity with FunnelSwift and
/// WorkflowSwift).
///
/// WHY THIS EXISTS (measured 2026-11-09): `POST /api/v1/auth/register` mints the account, its free
/// tier and the credentials mail, and NOTHING told the person who runs the install. The
/// plan-request and support-request arms next door already mail `operator_alert_email`; a new
/// customer is the same kind of news, and it is the one the operator most wants to see. FunnelSwift
/// and WorkflowSwift closed the identical gap the same day.
///
/// Destination = the operator's own address (`admin_settings.email` key `operator_alert_email`, set
/// from the console's Email Provider card). An unset address sends nothing: this code never invents
/// a mailbox. Sent through the SYSTEM mail configuration (`None`), never the new workspace's,
/// because that workspace has no sender identity yet and this is platform mail.
///
/// Never called for a harness/probe signup — the caller gates on the same two suppression reasons
/// the credentials mail uses, so machinery cannot mail the operator. Best effort by design: the
/// account is already committed, so a mail failure is logged and the caller keeps its 201.
pub async fn send_signup_operator_alert(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    owner_name: &str,
    owner_email: &str,
    source: &str,
) -> bool {
    // A FLEET PROBE MUST NEVER RING THE OPERATOR (kanban t_f17b5802, extended 2026-11-13).
    // The caller gates on the probe header and `harness_domain` — the MAIL-suppression class.
    // The fleet's disposable inbox (`maxxspace.com`) is deliberately NOT in that class (the
    // credential-delivery proof reads its password back out of it), so an audit signup through it
    // still pages a human. Measured live 2026-11-13: 31 "New workspace"/"plan request" alerts to
    // the operator's real mailbox from IncentiveSwift alone. Requester, not route: the same door
    // serves real customers.
    if let Some(reason) = crate::security::probe_addr::synthetic_requester(owner_email, owner_name)
    {
        tracing::info!(
            reason = %reason,
            "signup operator alert WITHHELD — the new workspace is a synthetic identity"
        );
        return false;
    }
    let row: Option<serde_json::Value> =
        match sqlx::query_scalar("SELECT value FROM admin_settings WHERE key = 'email'")
            .fetch_optional(pool)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("signup operator alert: could not read the mail settings: {e}");
                return false;
            }
        };
    let Some(to) = row
        .as_ref()
        .and_then(|v| v.get("operator_alert_email"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        // Nothing configured is a normal state, not a failure: no mailbox is invented.
        return false;
    };
    // The workspace name is what the operator recognises; a blank one falls back to the owner's
    // address, never to an invented label. The tier's DISPLAY name is read too, so the line says
    // "Free" and not the primary key of a plan row.
    let found: Option<(String, String)> = sqlx::query_as(
        "SELECT COALESCE(NULLIF(btrim(a.name), ''), ''), \
                COALESCE(NULLIF(btrim(p.name), ''), 'no plan attached') \
           FROM accounts a LEFT JOIN plan_tiers p ON p.id = a.plan_tier_id \
          WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);
    let (stored_name, plan_name) = found.unwrap_or_default();
    let workspace = if stored_name.trim().is_empty() {
        if owner_email.trim().is_empty() {
            "A workspace".to_string()
        } else {
            owner_email.to_string()
        }
    } else {
        stored_name
    };
    let plan_name = if plan_name.trim().is_empty() {
        "no plan attached".to_string()
    } else {
        plan_name
    };
    let who = if owner_name.trim().is_empty() {
        owner_email.to_string()
    } else {
        format!("{} ({})", owner_name.trim(), owner_email)
    };
    let vars = json!({
        "workspace": workspace,
        "who": who,
        "plan": plan_name,
        "source": if source.trim().is_empty() { "unknown" } else { source.trim() },
        "admin_url": "https://admin.incentiveswift.com",
        "app_name": APP_NAME,
    });
    match send_template_email(pool, None, &to, "signup_operator_alert", &vars).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(account = %account_id, "signup operator alert to {to} failed: {e}");
            false
        }
    }
}

/// Tell the VISITOR who used the contact page that their message moved (2026-11-06).
///
/// Same mail as [`send_plan_request_update_email`], but the asker has no workspace: a nil account
/// resolves no stored template and no stored branding, so the app's own header and default copy are
/// used. The address is the one the visitor typed; a reserved/harness recipient is withheld by the
/// send funnel itself, so a probe is never mailed.
pub async fn send_public_support_reply_email(
    pool: &sqlx::PgPool,
    to: &str,
    reference: &str,
    request: &str,
    status_word: &str,
    reply: &str,
) -> Result<(), String> {
    let vars = json!({
        "name": "there",
        "request": request,
        "status_word": status_word.replace('_', " "),
        "reply": reply,
        // The reference the visitor was handed at filing time (the ticket id). It is the OTHER half
        // of the credential that reads the answer back on /contact.html, so it must travel in the
        // mail — an earlier version reused the `plan_request_update` body, which sent this reader
        // to a dashboard they have no account for and never named the reference at all.
        "reference": reference,
        "app_name": APP_NAME,
        "login_url": APP_URL,
        "app_url": APP_URL,
    });
    send_template_email(pool, None, to, "public_support_reply", &vars).await
}

/// The operator's view of the LAST transactional send this installation attempted.
///
/// The send funnel already LOGS an outcome, but a log is not a panel. FunnelSwift records its own
/// last send (2026-10-14) and its comment names the siblings that do — ADASwift `email_send_state`,
/// MissedCall `email_last_send`, WorkflowSwift `last_send_ok`; IncentiveSwift recorded NOTHING, so
/// an operator could not tell from any screen whether a customer's credentials mail actually left.
/// Same key and shape as FunnelSwift (`admin_settings.email_last_send`), returned by
/// `GET /api/v1/admin/email-settings` and rendered on the console's Email Provider screen.
///
/// A WITHHELD send is recorded too (`ok: false` with the reason): the operator's question is "did the
/// customer get it", and a withheld message means no. Writing the record NEVER fails a send — an
/// error here is logged and dropped.
pub(crate) async fn record_last_send(
    pool: &sqlx::PgPool,
    to: &str,
    subject: &str,
    ok: bool,
    detail: &str,
) {
    write_panel(pool, to, subject, ok, detail).await;
    append_log(
        pool,
        to,
        subject,
        if ok { "sent" } else { "failed" },
        detail,
    )
    .await;
}

/// Append one row to the delivery history (`email_send_log`, migration zz_is15).
///
/// The history answers what the single slot cannot: it is append-only, so a credentials mail the
/// provider refused at signup is still readable after any later message succeeds. Writing it NEVER
/// fails a send — an error here is logged and dropped, exactly like the panel record below.
async fn append_log(pool: &sqlx::PgPool, to: &str, subject: &str, status: &str, detail: &str) {
    if let Err(e) = sqlx::query(
        "INSERT INTO email_send_log (to_email, subject, status, detail) VALUES ($1, $2, $3, $4)",
    )
    .bind(to)
    .bind(subject)
    .bind(status)
    .bind(detail.chars().take(240).collect::<String>())
    .execute(pool)
    .await
    {
        tracing::warn!("could not append to email_send_log: {e}");
    }
}

/// Write the single-value record the Email Provider panel reads
/// (`admin_settings.email_last_send`) — "what happened to the most recent message?".
async fn write_panel(pool: &sqlx::PgPool, to: &str, subject: &str, ok: bool, detail: &str) {
    let payload = json!({
        "at": chrono::Utc::now().to_rfc3339(),
        "ok": ok,
        "to": to,
        "subject": subject,
        "detail": detail.chars().take(240).collect::<String>(),
    });
    if let Err(e) = sqlx::query(
        "INSERT INTO admin_settings (key, value, description, updated_at) VALUES ('email_last_send', $1, 'The last transactional email this installation attempted to send (written by the send funnel)', NOW()) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = NOW()",
    )
    .bind(payload)
    .execute(pool)
    .await
    {
        tracing::warn!("could not record email_last_send: {e}");
    }
}

/// Record a send that was WITHHELD before any provider request (a reserved/harness recipient).
///
/// `withheld` is its own status, never `failed`: nothing was attempted, so nothing can be blamed on
/// the provider, and the panel must not tell the operator a customer was let down. The history row
/// is written on its own status; the single slot carries the same reason for the panel's one line.
pub(crate) async fn record_withheld(pool: &sqlx::PgPool, to: &str, subject: &str, reason: &str) {
    let detail = format!("withheld: {reason} (no send attempted)");
    append_log(pool, to, subject, "withheld", &detail).await;
    write_panel(pool, to, subject, false, &detail).await;
}

/// Whether a recipient can never receive mail (reserved RFC-2606 name, or a fleet harness domain).
/// Used to record an HONEST outcome for a send that was withheld before any provider request.
pub(crate) fn recipient_is_undeliverable(to: &str) -> bool {
    crate::security::probe_addr::harness_domain(to).is_some()
        || crate::security::email_addr::refuse_undeliverable_recipient(to).is_err()
}

/// Core email sender — the provider (`smtp | mailgun | sendgrid | sendiio`) and its credentials
/// come from the database (`admin_settings.email`, the fleet-wide system row). Nothing is read from
/// the process environment. The per-tenant override this used to fall back on is RETIRED (kanban
/// t_123b886b, see `email_provider`); a tenant's own server is the `smtp_*` family in
/// `delivery::sender`. This is also the arm an account with NO mail server of its own rides
/// (`delivery::sender::send_email`, kanban t_2e9117a5): one system-mail path, not two.
pub(crate) async fn send_email_request(
    pool: &sqlx::PgPool,
    to: &str,
    subject: &str,
    text_body: &str,
    html_body: &str,
) -> Result<(), String> {
    // Fleet harness addresses never reach a real relay (parity with FunnelSwift, kanban t_36b55ed2).
    // A probe that signs up with a fleet-dev domain (`swiftsoftware.dev/.net`) is created normally but
    // its mail is withheld: the address is routable, so a send can only land in a fleet mailbox or
    // bounce (measured 2026-10-09 on mail.incentiveswift.com: `accepted` then `bounced` 552), and every
    // such send burns a delivery on the domain's sending reputation. This function is the choke point
    // BOTH the template arm and the inline fallback pass through, so every transactional type is
    // covered. The RFC-2606 class is now ALSO suppressed (changed 2026-10-09 after measuring real provider attempts) (.local/.test/.invalid/example.*) is deliberately NOT suppressed —
    // content harnesses point the provider at a local sink and read the message off the wire, so
    // silencing it would delete proof.
    if let Some(domain) = crate::security::probe_addr::harness_domain(to) {
        tracing::info!(
            to = %to,
            domain = %domain,
            subject = %subject,
            "send suppressed: recipient is a fleet harness address (fleet-dev domain)"
        );
        record_withheld(pool, to, subject, "recipient is a fleet harness address").await;
        return Ok(());
    }
    let Some(cfg) = crate::email_provider::resolve(pool).await else {
        tracing::warn!(
            to = %to,
            subject = %subject,
            "email skipped — no email provider configured (Admin > Settings > Email Provider)"
        );
        record_last_send(pool, to, subject, false, "no email provider configured").await;
        return Err(
            "Email provider not configured. Set it in Admin > Settings > Email Provider."
                .to_string(),
        );
    };

    let html = if html_body.trim().is_empty() {
        None
    } else {
        Some(html_body)
    };

    // The RECIPIENT decides whether this was an attempt at all, not the shape of the outcome:
    // `deliver` refuses the RFC-2606 class with `Err("recipient-refused: …")` and the fleet-harness
    // class with `Ok(())`. Keying the withheld arm on `Ok` alone recorded a deliberately-withheld
    // message as a delivery FAILURE (measured 2026-10-18 through the panel's test-send), which is the
    // misreport the fleet rule forbids: nothing was attempted, so nothing may read as refused mail.
    let held = recipient_is_undeliverable(to);
    let outcome = crate::email_provider::deliver(pool, &cfg, to, subject, text_body, html).await;
    if held {
        record_withheld(pool, to, subject, "recipient cannot receive mail").await;
    } else {
        match &outcome {
            Ok(()) => record_last_send(pool, to, subject, true, "").await,
            Err(e) => {
                tracing::warn!(provider = %cfg.provider, to = %to, "email send failed: {e}");
                record_last_send(pool, to, subject, false, e).await;
            }
        }
    }
    // The OUTCOME is returned unchanged: `email_queue` deliberately treats a refused recipient as an
    // ordinary bounded failure (3 attempts, no provider request) and dead-letters it, which is the
    // surface that makes such a row visible. Only the RECORDING is honest now.
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::branding::app_default_branding;

    /// The answer to a CONTACT-PAGE visitor names a door they can actually open (2026-11-07).
    ///
    /// The visitor who files from `/contact.html` has no account, so the reply must not send them to
    /// a dashboard. It has to carry the two things that DO work: the page, and the reference they
    /// were handed at filing time (which is only usable together with the address they wrote from).
    /// This renders the real arm — the same function the live send runs — instead of asserting on
    /// source text, so a regression in the wording is a red test.
    #[tokio::test]
    async fn the_contact_page_answer_names_the_page_and_the_reference() {
        // Deliberately NOT a uuid literal: the gate refuses hardcoded UUIDs in src/, and this test
        // only needs a reference the body must echo back.
        let reference = "probe-reference-abc123";
        let vars = json!({
            "name": "there",
            "request": "Locked out of my account",
            "status_word": "answered",
            "reply": "Use the reset link on the sign-in page.",
            "reference": reference,
            "app_name": APP_NAME,
            "app_url": APP_URL,
            "login_url": APP_URL,
        });
        let (subject, body, _html) = send_inline(
            "probe@example.invalid",
            "public_support_reply",
            &vars,
            APP_NAME,
            APP_URL,
        )
        .await
        .expect("the contact-page answer renders");
        assert_eq!(subject, format!("About your message — {APP_NAME}"));
        assert!(body.contains("/contact.html"), "{body}");
        assert!(body.contains(reference), "{body}");
        assert!(
            body.contains("Use the reset link on the sign-in page."),
            "{body}"
        );
        // The defect this arm replaces: a dashboard sentence sent to someone with no account.
        assert!(!body.to_lowercase().contains("dashboard"), "{body}");
        assert!(!body.contains("/dashboard/"), "{body}");
    }

    /// The OPERATOR's plan-request alert names the workspace, the plan and who asked (2026-11-09).
    ///
    /// The customer's receipt existed; nothing told the person who can move the request row, so a
    /// buying ask waited in the console unnoticed. This renders the real inline arm — the same one
    /// the live send uses when the installation has no DB template for the type — so a regression in
    /// the wording or the merge fields is a red test.
    #[tokio::test]
    async fn the_operator_alert_names_the_workspace_the_plan_and_the_requester() {
        let vars = json!({
            "workspace": "Acme Rewards",
            "plan": "Pro",
            "requester": "buyer@example.invalid",
            "admin_url": "https://admin.incentiveswift.com",
            "app_name": APP_NAME,
        });
        let (subject, body, html) = send_inline(
            "ops@example.invalid",
            "plan_request_operator_alert",
            &vars,
            APP_NAME,
            APP_URL,
        )
        .await
        .expect("the operator alert renders");
        assert_eq!(subject, "A customer asked to buy Pro");
        assert!(body.contains("Acme Rewards"), "{body}");
        assert!(body.contains("Pro"), "{body}");
        assert!(body.contains("buyer@example.invalid"), "{body}");
        assert!(body.contains("admin.incentiveswift.com"), "{body}");
        assert!(html.contains("admin.incentiveswift.com"), "{html}");
    }

    /// An account that has configured NOTHING still sends mail that says whose it is (2026-10-24).
    ///
    /// Before this the header block appeared only once an account had uploaded a logo of its own,
    /// so the credentials mail a brand-new customer receives — the first thing this product ever
    /// shows them — arrived with no wordmark at all. The app's OWN identity is the fallback: a PNG
    /// on a public host (mail clients do not render SVG) whose `alt` carries the name, so a client
    /// that blocks images still says whose mail this is.
    #[test]
    fn an_unbranded_account_still_gets_the_apps_own_wordmark() {
        let b = app_default_branding();
        assert_eq!(b.brand_name, APP_NAME);
        let url = b
            .resolve_logo_url(APP_URL)
            .expect("the app wordmark resolves");
        assert_eq!(url, "https://app.incentiveswift.com/logo.png");

        let header = b.header_html(Some(&url));
        assert!(header.contains("<img"), "{header}");
        assert!(header.contains(&url), "{header}");
        assert!(header.contains("alt=\"IncentiveSwift\""), "{header}");
        // ...painted in the fleet accent, which is the rule under the name.
        assert!(
            header.contains(crate::branding::DEFAULT_BRAND_COLOR),
            "{header}"
        );
        // The text part carries the name (a text part cannot carry an image).
        assert_eq!(b.text_header(), format!("{APP_NAME}\n\n"));
    }

    /// The merge fields always carry something that RESOLVES, and `logo_is_own` tells the truth.
    #[test]
    fn bind_branding_binds_the_app_identity_and_flags_whose_logo_it_is() {
        let mut v = json!({ "name": "Ada" });
        bind_branding(&mut v, None);
        assert_eq!(v.get("brand_name").and_then(|x| x.as_str()), Some(APP_NAME));
        assert_eq!(
            v.get("logo_url").and_then(|x| x.as_str()),
            Some("https://app.incentiveswift.com/logo.png")
        );
        assert_eq!(v.get("logo_is_own").and_then(|x| x.as_str()), Some("no"));
        // every other key is left alone
        assert_eq!(v.get("name").and_then(|x| x.as_str()), Some("Ada"));

        // An account with its OWN name but no logo keeps its name, and the flag still says the
        // header's logo is not theirs.
        let own_name = crate::branding::Branding {
            brand_name: "Giraudy Capital".into(),
            brand_color: "#0ea5e9".into(),
            logo_url: String::new(),
        };
        let mut v = json!({});
        bind_branding(&mut v, Some(&own_name));
        assert_eq!(
            v.get("brand_name").and_then(|x| x.as_str()),
            Some("Giraudy Capital")
        );
        assert_eq!(v.get("logo_url").and_then(|x| x.as_str()), Some(""));
        assert_eq!(v.get("logo_is_own").and_then(|x| x.as_str()), Some("no"));

        // ...and an account WITH a logo is described as having one.
        let with_logo = crate::branding::Branding {
            brand_name: "Giraudy Capital".into(),
            brand_color: String::new(),
            logo_url: "/api/v1/branding/logo/abc?v=7".into(),
        };
        let mut v = json!({});
        bind_branding(&mut v, Some(&with_logo));
        assert_eq!(v.get("logo_is_own").and_then(|x| x.as_str()), Some("yes"));
        assert_eq!(
            v.get("logo_url").and_then(|x| x.as_str()),
            Some("https://app.incentiveswift.com/api/v1/branding/logo/abc?v=7")
        );

        // a map that is not an object is replaced, never panics
        let mut v = json!("nope");
        bind_branding(&mut v, None);
        assert!(v.is_object());
    }

    /// `None` still returns its input untouched — the shape the additive leg pins, and what keeps
    /// a caller that wants an unbranded render able to ask for one.
    #[test]
    fn with_branding_header_none_is_still_a_no_op() {
        let (t, h) = with_branding_header(None, "body".to_string(), "<p>body</p>".to_string());
        assert_eq!(t, "body");
        assert_eq!(h, "<p>body</p>");
    }

    /// The fallback header opens BOTH parts, and builds an HTML part when there is none (every
    /// inline fallback returns text only — including the branding preview itself).
    #[test]
    fn the_app_header_opens_both_parts_of_a_text_only_mail() {
        let b = app_default_branding();
        let (text, html) = with_branding_header(
            Some(&b),
            "Hi Ada,\n\n<not markup>".to_string(),
            String::new(),
        );
        assert!(text.starts_with(&format!("{APP_NAME}\n\nHi Ada")), "{text}");
        assert!(html.contains("<img"), "{html}");
        assert!(
            html.contains("https://app.incentiveswift.com/logo.png"),
            "{html}"
        );
        // the body's own bytes are escaped, not interpreted
        assert!(html.contains("&lt;not markup&gt;"), "{html}");
        assert!(!html.contains("<not markup>"), "{html}");
    }

    /// The preview mail must not call this app's own header "your branding" (2026-10-24).
    #[tokio::test]
    async fn the_branding_preview_describes_the_header_it_actually_sent() {
        // An account with no branding: the honesty arm.
        let mut vars = json!({ "name": "Ada" });
        bind_branding(&mut vars, None);
        let (subject, text, _html) = send_inline(
            "ada@example.org",
            "branding_preview",
            &vars,
            APP_NAME,
            APP_URL,
        )
        .await
        .expect("the inline preview renders");
        assert!(subject.contains(APP_NAME), "{subject}");
        assert!(
            text.contains("No logo or display name of your own is set yet"),
            "unbranded preview must say whose header it carries: {text}"
        );
        assert!(
            !text.contains("is YOUR branding"),
            "must not call the app header theirs: {text}"
        );

        // An account with a display name and NO logo: its own name, no image, and copy that says
        // exactly that instead of claiming either of the other two states.
        let name_only = crate::branding::Branding {
            brand_name: "Giraudy Capital".into(),
            brand_color: String::new(),
            logo_url: String::new(),
        };
        let mut vars = json!({ "name": "Ada" });
        bind_branding(&mut vars, Some(&name_only));
        let (_, text, _) = send_inline(
            "ada@example.org",
            "branding_preview",
            &vars,
            APP_NAME,
            APP_URL,
        )
        .await
        .expect("the inline preview renders");
        assert!(text.contains("The name above this line is yours"), "{text}");
        assert!(!text.contains("is YOUR branding"), "{text}");

        // An account WITH a logo: the other arm.
        let own = crate::branding::Branding {
            brand_name: "Giraudy Capital".into(),
            brand_color: String::new(),
            logo_url: "/api/v1/branding/logo/abc?v=7".into(),
        };
        let mut vars = json!({ "name": "Ada" });
        bind_branding(&mut vars, Some(&own));
        let (_, text, _) = send_inline(
            "ada@example.org",
            "branding_preview",
            &vars,
            APP_NAME,
            APP_URL,
        )
        .await
        .expect("the inline preview renders");
        assert!(text.contains("is YOUR branding"), "{text}");
        assert!(!text.contains("No logo of your own is set yet"), "{text}");
    }
}
