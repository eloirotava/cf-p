# cf-p

Uma alternativa mínima ao `cloudflared`, composta por dois executáveis:

- `cfp-client`: binário pequeno que roda junto dos serviços privados;
- `cfp-server`: processo na VPS com IP público que publica portas e domínios.

## Instalação rápida na VPS

O projeto ainda é distribuído como código-fonte, portanto a VPS precisa do
toolchain Rust para gerar os dois executáveis. Em Debian ou Ubuntu, como `root`:

```bash
./scripts/install-rust.sh
source "$HOME/.cargo/env"
cargo build --release
```

O script instala `build-essential`, `ca-certificates`, `curl` e `pkg-config`,
instala o Rust estável pelo `rustup` com o perfil mínimo e disponibiliza
`cargo`. Não é necessário instalar `libssl-dev`, pois este projeto usa
`rustls`.

Depois do build, os programas ficam em:

```text
target/release/cfp-server
target/release/cfp-client
```

Se preferir executar os comandos manualmente:

```bash
apt-get update
apt-get install -y build-essential ca-certificates curl pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
cargo build --release
```

## Decisão de projeto

O transporte do MVP será **WebSocket binário sobre TLS (`wss://`) na porta
443**. O cliente inicia uma conexão de saída parecida com qualquer WebSocket
HTTPS, portanto atravessa NAT e a maioria dos proxies sem exigir portas ou
protocolos incomuns.

```text
Internet                     VPS                              rede privada
────────                     ───                              ────────────
browser ── HTTPS ─────┐
                      ├──> cfp-server :443
cfp-client ── WSS ────┘         │                                  ▲
                                └── streams no WebSocket ───────────┘
```

O endpoint do agente pode ser, por exemplo,
`wss://tunnel.exemplo.com/_cfp/connect`. O mesmo listener `:443` atende sites
normais e upgrades WebSocket, separando-os por hostname e caminho. Isso evita a
complexidade de usar ALPN próprio e faz o túnel parecer tráfego WebSocket
convencional para a rede intermediária.

WebSocket não torna o conteúdo invisível ao operador da VPS: TLS protege o
trecho cliente–VPS, mas o servidor termina essa conexão. Se a rota usar TLS
passthrough, o TLS da aplicação permanece ponta a ponta.

## Exemplo completo atrás de NAT

Sim, esse é exatamente o fluxo suportado. Supondo que a máquina privada rode
RDP em `127.0.0.1:3389`:

1. `cfp-client` abre **de dentro para fora** uma conexão persistente para
   `wss://tunnel.exemplo.com/_cfp/connect`, usando TCP **443** (não 433);
2. a VPS mantém essa sessão associada ao cliente autenticado `casa`;
3. `cfp-server` abre publicamente `0.0.0.0:33890`;
4. quando alguém conecta em `IP_DA_VPS:33890`, o servidor cria um novo
   `stream_id` e envia `OPEN` pela sessão WSS existente;
5. o cliente recebe `OPEN`, conecta localmente em `127.0.0.1:3389` e confirma
   com `OPEN_OK`;
6. servidor e cliente transportam os bytes do RDP em mensagens `DATA` até um
   dos lados fechar a conexão.

```text
mstsc ── TCP :33890 ──> VPS/cfp-server
                              ║
                              ║ stream multiplexado dentro de WSS/TCP :443
                              ║ conexão sempre iniciada pelo cliente
                              ▼
                         cfp-client ── TCP :3389 ──> RDP local
```

O roteador da rede privada não precisa de port forwarding: para ele existe
somente uma conexão TCP de saída para `:443`. Firewall e security group da VPS,
por outro lado, precisam liberar tanto `443/tcp` quanto cada porta pública, como
`33890/tcp`.

Para um site, o começo é semelhante: DNS aponta `app.exemplo.com` para a VPS e
o navegador conecta ao `:443` público. O servidor escolhe a rota pelo hostname
e carrega a requisição/resposta em outro `stream_id` da mesma sessão WSS até,
por exemplo, `127.0.0.1:3000` na máquina privada.

