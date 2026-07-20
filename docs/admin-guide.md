# IncentiveSwift Admin Guide

## Overview
Multi-tenant loyalty engine powering ZaarHub's loyalty programs, purchase verification, rotating cross-promotions, and referral systems.

## Tenant Roles

| Role | Description |
|---|---|
| `admin` | Full system access, impersonation, all tenants |
| `portfolio_company` | Sister company (ZaarHub) — owns its campaigns and loyalty |
| `company_admin` | External business tenant — isolated from ZaarHub |

## ZaarHub Loyalty Campaigns

### 1. ZaarHub Local Pass (B2C) — ZaarCash 🟠
| Earning Action | Points |
|---|---|
| Purchase at claimed business + PIN verification | +50 |
| Daily check-in | +10 |
| Refer a friend who buys | +100 |
| Social share | +5 |

| Reward Tier | Points | Type |
|---|---|---|
| Free Coffee Voucher | 100 | Auto |
| 25% Off Local Dining | 300 | Auto |
| VIP Local Pass ($100 Service Credit) | 1,000 | Admin approval |
| Featured Directory Spotlight (30 days) | 2,500 | Admin approval |

### 2. ZaarHub B2B Supplier Loop — Pro Credits 💼
| Earning Action | Credits |
|---|---|
| Purchase from listed supplier | +200 |
| Refer another business to ZaarHub | +500 |

| Reward Tier | Credits | Type |
|---|---|---|
| Featured Supplier Badge (30 days) | 500 | Admin approval |
| Newsletter Ad Placement | 1,500 | Admin approval |
| AI Lead Campaign (100 leads) | 3,000 | Admin approval |

## Purchase Verification Flow
1. Business generates 4-digit PIN via their portal
2. Customer enters PIN in ZaarHub to verify purchase
3. Rotating cross-promo voucher issued automatically
4. Customer redeems voucher at non-competing claimed business within 30 days

## Business Pledge Flow
1. Claimed business submits reward offer (e.g., "15% off")
2. Status: `pending` → admin reviews
3. Once approved, business joins the loyalty rotation
4. Only businesses with active pledges participate in the network


## Rotating Cross-Promotion Engine

When a purchase is verified, the system automatically issues a voucher to a non-competing business in the same rotation group:

1. Admin creates a **rotation config** for a campaign
2. Admin adds businesses to rotation groups (non-competing categories)
3. When a customer verifies a purchase at Business A, a voucher for Business B is auto-issued
4. Customer redeems at Business B within 30 days

### Rotation Group API

| Endpoint | Method | Description |
|---|---|---|
| `POST /api/v1/admin/rotation-configs` | POST | Create rotation config |
| `GET /api/v1/admin/rotation-configs/:slug` | GET | List configs for campaign |
| `POST /api/v1/admin/rotation-members` | POST | Add business to rotation |
| `GET /api/v1/admin/rotation-members/:config_id` | GET | List members |
| `DELETE /api/v1/admin/rotation-members/:config_id/:bus_id` | DELETE | Remove member |

## Reward Redemption

Consumers can redeem ZaarCash or Pro Credits for rewards. Fires a webhook to the tenant when redeemed.

| Endpoint | Method | Description |
|---|---|---|
| `POST /api/v1/loyalty/redeem-reward` | POST | Redeem points for reward |
| `GET /api/v1/loyalty/rewards-earned/:contact_id` | GET | List earned rewards |

## Auto-Expire

Vouchers expire after 30 days. Cron runs every 6 hours: `POST /api/v1/loyalty/expire-vouchers`

## API Endpoints

| Endpoint | Method | Who | What |
|---|---|---|---|
| `/api/v1/loyalty/generate-pin` | POST | Business | Generate PIN for customer |
| `/api/v1/loyalty/verify-purchase` | POST | Consumer | Enter PIN to verify |
| `/api/v1/loyalty/issue-voucher` | POST | Internal | Issue rotating voucher |
| `/api/v1/loyalty/my-vouchers/:id` | GET | Consumer | List vouchers |
| `/api/v1/loyalty/claim-voucher` | POST | Consumer | Redeem voucher |
| `/api/v1/business/pledge` | POST | Business | Submit reward offer |
| `/api/v1/business/pledges/:id` | GET | Business | View pledges |
| `/api/v1/admin/pledges` | GET | Admin | Pending approvals |
| `/api/v1/admin/pledges/:id/review` | POST | Admin | Approve/reject |
| `/api/v1/admin/impersonate` | POST | Admin | Switch to portfolio company |
