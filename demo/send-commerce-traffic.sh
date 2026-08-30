#!/usr/bin/env bash

set -euo pipefail

LAZYAPI_URL="${LAZYAPI_URL:-http://127.0.0.1:3000}"
TENANT="X-Tenant-ID: northstar-uk"

request() {
  curl --silent --show-error --output /dev/null "$@"
  sleep 0.35
}

request -H "$TENANT" "$LAZYAPI_URL/catalog/products?query=jacket&limit=25"
request -H "$TENANT" "$LAZYAPI_URL/catalog/products/SKU-RED-42"
request \
  -X POST \
  -H "$TENANT" \
  -H "Idempotency-Key: 6f9619ff-8b86-d011-b42d-00cf4fc964ff" \
  -H "Content-Type: application/json" \
  -d '{"customerId":"cus_01J8Y8VMQY","currency":"GBP","items":[{"sku":"SKU-RED-42","quantity":2}],"metadata":{"campaign":"autumn-launch"}}' \
  "$LAZYAPI_URL/orders"
request \
  -X POST \
  -H "$TENANT" \
  -H "Content-Type: application/json" \
  -d '{"paymentMethodId":"pm_card_visa","amount":{"amount":259.90,"currency":"GBP"}}' \
  "$LAZYAPI_URL/orders/ord_01J8Y91C7K/payments"
request -H "$TENANT" "$LAZYAPI_URL/legacy/export"

# Finish on a matched operation with several deliberate contract violations.
request \
  -X POST \
  -H "$TENANT" \
  -H "Authorization: Bearer demo-secret-token" \
  -H "X-API-Key: demo-api-key" \
  -H "Content-Type: application/json" \
  -d '{"items":[{"sku":"","quantity":"many"}],"metadata":{"accessToken":"body-secret"}}' \
  "$LAZYAPI_URL/orders"