“Parecer navegação web normal” significa que o transporte usa handshake HTTPS,
TLS válido e upgrade WebSocket padrão. Isso costuma ser compatível com NAT e
proxies que aceitam WebSocket, mas não o torna indistinguível: firewalls com
inspeção podem identificar uma conexão WSS longa e bloqueá-la. O projeto não
deve tentar disfarçar ou burlar políticas de rede; deve usar WebSocket conforme
o padrão, com hostname e certificado legítimos.

## Linguagem e tamanho

**Go não é a escolha deste projeto**, pois mesmo um programa simples carrega um
runtime relativamente grande. Há duas opções adequadas:

### Opção recomendada: Rust nos dois lados

Rust oferece segurança de memória sem garbage collector e mantém uma única
base de protocolo. O servidor pode usar um runtime assíncrono; o cliente deve
evitar frameworks HTTP completos e implementar somente o necessário para o
handshake e os frames WebSocket sobre uma biblioteca TLS pequena.

Perfil de release sugerido:

```toml
[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
strip = true
```

Também devem ser desabilitadas features padrão não utilizadas, e o tamanho real
do cliente deve ser verificado em CI. `musl` facilita distribuição estática,
mas não garante por si só o menor binário; é necessário comparar os artefatos
por plataforma.

### Menor cliente possível: C no cliente, Rust no servidor

Se cada kilobyte for prioritário, o cliente pode ser C, com uma biblioteca TLS
compacta e uma implementação WebSocket mínima e auditável. O servidor pode
continuar em Rust, onde tamanho é irrelevante. O custo é manter duas
implementações do protocolo e assumir riscos maiores de segurança de memória no
componente instalado em redes privadas.

Não se deve implementar TLS. A comparação de tamanho precisa incluir o binário
e a biblioteca TLS efetivamente distribuídos; um executável C aparentemente
pequeno que depende de várias bibliotecas dinâmicas não é um cliente menor na
prática.

## Sessão WebSocket

O cliente abre uma sessão WSS persistente, autentica-se, envia ping
periodicamente e reconecta com backoff exponencial e jitter. O tráfego da sessão
usa somente **mensagens WebSocket binárias**.

WebSocket já delimita mensagens, então não é necessário duplicar um campo de
tamanho externo. Cada mensagem começa com:

```text
+---------+------+-----------+------------------+
| version | type | stream_id | payload          |
| 1 byte  | 1 B  | 4 B (BE)  | restante da msg |
+---------+------+-----------+------------------+
```

Tipos mínimos:

1. `AUTH`: token, versão e identificador do cliente;
2. `AUTH_OK` ou `ERROR`;
3. `OPEN`: solicita conexão a um destino local autorizado;
4. `OPEN_OK` ou `OPEN_ERROR`;
5. `DATA`: bytes associados ao `stream_id`;
6. `WINDOW_UPDATE`: backpressure por stream;
7. `CLOSE`: half-close ou encerramento do stream;
8. `PING` e `PONG`: heartbeat da aplicação, além do ping WebSocket quando
   necessário.

Uma única sessão multiplexa todas as conexões. Isso é simples, mas todos os
streams compartilham a mesma conexão TCP e sofrem head-of-line blocking quando
há perda. Para o MVP essa troca é aceitável; se medições mostrarem problema, o
cliente poderá abrir um pequeno pool de WebSockets, distribuindo streams entre
eles sem mudar o protocolo.

## Autenticação

O agente envia o token no primeiro frame `AUTH`, nunca na URL. Colocar o token
na query string facilita vazamento em access logs, históricos e métricas. O
upgrade WebSocket só é considerado autenticado depois da resposta `AUTH_OK`.

Tokens devem ser aleatórios, individuais por cliente, revogáveis e armazenados
no servidor apenas como hash. O cliente lê o segredo de arquivo protegido ou de
variável de ambiente; argumento de linha de comando é permitido apenas como
atalho explícito, pois pode aparecer na lista de processos e no histórico do
shell.

## Configuração

O servidor é a fonte de verdade das rotas. Neste MVP o token seleciona as rotas
e destinos definidos na VPS; uma allowlist local no cliente será adicionada
antes de considerar o agente pronto para ambientes não controlados.

`server.yaml`:

```yaml
listen: ":443"
cert: "/etc/letsencrypt/live/tunnel.exemplo.com/fullchain.pem"
key: "/etc/letsencrypt/live/tunnel.exemplo.com/privkey.pem"

clients:
  - token_sha256: "SHA256_HEXADECIMAL_DO_TOKEN"
    routes:
      - listen: "0.0.0.0:33890"
        target: "127.0.0.1:3389"
```

