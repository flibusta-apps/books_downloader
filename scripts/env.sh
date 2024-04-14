#! /usr/bin/env sh

response=`curl -X 'GET' "https://$VAULT_HOST/v1/$VAULT_SECRET_PATH" -s \
  -H 'accept: application/json' \
  -H "X-Vault-Token: $VAULT_TOKEN"`

echo "$(echo "$response" | jq -r '.data.data | to_entries | map("\(.key)='\''\(.value)'\''") | .[]')"
