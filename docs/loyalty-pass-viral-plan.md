# Loyalty Pass + Viral Program — ZaarHub

## Pre-Flight Check: What IncentiveSwift Already Has ✅

| Feature | Status |
|---|---|
| Spin-to-Win | ✅ Built |
| Loyalty check-ins (daily, online visit, share, referral click) | ✅ Built |
| Loyalty programs with tiers | ✅ Built |
| Rewards approval flow | ✅ Built |
| Referral codes per user | ✅ Built |
| Referral click tracking | ✅ Built |
| Viral earn channels | ✅ Built |
| Campaign share links | ✅ Built |
| Leaderboards | ✅ Built |
| Online stats per referral code | ✅ Built |

## What Needs to Be Built

### Phase 1: Claim Barrier — "Unlock the Network"
- Auto-generate referral/Matchmaker link on visitor signup
- Claimed business verification flow: business claims listing → gets "Official Loyalty Partner" badge + sticker
- Pin/receipt verification system (4-digit PIN in business portal, consumer enters to confirm purchase)
- Business pledge: agree to honor referral discounts during claim setup

### Phase 2: ZaarHub Local Pass — Rotating Voucher Engine
- After purchase verification, auto-issue rotating "Next-Step Voucher" from non-competing business
- Weekly rotation logic: week 1 = pizza, week 2 = auto detail, week 3 = cafe
- Dynamic "Local Passport Voucher" with 30-day expiry
- Cross-promotion campaign grouping (e.g., "Home & Hearth Rotation")

### Phase 3: ZaarCash — Rewards & Payouts
- ZaarCash virtual currency (points redeemable for featured placement, newsletter ads, AI lead campaigns)
- Cash payout integration (Venmo/PayPal/CashApp)
- Tiered referral thresholds (1 ref = free coffee, 3 refs = $25 dining, 5 refs = $100 service pass)

### Phase 4: B2B Supplier Loyalty Loop
- Supplier discount loop: 5-10% off for claimed ZaarHub businesses
- Pro Credits for B2B purchases → redeemable for directory features
- Group purchasing power display

### Phase 5: Automated Push & Messaging
- 1-click share buttons (WhatsApp, SMS, Facebook, iMessage) with pre-written templates
- Automated "Local Perk Digest" emails (3 rotating offers per zip code)
- Automated referral notifications (friend signed up, deal completed)

## Architecture

```
Consumer purchases → PIN/Receipt → Reward Engine
                                   ├── Rotating Voucher (cross-promo)
                                   ├── ZaarCash points
                                   └── Referral bounty released

Supplier sells → B2B verification → Pro Credits
                                   └── Redeemable for featured placement
```

Ready to start building Phase 1 when you give the word.