Nesta primeira versão, as rotas pertencem ao hash do token. Assim, tokens
diferentes ativam conjuntos diferentes de portas. O token em texto puro nunca
precisa ficar no arquivo da VPS.

**`token_sha256` não recebe o token em texto puro.** O valor `SEU_TOKEN` dos
exemplos é apenas um placeholder. Gere um par pronto com:

```bash
./scripts/generate-token.sh
```

O script imprime dois valores diferentes: copie o **hash** para
`token_sha256` no `server.yaml`, reinicie o servidor e use o **token original**
em `CFP_TOKEN` no cliente. Não use as aspas ou os títulos impressos como parte
dos valores.

Execução:

```bash
cp server.example.yaml server.yaml
# edite certificado, chave e token_sha256
cargo build --release
sudo ./target/release/cfp-server --config server.yaml
CFP_SERVER=wss://tunnel.exemplo.com CFP_TOKEN="$TOKEN" \
  ./target/release/cfp-client
```

O servidor escuta TLS/WSS em `:443`; o cliente não escuta essa porta, apenas
inicia a conexão de saída até ela. No cliente, os argumentos `--server` e
`--token` são equivalentes às variáveis de ambiente mostradas.

### Certificado para `a.rotava.com`

#### Com Caddy já instalado (recomendado)

Sim: Caddy pode continuar atendendo todos os sites em `:443` e terminar o TLS
do túnel. Reserve `a.rotava.com` para o agente e adicione ao `Caddyfile`:

```caddyfile
a.rotava.com {
    reverse_proxy 127.0.0.1:444
}
```

Caddy encaminha o upgrade WebSocket automaticamente. Nesse cenário,
`cfp-server` recebe WebSocket sem TLS **somente no loopback**:

```yaml
listen: "127.0.0.1:444"

clients:
  - token_sha256: "SHA256_HEXADECIMAL_DO_TOKEN"
    routes:
      - listen: "0.0.0.0:33890"
        target: "127.0.0.1:3389"
```

O cliente continua usando
`CFP_SERVER=wss://a.rotava.com CFP_TOKEN="$TOKEN"`: externamente tudo passa por
TLS/443; apenas o trecho local Caddy → `cfp-server` usa `ws://127.0.0.1:444`.
Não publique a porta `444` em `0.0.0.0` nem no firewall.

Outros blocos do Caddy continuam servindo normalmente seus próprios domínios:

```caddyfile
rotava.com {
    reverse_proxy 127.0.0.1:8080
}

a.rotava.com {
    reverse_proxy 127.0.0.1:444
}
```

Isso permite usar sites e o túnel simultaneamente no mesmo IP e na mesma porta
pública `443`, porque Caddy seleciona o upstream pelo hostname TLS/HTTP. O
encaminhamento público `33890 → cliente:3389` permanece separado e continua
funcionando.

#### Sem Caddy

Primeiro, crie no DNS um registro `A` para `a.rotava.com` apontando para o IPv4
da VPS (e um `AAAA` somente se a VPS aceitar IPv6). Para emitir com Let's
Encrypt, instale o `certbot`, libere temporariamente `80/tcp` e execute:

```bash
sudo EMAIL=admin@rotava.com \
  ./scripts/setup-certificate.sh letsencrypt
```

O script usa `a.rotava.com` como padrão e grava os arquivos em
`/etc/letsencrypt/live/a.rotava.com/`. É possível trocar o nome com
`DOMAIN=outro.rotava.com`. Para testar a emissão sem consumir limites, use
`STAGING=1`.

Para desenvolvimento local, sem DNS público:

```bash
sudo ./scripts/setup-certificate.sh self-signed
```

Esse modo grava em `/etc/cfp/tls` por padrão. Um certificado autoassinado não é
aceito automaticamente pelo cliente: o certificado precisa ser instalado como
confiável na máquina cliente. Por isso, para a VPS pública, Let's Encrypt é o
caminho recomendado.

> O MVP implementado encaminha portas TCP. O reverse proxy HTTP por hostname,
> heartbeat, `WINDOW_UPDATE`, validação de path do WebSocket e hot reload são os
> próximos passos; ainda não devem ser considerados disponíveis.

