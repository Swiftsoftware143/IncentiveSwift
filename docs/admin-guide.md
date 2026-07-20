# IncentiveSwift Admin Guide

## Overview
IncentiveSwift is a multi-tenant loyalty engine powering spin-to-win, loyalty programs, referral systems, purchase verification, and rotating cross-promotions for ZaarHub and other platforms.

## Tenant Roles

| Role | Description |
|---|---|
| `admin` | Super admin — full access to all tenants, impersonation, system config |
| `portfolio_company` | Sister/portfolio company (e.g., ZaarHub) — owns campaigns, manages its own loyalty |
| `company_admin` | External business tenant — separate campaigns, isolated from ZaarHub |

## ZaarHub Loyalty Campaigns

ZaarHub has two active campaigns in IncentiveSwift:

### 1. ZaarHub Local Pass (B2C)
- **Currency:** ZaarCash 🟠
- **How consumers earn:** Purchase at claimed business + verify with PIN, refer friends, daily check-in
- **How consumers redeem:** Vouchers for non-competing local businesses (rotating cross-promo)
- **Entry requirement:** Business must claim their listing AND submit a pledge (reward offer) — admin approves

### 2. ZaarHub B2B Supplier Loop
- **Currency:** Pro Credits 💼
- **How businesses earn:** Purchase from listed suppliers on ZaarHub
- **How businesses redeem:** Featured directory placement, newsletter ads, AI lead campaigns

## Purchase Verification Flow

1. **Business generates PIN** — Business logs into their portal, clicks "Generate PIN" for a customer purchase
2. **Customer enters PIN** — Customer enters the 4-digit PIN in ZaarHub to verify the purchase
3. **Voucher issued** — System automatically issues a rotating cross-promo voucher to the customer
4. **Customer redeems** — Customer uses the voucher at another claimed business within 30 days

## Business Pledge Flow

1. Business submits a pledge with their reward offer (e.g., "15% off HVAC inspection")
2. Pledge goes to `pending` status
3. Directory admin reviews in admin panel
4. Once approved, business is activated in the loyalty rotation

## Admin Pledge Approval

`GET /api/v1/admin/pledges` — view all pending pledges
`POST /api/v1/admin/pledges/:id/review` — approve or reject

## New API Endpoints

### Consumer
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/loyalty/verify-purchase` | POST | Enter PIN to verify purchase |
| `/api/v1/loyalty/my-vouchers/:contact_id` | GET | List active vouchers |
| `/api/v1/loyalty/claim-voucher` | POST | Redeem a voucher by code |

### Business
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/loyalty/generate-pin` | POST | Generate 4-digit PIN for customer |
| `/api/v1/business/pledge` | POST | Submit reward offer |
| `/api/v1/business/pledges/:business_id` | GET | View pledges and status |

### Admin
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/admin/pledges` | GET | List pending pledges |
| `/api/v1/admin/pledges/:id/review` | POST | Approve/reject pledge |
| `/api/v1/admin/impersonate` | POST | Impersonate a portfolio company |
