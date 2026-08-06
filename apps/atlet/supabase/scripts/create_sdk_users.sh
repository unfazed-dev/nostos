#!/usr/bin/env bash
set -euo pipefail
: "${SUPABASE_URL:?}" "${SUPABASE_SERVICE_ROLE_KEY:?}" "${ATLET_SDK_USER_PASSWORD:?}"
for sdk in flutter react_native web kotlin swift node capacitor tauri dotnet; do
  curl -sf -X POST "$SUPABASE_URL/auth/v1/admin/users" \
    -H "apikey: $SUPABASE_SERVICE_ROLE_KEY" \
    -H "Authorization: Bearer $SUPABASE_SERVICE_ROLE_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"email\":\"$sdk@atlet.internal\",\"password\":\"$ATLET_SDK_USER_PASSWORD\",\"email_confirm\":true}" \
    && echo " created: $sdk@atlet.internal" || echo " exists/failed: $sdk (check manually)"
done