### Teste ponta a ponta com uma terceira máquina

As mensagens `tunel autenticado` no cliente e `rota ativa` no servidor já
confirmam WSS, token e abertura de `33890`. Para comprovar que bytes atravessam
o túnel sem depender de RDP, faça este teste temporário.

Na máquina privada, pare qualquer serviço que já esteja usando `3389` e, em
outro terminal, suba um servidor HTTP de teste:

```bash
python3 -m http.server 3389 --bind 127.0.0.1
```

Mantenha o `cfp-client` conectado. Na terceira máquina, execute:

```bash
curl -v --max-time 10 http://IP_PUBLICO_DA_VPS:33890/
```

Uma resposta `HTTP/1.0 200 OK` com a listagem do diretório comprova o caminho
completo:

```text
terceira máquina → VPS:33890 → WSS/Caddy:443 → cfp-client → localhost:3389
```

Também é possível fazer apenas o teste de abertura da porta:

```bash
nc -vz -w 5 IP_PUBLICO_DA_VPS 33890
```

Esse teste do `nc` é mais fraco: ele comprova que a VPS aceitou TCP, mas o
`curl` comprova que requisição e resposta realmente atravessaram o túnel. Ao
terminar, encerre o `python3` com `Ctrl+C` e volte a iniciar o serviço RDP real.

Os avisos `No "Connection: upgrade" header` indicam que algum navegador,
monitor ou scanner fez HTTP comum em `a.rotava.com`, sem solicitar WebSocket.
Eles não significam queda do cliente que continua mostrando `tunel
autenticado`.

## Encaminhamento

### Portas TCP

Para cada conexão em uma porta pública, o servidor aloca um `stream_id` e envia
`OPEN`. O cliente conecta ao serviço local e ambos passam a trocar `DATA`, com
janela limitada, timeouts e half-close. Isso atende SSH, bancos e protocolos TCP
genéricos.

### Domínios HTTP/HTTPS

Na próxima etapa, o servidor terminará TLS, escolherá a rota pelo `Host` e
encaminhará HTTP pelo túnel. Isso permitirá servir vários domínios no mesmo
`:443`, automatizar certificados e adicionar `X-Forwarded-For` de forma
controlada. Essa parte ainda não está implementada no MVP atual.

TLS passthrough por SNI pode vir depois. Nesse modo, o servidor inspeciona o
ClientHello somente para escolher a rota e encaminha os bytes intactos; o
serviço privado fica responsável pelo certificado. Não há roteamento por path,
alteração de headers ou fallback confiável para clientes sem SNI.

## Controles obrigatórios

- validar `Origin`, `Host`, path e método do upgrade WebSocket;
- limitar tamanho de mensagem, streams, janela, fila, bytes e taxa por cliente;
- impor timeouts de upgrade, autenticação, conexão local, escrita e ociosidade;
- rejeitar frames WebSocket de texto e mensagens de protocolo malformadas;
- usar TLS moderno, verificar certificado e nome no cliente e nunca oferecer
  modo `--insecure` silencioso;
- não registrar token, query string sensível nem conteúdo encaminhado;
- permitir rotação e revogação de credenciais sem reiniciar o servidor;
- executar sem root e conceder acesso a portas privilegiadas somente ao
  listener frontal;
- expor métricas de sessões, reconexões, streams, bytes, filas e erros;
- realizar shutdown gracioso e validar toda a configuração antes de aplicá-la.

## Escopo do MVP

1. workspace Rust com `cfp-client`, `cfp-server` e crate de protocolo;
2. WSS, autenticação, heartbeat e reconexão;
3. multiplexação com backpressure e encaminhamento TCP;
4. reverse proxy HTTP/HTTPS por hostname;
5. limites, logs estruturados e métricas básicas;
6. medição automática do tamanho do cliente em release;
7. depois: cliente C, pool de WebSockets, hot reload, TLS passthrough e UDP.

```text
crates/cfp-client/     agente mínimo
crates/cfp-server/     listener público e roteamento
crates/cfp-protocol/   tipos e codec sem dependência do runtime do servidor
```

O orçamento de tamanho deve ser definido antes da implementação, separando
binário dinâmico, binário estático e pacote comprimido. Sem essa definição,
“pequeno” não é uma meta testável.
