#!/usr/bin/env bash
set -euo pipefail

CC=${CC:-cc}
STRIP=${STRIP:-strip}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

cat >"$WORK/tls-floor.c" <<'EOF'
#include <openssl/ssl.h>
int main(void) {
    SSL_CTX *ctx = SSL_CTX_new(TLS_client_method());
    if (ctx == NULL) return 1;
    SSL_CTX_free(ctx);
    return 0;
}
EOF

build() {
    local name=$1
    shift
    if "$CC" -Os -flto -ffunction-sections -fdata-sections \
        "$WORK/tls-floor.c" -Wl,--gc-sections "$@" -lssl -lcrypto \
        -o "$WORK/$name" 2>"$WORK/$name.err"; then
        "$STRIP" "$WORK/$name"
        printf '%-10s %10s bytes' "$name" "$(wc -c <"$WORK/$name")"
        if command -v file >/dev/null; then
            printf '  '
            file "$WORK/$name"
        else
            printf '\n'
        fi
    else
        printf '%-10s indisponível: ' "$name"
        head -n 1 "$WORK/$name.err"
    fi
}

echo "Piso de tamanho: inicialização TLS, ainda sem túnel ou WebSocket."
build dynamic
build static -static
echo
echo "Bibliotecas dinâmicas do resultado dynamic:"
ldd "$WORK/dynamic" 2>/dev/null || true
