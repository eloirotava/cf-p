# cf-p

Uma alternativa mínima ao `cloudflared`, composta por dois executáveis:

- `cfp-client`: binário pequeno que roda junto dos serviços privados;
- `cfp-server`: processo na VPS com IP público que publica portas e domínios.

Um segundo cliente mínimo em C está especificado em
[`docs/c-client.md`](docs/c-client.md). O documento separa corretamente um
executável pequeno com bibliotecas dinâmicas de um pacote realmente estático e
autossuficiente, e inclui um script para medir o piso de tamanho TLS no host.
O perfil criptográfico sem TLS/WSS proposto usa Noise sobre TCP, chave pública
fixada e PSK individual; veja
[`docs/minimal-secure-transport.md`](docs/minimal-secure-transport.md).
O desenho é aparentado ao handshake do WireGuard, mas mantém encaminhamento de
aplicação em vez de criar uma VPN/IP; a comparação e o critério para simplesmente
usar WireGuard estão documentados no mesmo arquivo.

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

### Binários pelo GitHub Actions

O workflow `Build release binaries` compila e publica artefatos para:

- Linux AMD64 estático (`x86_64-unknown-linux-musl`);
- Linux ARM64 estático (`aarch64-unknown-linux-musl`);
- Linux ARMv7 armhf estático (`armv7-unknown-linux-musleabihf`);
- Windows x64 (`x86_64-pc-windows-gnu`).

Em pushes e pull requests, os pacotes ficam na seção **Actions → Artifacts** por
14 dias. Cada pacote inclui `cfp-client`, `cfp-server`, README, configuração de
exemplo e um arquivo `.sha256` para verificação.

Para criar uma GitHub Release com todos os pacotes:

```bash
git tag v0.1.0
git push origin v0.1.0
```

Tags começando com `v` publicam automaticamente os `.tar.gz` de Linux, o `.zip`
de Windows e seus checksums na página de releases.

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

### Painel web e recarga automática

O servidor pode editar o próprio `server.yaml` por uma interface protegida com
HTTP Basic. Configure credenciais exclusivas e uma porta presa ao loopback:

```yaml
admin:
  listen: "127.0.0.1:446"
  public_url: "wss://a.rotava.com"
  username: "admin"
  password: "UMA_SENHA_LONGA_E_ALEATORIA"
```

Para servir o painel e o endpoint do agente no mesmo `a.rotava.com`, o Caddy
separa upgrades WebSocket das requisições normais:

```caddyfile
a.rotava.com {
    @tunnel {
        header Connection *Upgrade*
        header Upgrade websocket
    }
    reverse_proxy @tunnel 127.0.0.1:444
    reverse_proxy 127.0.0.1:446
}
```

Depois de `caddy validate --config /etc/caddy/Caddyfile` e
`systemctl reload caddy`, abra `https://a.rotava.com`. O painel permite criar e
excluir clientes e rotas. Ao criar um cliente, ele gera um token aleatório,
grava somente seu SHA-256 e mostra uma única vez comandos prontos para Linux,
macOS, Windows PowerShell e Windows CMD. O valor de `public_url` é usado nesses
comandos e deve ser o endereço WSS público do agente.

Cada alteração é validada, escrita de forma atômica no YAML e aplicada sem
reiniciar o processo. As sessões existentes são encerradas de propósito; os
clientes reconectam automaticamente e passam a usar as novas rotas. A senha do
painel fica no YAML, portanto proteja o arquivo (`chmod 600 server.yaml`) e não
publique a porta `446` diretamente na internet. Remover todo o bloco `admin`
desativa a interface.

O túnel não transforma automaticamente qualquer porta da rede privada em uma
porta pública. Ele encaminha somente as rotas TCP cadastradas para aquele token,
e o processo cliente ainda precisa ter permissão e conectividade até o `target`.
Rotas UDP usam o prefixo `udp://` nos dois lados. Como o cliente atualmente
confia nos destinos enviados pelo servidor, comprometer a VPS ou o painel pode
dar acesso TCP ou UDP com os mesmos
privilégios de rede do processo cliente. Execute-o com usuário restrito e
proteja rigorosamente a administração; uma allowlist local no agente continua
planejada antes de uso em redes não controladas.

WSS em `443/tcp` costuma atravessar NAT porque é uma conexão iniciada de dentro
para fora, mas não é impossível de bloquear. Uma rede pode negar o domínio ou
IP da VPS, filtrar DNS/SNI, permitir saída apenas por proxy autenticado, rejeitar
o upgrade WebSocket, limitar conexões longas ou usar inspeção TLS em dispositivos
administrados. Portanto, o projeto oferece transporte web compatível; não é uma
garantia de contornar firewalls nem deve ser usado para contrariar políticas da
rede.

### Encaminhamento UDP

UDP pode ser publicado explicitamente, embora continue viajando dentro do WSS
sobre TCP:

```yaml
- listen: "udp://0.0.0.0:5353"
  target: "udp://127.0.0.1:53"
```

Cada origem UDP pública recebe um stream lógico, e cada datagrama ocupa um frame
`DATA`, preservando seus limites. Associações ociosas expiram depois de 60
segundos e cada rota aceita no máximo 1024 origens simultâneas. A porta UDP deve
ser liberada no firewall da VPS.

