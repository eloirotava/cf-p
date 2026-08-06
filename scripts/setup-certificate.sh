#!/usr/bin/env bash
set -Eeuo pipefail

MODE="${1:-}"
DOMAIN="${DOMAIN:-a.rotava.com}"
CERT_DIR="${CERT_DIR:-/etc/cfp/tls}"
DAYS="${DAYS:-365}"

usage() {
  cat <<'EOF'
Uso:
  sudo ./scripts/setup-certificate.sh self-signed
  sudo EMAIL=voce@rotava.com ./scripts/setup-certificate.sh letsencrypt

Variáveis:
  DOMAIN=a.rotava.com   domínio do certificado (padrão: a.rotava.com)
  CERT_DIR=/etc/cfp/tls destino no modo self-signed
  DAYS=365              validade no modo self-signed
  EMAIL=...             obrigatório no modo letsencrypt
  STAGING=1             usa o ambiente de testes do Let's Encrypt
EOF
}

die() {
  printf 'erro: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "comando '$1' não encontrado"
}

[[ "$MODE" == "self-signed" || "$MODE" == "letsencrypt" ]] || {
  usage
  exit 2
}
[[ $EUID -eq 0 ]] || die "execute com sudo (os certificados são gravados em /etc)"
[[ "$DOMAIN" =~ ^[A-Za-z0-9.-]+$ ]] || die "DOMAIN inválido"

if [[ "$MODE" == "self-signed" ]]; then
  require_command openssl
  install -d -m 0750 "$CERT_DIR"
  umask 077
  openssl req -x509 -newkey rsa:2048 -sha256 -nodes \
    -days "$DAYS" \
    -keyout "$CERT_DIR/privkey.pem" \
    -out "$CERT_DIR/fullchain.pem" \
    -subj "/CN=$DOMAIN" \
    -addext "subjectAltName=DNS:$DOMAIN" \
    -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
    -addext "extendedKeyUsage=serverAuth"
  chmod 0600 "$CERT_DIR/privkey.pem"
  chmod 0644 "$CERT_DIR/fullchain.pem"

  cat <<EOF

Certificado autoassinado criado.
cert: $CERT_DIR/fullchain.pem
key:  $CERT_DIR/privkey.pem

Aviso: o cfp-client usa a cadeia pública de confiança e recusará este
certificado por padrão. Use o modo letsencrypt para funcionamento imediato;
autoassinado é destinado a desenvolvimento e exige instalar a CA/certificado
na máquina cliente.
EOF
else
  require_command certbot
  [[ -n "${EMAIL:-}" ]] || die "defina EMAIL, por exemplo: sudo EMAIL=admin@rotava.com $0 letsencrypt"

  args=(
    certonly --standalone --preferred-challenges http
    --non-interactive --agree-tos --keep-until-expiring
    --email "$EMAIL" --domain "$DOMAIN"
  )
  [[ "${STAGING:-0}" == "1" ]] && args+=(--staging)

  cat <<EOF
Antes da validação, confirme que:
  - $DOMAIN possui registro A/AAAA apontando para esta VPS;
  - a porta 80/TCP está liberada e não está sendo usada por outro processo.
EOF
  certbot "${args[@]}"

  live_dir="/etc/letsencrypt/live/$DOMAIN"
  [[ -r "$live_dir/fullchain.pem" && -r "$live_dir/privkey.pem" ]] || \
    die "certbot terminou, mas os arquivos esperados não foram encontrados"

  cat <<EOF

Certificado Let's Encrypt disponível.
cert: $live_dir/fullchain.pem
key:  $live_dir/privkey.pem

Use esses caminhos no server.yaml. A instalação do certbot normalmente cria
renovação automática; verifique com:
  systemctl list-timers | grep certbot
  certbot renew --dry-run

Após uma renovação, reinicie o cfp-server para ele reler os arquivos.
EOF
fi
