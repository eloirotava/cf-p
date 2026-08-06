#!/usr/bin/env bash
set -Eeuo pipefail

command -v openssl >/dev/null 2>&1 || {
  printf 'erro: openssl não encontrado\n' >&2
  exit 1
}

TOKEN="$(openssl rand -hex 32)"
if command -v sha256sum >/dev/null 2>&1; then
  TOKEN_SHA256="$(printf '%s' "$TOKEN" | sha256sum | awk '{print $1}')"
else
  TOKEN_SHA256="$(printf '%s' "$TOKEN" | openssl dgst -sha256 -r | awk '{print $1}')"
fi

cat <<EOF
Token para usar no cfp-client:
$TOKEN

Hash para usar em token_sha256 no server.yaml:
$TOKEN_SHA256

Trecho de configuração:
clients:
  - token_sha256: "$TOKEN_SHA256"
    routes:
      - listen: "0.0.0.0:33890"
        target: "127.0.0.1:3389"

Comando do cliente:
CFP_SERVER=wss://a.rotava.com CFP_TOKEN="$TOKEN" ./cfp-client
EOF
