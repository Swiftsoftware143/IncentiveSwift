# IncentiveSwift — Ecosystem Architecture

## What IncentiveSwift is

IncentiveSwift is a single-purpose engagement and capture engine. It runs ten light,
gamified incentive mechanics (spin wheel, scratch card, calculator, personality quiz,
mystery reveal, countdown, poll, chat funnel, benchmark, score reveal), a raffle/
giveaway system, and a long-form qualifier for high-ticket pre-qualification — twelve
mechanics total. It also offers an optional Loyalty Program module — a separate,
recurring point-based check-in system sellable as an add-on upsell — that sits
alongside the 12 single-moment mechanics rather than being one of them. It captures
whoever interacts with these mechanics, scores or qualifies them where relevant,
applies a tag, and pushes the tagged contact outward.

That is the entire job. IncentiveSwift does not run campaigns. It does not manage a
pipeline. It does not send follow-up sequences. It does not own a CRM. It has no
opinion about what happens to a contact after the tag is applied — that decision
belongs entirely to whatever sits downstream.

## What IncentiveSwift is not

- Not a delivery system. It pushes once and is done.
- Not connected to FunnelSwift's demo engine. The two apps do not call each other.
- Not a card-scanning or manual live-entry tool. That is FunnelSwift's job.
- Not a pipeline or campaign manager. That logic lives downstream.

## The one-way flow

```
 ┌─────────────────────────┐
 │ IncentiveSwift           │
 │                          │
 │ 10 Incentive Mechanics   │
 │ Raffle & Giveaway System │
 │                          │
 │ Captures entry           │
 │ Captures every question  │
 │ and answer given         │
 │ Scores / determines      │
 │ outcome (win/runner-up/  │
 │ entrant)                 │
 │ Applies matching tag     │
 └─────────────┬────────────┘
               │
               │ PUSH ONLY — one direction
               │ Full payload: contact + tags +
               │ questions_and_answers
               │ (webhook or direct API call —
               │ never a shared database)
               ▼
 ┌─────────────────────────┐
 │ FunnelSwift              │
 │                          │
 │ Reads the tag and the    │
 │ full Q&A payload         │
 │ Decides what to do:      │
 │ - grant software access  │
 │ - send runner-up prize   │
 │ - drop into nurture      │
 │ Owns the pipeline/campaign│
 │ Relays contact + Q&A     │
 │ onward to CRM            │
 └─────────────┬────────────┘
               │
               │ RELAY — same payload,
               │ forwarded onward
               ▼
 ┌─────────────────────────┐
 │ Third-Party CRM          │
 │ (HubSpot, GoHighLevel,   │
 │ ActiveCampaign, etc.)    │
 │                          │
 │ Stores contact + activity│
 │ history with full Q&A    │
 │ detail attached          │
 └─────────────────────────┘
```

FunnelSwift never calls IncentiveSwift. IncentiveSwift never asks FunnelSwift for
anything, and never calls the CRM directly either (unless using the direct API
delivery method for standalone use without FunnelSwift in the picture). The
question/answer detail captured at the very first step survives every hop —
IncentiveSwift captures it, FunnelSwift relays it, the CRM stores it.

## The binding mechanism: campaign name = tag namespace

Every IncentiveSwift campaign has a name. That name is the tag namespace. Nothing
else needs to be shared between the two systems — no API contract, no shared schema,
just a naming convention both sides agree on.

Example — a raffle campaign called "Summer Giveaway":

| Outcome | Tag applied | What happens downstream |
|---|---|---|
| Enters the raffle | `Summer_Giveaway_Entrant` | Goes into general nurture |
| Selected as winner | `Summer_Giveaway_Winner` | FunnelSwift/CRM grants full software access automatically |
| Selected as runner-up | `Summer_Giveaway_RunnerUp` | FunnelSwift/CRM sends secondary prize |

Same pattern applies to the 10 instant-resolution mechanics. A spin-to-win campaign
called "Spring Launch" applies `Spring_Launch_Spin_GrandPrize`, `Spring_Launch_Spin_Discount`,
etc., depending on which prize slot was hit.

The admin defines the tag suffixes per outcome when building the campaign — never
hardcoded, always editable from the dashboard.

## How the contact actually moves downstream

Two supported delivery methods, configurable per campaign in Settings. IncentiveSwift
and FunnelSwift never share a database — each app runs its own Supabase project, own
accounts, own billing. The only thing that ever crosses the boundary is a payload over
the network, never a shared table or shared connection string.

**1. Webhook push (primary)**
On entry/outcome, IncentiveSwift fires a webhook to a configured URL — FunnelSwift's
ingest endpoint, Zapier, Make.com, n8n, or any CRM webhook. Payload includes contact
info, campaign name, outcome, and tags. This is the default and the one that should
be used for the FunnelSwift connection specifically.

**2. Direct third-party API**
For solo operators without FunnelSwift in the mix at all, IncentiveSwift can push
straight to a CRM/email tool's API (HubSpot, ActiveCampaign, GoHighLevel, etc.) using
stored, admin-configured API keys per campaign. No middleman needed.

In every case, IncentiveSwift's responsibility ends the moment the push succeeds.
No retries beyond standard webhook retry/backoff, no waiting for a response to act on,
no callback expected, no shared infrastructure of any kind.

### The payload contract — questions and answers travel with the contact

This is the part that makes the relay useful rather than just a tag. Every webhook
push carries the full question/answer set from that entry, not just a score or an
outcome label. FunnelSwift receives the complete context and relays it forward to
whatever CRM it's connected to — so a sales rep looking at a contact in their CRM
sees exactly what that person answered, not just that they "entered a raffle."

```json
{
 "event": "entry.captured",
 "contact": {
 "first_name": "Marcus",
 "last_name": "Torres",
 "email": "marcus@torreselectric.com",
 "phone": "+13125550100",
 "business_name": "Torres Electric LLC"
 },
 "campaign": {
 "name": "Summer Giveaway",
 "type": "raffle",
 "tag_namespace": "Summer_Giveaway"
 },
 "outcome": "winner",
 "tags_applied": ["Summer_Giveaway_Winner"],
 "score": 74,
 "questions_and_answers": [
 {
 "question": "What's your biggest challenge right now?",
 "answer": "Not enough leads"
 },
 {
 "question": "How many leads do you get per month?",
 "answer": "10-50"
 }
 ],
 "entry_id": "uuid",
 "captured_at": "2026-06-12T14:32:00Z"
}
```

FunnelSwift's ingest endpoint reads `questions_and_answers` and writes it into the
contact's activity history alongside whatever FunnelSwift itself has already captured
about that person, then relays the same structure onward into the CRM it's connected
to (HubSpot, GoHighLevel, etc.) as custom fields or a logged activity — so the
question/answer detail isn't lost at any hop in the chain: **IncentiveSwift captures
it → FunnelSwift relays it → CRM stores it.**