Isso é útil para DNS, syslog, telemetria e testes simples, mas não reproduz as
características naturais do UDP: perda de um segmento TCP bloqueia todos os
datagramas posteriores (head-of-line blocking), retransmissões aumentam latência
e todos os fluxos dividem a mesma conexão. Não é recomendado para jogos, mídia
em tempo real, QUIC ou cargas sensíveis a jitter. Também não é UDP transparente
em nível IP: o serviço privado enxerga como origem o socket criado pelo
`cfp-client`, não o endereço original da internet.

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

É possível colocar quantas rotas forem necessárias no mesmo token:

```yaml
clients:
  - token_sha256: "HASH_DO_TOKEN"
    routes:
      - listen: "0.0.0.0:33890"
        target: "127.0.0.1:3389"
      - listen: "0.0.0.0:2222"
        target: "127.0.0.1:22"
      - listen: "b.rotava.com"
        target: "127.0.0.1:3000"
```

Cada `listen` precisa ser único na VPS inteira, inclusive entre tokens
diferentes: sockets não podem repetir endereço/porta e hostnames não podem ter
duas sessões responsáveis ao mesmo tempo. Um mesmo `target` pode ser reutilizado
por várias rotas. O cliente não repete essa configuração: o servidor seleciona
as rotas pelo token e envia cada `target` no frame `OPEN`.

### Vários clientes

Sim: cada item de `clients` representa uma máquina/agente diferente. Cada um
usa seu próprio token original no `CFP_TOKEN` e possui seu próprio hash e suas
próprias rotas no servidor:

```yaml
clients:
  - token_sha256: "HASH_DO_TOKEN_A"
    routes:
      - listen: "0.0.0.0:33890"
        target: "127.0.0.1:3389"
      - listen: "casa.rotava.com"
        target: "127.0.0.1:3000"

  - token_sha256: "HASH_DO_TOKEN_B"
    routes:
      - listen: "0.0.0.0:33891"
        target: "127.0.0.1:3389"
      - listen: "escritorio.rotava.com"
        target: "127.0.0.1:3000"
```

Nos dois clientes o `target` é `127.0.0.1:3389`, mas cada endereço se refere à
própria máquina onde aquele `cfp-client` está rodando. Na máquina A, execute com
o token A; na máquina B, com o token B.

Tokens e valores de `listen` não podem se repetir. O servidor agora valida isso
ao iniciar e recusa configurações ambíguas antes de abrir qualquer porta. Uma
mesma credencial também não deve ser usada simultaneamente por duas instâncias
do cliente, porque ambas representariam a mesma identidade e disputariam as
mesmas rotas.

### Domínios HTTP/HTTPS

O servidor também roteia HTTP diretamente pelo hostname, sem criar uma porta
intermediária por site. Para publicar a aplicação privada
`127.0.0.1:3000` como `b.rotava.com`, configure no `server.yaml`:

```yaml
clients:
  - token_sha256: "HASH_DO_TOKEN"
    routes:
      - listen: "b.rotava.com"
        target: "127.0.0.1:3000"
```

No topo do mesmo arquivo, `http_listen` é o único upstream local usado por todos
os domínios:

```yaml
listen: "127.0.0.1:444"
http_listen: "127.0.0.1:445"
```

No DNS, `b.rotava.com` deve apontar para o IP da VPS. No `Caddyfile`:

```caddyfile
# Conexão WSS persistente do cfp-client.
a.rotava.com {
    reverse_proxy 127.0.0.1:444
}

# Todos os sites publicados podem compartilhar o roteador HTTP do cfp-server.
b.rotava.com {
    reverse_proxy 127.0.0.1:445
}
```

O fluxo fica:

```text
navegador → HTTPS/443 → Caddy → cfp-server:445
                                      │ Host: b.rotava.com
                                      ↓
                                WSS → cfp-client → 127.0.0.1:3000
```

Caddy cuida do certificado e HTTPS. O `cfp-server` lê `Host`, encontra a rota
registrada pela sessão autenticada e encaminha a conexão HTTP para o `target`.
A porta `445` fica em loopback e não deve ser liberada no firewall. Vários
domínios podem apontar para a mesma `445`; cada um ainda precisa de seu bloco no
Caddy (ou de um wildcard) e de sua rota no `server.yaml`.

Rotas por domínio são HTTP neste MVP. Portanto, `listen: "b.rotava.com"` com
`target: "127.0.0.1:3389"` não transforma RDP em site: um navegador enviaria
HTTP para um serviço RDP. Para RDP e outros protocolos TCP, continue usando uma
porta pública como `0.0.0.0:33890`. TLS passthrough por SNI será uma
funcionalidade separada.

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

## Estado do MVP

Já implementado: workspace Rust, WSS, autenticação, reconexão, multiplexação e
encaminhamento TCP. Domínios funcionam com Caddy e uma rota TCP local por
upstream, como descrito acima.

Próximos passos: heartbeat, controle de fluxo explícito, limites, métricas,
roteamento HTTP nativo por hostname, hot reload, TLS passthrough e UDP.

```text
crates/cfp-client/     agente mínimo
crates/cfp-server/     listener público e roteamento
crates/cfp-protocol/   tipos e codec sem dependência do runtime do servidor
```

O orçamento de tamanho deve ser definido antes da implementação, separando
binário dinâmico, binário estático e pacote comprimido. Sem essa definição,
“pequeno” não é uma meta testável.
