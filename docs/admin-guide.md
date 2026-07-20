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
