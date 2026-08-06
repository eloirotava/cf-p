#!/usr/bin/env bash
set -Eeuo pipefail

if command -v apt-get >/dev/null 2>&1; then
  apt-get update
  DEBIAN_FRONTEND=noninteractive apt-get install -y \
    build-essential ca-certificates curl pkg-config
else
  printf 'erro: este script suporta Debian/Ubuntu (apt-get).\n' >&2
  printf 'instale rustup manualmente em https://rustup.rs/\n' >&2
  exit 1
fi

if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal
fi

# rustup modifica o PATH apenas em shells novos; carregue-o nesta execução.
# shellcheck disable=SC1090
source "${CARGO_HOME:-$HOME/.cargo}/env"
rustup toolchain install stable --profile minimal
rustup default stable

printf '\nInstalação concluída:\n'
rustc --version
cargo --version
printf '\nAgora execute: cargo build --release\n'
