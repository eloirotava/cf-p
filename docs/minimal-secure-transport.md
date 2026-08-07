# Transporte criptográfico para o cliente C ultramínimo

## Recomendação

Para o perfil realmente mínimo, usar **Noise sobre TCP**, em uma porta própria,
com chave pública estática do servidor fixada no cliente e uma PSK individual
por cliente:

```text
Noise_NKpsk0_25519_ChaChaPoly_BLAKE2s
```

- `NK`: o cliente conhece previamente a chave pública estática do servidor;
- `psk0`: o token de 32 bytes também participa do handshake;
- `25519`: acordo de chaves X25519;
- `ChaChaPoly`: criptografia autenticada;
- `BLAKE2s`: hash e derivação de chaves.

O transporte pode escutar, por exemplo, em TCP/4443. Ele não é HTTPS e não deve
ser anunciado como tráfego web. WSS continua sendo o modo compatível com proxies;
QUIC continua sendo o modo UDP; Noise/TCP é o modo de tamanho mínimo.

## Por que este desenho

Fixar a chave do servidor remove DNS PKI, ASN.1, X.509, cadeia de CAs, OCSP,
negociação de cifras e o handshake HTTP/WebSocket. A PSK autentica qual cliente
está conectando sem enviar o token em claro. O handshake produz chaves de sessão
distintas e, após ele, os mesmos frames `cfp-protocol` podem ser carregados em
records autenticados.

Isso reduz muito código, mas não elimina criptografia: uma implementação segura
de X25519, ChaCha20-Poly1305, BLAKE2s, geração aleatória e a máquina de estados
Noise precisa estar dentro do executável. A meta plausível é dezenas ou poucas
centenas de KiB, dependendo da libc e da implementação escolhida — não há meta
honesta de poucos KiB para um binário seguro e autossuficiente.

## Formato externo ao handshake

Depois do handshake Noise, cada record deve ser:

```text
uint32_be ciphertext_length
ciphertext_and_tag
```

O plaintext é um único frame `cfp-protocol`. Limites obrigatórios:

- máximo de 64 KiB por record no perfil mínimo;
- contador de nonce monotônico e encerramento antes de overflow;
- fechamento imediato em tag inválida, tamanho inválido ou contador inesperado;
- rekey periódico conforme a implementação Noise escolhida;
- leitura e escrita completas, tratando interrupções e operações parciais;
- timeout de handshake, autenticação e inatividade;
- nenhuma compressão.

TCP garante ordem, portanto o contador não precisa ser transmitido. Nunca se deve
reutilizar uma chave/nonce e nunca se deve continuar após falha de autenticação.

## Provisionamento

O servidor terá uma chave estática Noise separada do certificado TLS. O painel
deve mostrar ao criar o cliente:

```text
CFP_SERVER=tcp://a.rotava.com:4443
CFP_TOKEN=CFPM1.<credencial-de-provisionamento>
```

Essa credencial contém a chave pública fixada e a PSK descritas abaixo. O cliente
deve recusar conexão se a chave apresentada não for exatamente a fixada. Rotação
exige aceitar temporariamente chave antiga e nova ou reprovisionar os clientes.
Usar apenas a PSK sem fixar a identidade do servidor aumenta o dano de vazamento
do token e não é o desenho recomendado.

### Uma única credencial para o usuário

A interface não precisa pedir duas variáveis. O painel pode empacotar tudo em um
único token de provisionamento Base64URL:

```text
CFPM1.<base64url(version || client_id || server_public_key || client_psk)>
```

Layout inicial:

| Campo | Tamanho | Secreto? |
|---|---:|---|
| version | 1 byte | não |
| client_id | 16 bytes | não |
| server_public_key | 32 bytes | não |
| client_psk | 32 bytes | sim |

O cliente recebe somente:

```text
CFP_SERVER=tcp://a.rotava.com:4443
CFP_TOKEN=CFPM1....
```

Internamente ele decodifica o token, usa `server_public_key` para autenticar a
VPS e `client_psk` no handshake. O `client_id` pode ser enviado antes do
handshake para o servidor localizar a PSK correta sem testar o segredo de todos
os clientes; ele é um identificador aleatório, não uma identidade pessoal.

Não se deve cortar uma única chave secreta ao meio nem derivar a chave privada do
servidor a partir do token. Se o mesmo segredo permitisse calcular os dois lados,
quem roubasse o token poderia também fingir ser o servidor. O token único é um
**envelope** com material público e secreto independentes, não uma chave dividida.

Como Noise com PSK precisa do segredo para executar o handshake, o servidor não
pode guardar somente `SHA-256(PSK)` como faz com o token WSS atual. Ele precisa
guardar a PSK (ou uma chave derivada definida como a própria PSK do protocolo),
idealmente protegida em repouso com uma chave mestra fora do YAML. Logs e a lista
do painel nunca devem exibir novamente o envelope completo.

## Implementação

Não implementar X25519, AEAD ou Noise manualmente. Para não ter dependências em
tempo de execução, incorporar ao build uma implementação C pequena, auditada e
com licença compatível, fixada em uma versão e testada com vetores oficiais do
Noise. “Sem dependências” deve significar **sem `.so`, runtime ou pacote externo
na máquina alvo**, e não “criptografia inventada no projeto”.

O CI deve executar:

- vetores oficiais do handshake escolhido;
- interoperabilidade cliente C ↔ servidor Rust;
- corrupção e truncamento de cada byte do handshake e dos records;
- replay, chave fixada incorreta, PSK incorreta e nonce fora de sequência;
- sanitizers, fuzzing do parser e limites de memória;
- medição do executável e listagem de dependências com `readelf`.

## Escolha operacional

| Modo | Transporte | Vantagem | Limitação |
|---|---|---|---|
| WSS | TCP/443 | atravessa proxies web | cliente e protocolo maiores |
| QUIC | UDP/443 | streams e datagramas eficientes | UDP pode ser bloqueado |
| Noise | TCP/4443 | cliente mais simples e pequeno | porta/protocolo identificável e bloqueável |

O modo Noise não substitui WSS. Ele é uma opção explícita para ambientes em que
o operador controla o firewall e prioriza tamanho do agente.

## Meta de tamanho

`100 KiB` é uma meta agressiva, não uma estimativa garantida. Antes de existir o
cliente C completo, o orçamento honesto para um Linux estático e stripado é:

| Componente | Orçamento inicial |
|---|---:|
| primitivas criptográficas | 35–90 KiB |
| estado Noise e records | 8–25 KiB |
| sockets, protocolo e `poll` | 10–35 KiB |
| startup, libc mínima e auxiliares | 20–100 KiB |
| **faixa total de engenharia** | **73–250 KiB** |

Esses valores são metas de engenharia, não medições do artefato final. LTO,
`-Os`, `--gc-sections`, uma biblioteca criptográfica configurada e a ausência de
DNS podem aproximar o binário de 100 KiB. Resolver hostnames, suportar IPv6,
embutir libc completa, mensagens de erro, UDP ou mais plataformas aumenta o
tamanho. Um build dinâmico pode produzir um arquivo menor sem reduzir o tamanho
real das bibliotecas exigidas na máquina.

A primeira meta de aceitação deve ser **menos de 256 KiB estático**, mantendo
todas as verificações. Depois se mede e otimiza para 128 KiB; `100 KiB` só vira
promessa quando CI publicar o tamanho real por arquitetura. Não se deve remover
autenticação, RNG seguro, checagem de tags ou limites para atingir um número.

## Relação com WireGuard

Sim, a construção é deliberadamente parecida: WireGuard também usa um handshake
baseado em Noise com Curve25519, ChaCha20-Poly1305, BLAKE2s e uma PSK opcional.
Isso não torna este modo uma implementação de WireGuard.

WireGuard cria uma interface IP de camada 3, encapsula pacotes IP sobre UDP,
administra peers por chaves públicas e inclui roaming, timers, replay protection
e mitigação de DoS próprios. O `cf-p` abre streams de aplicação a pedido do
servidor, mantém rotas por token e não cria TUN, tabela de rotas ou uma VPN da
rede inteira. O modo mínimo proposto roda Noise sobre TCP e reutiliza os frames
`OPEN`/`DATA`/`CLOSE` existentes.

Quando o operador controla as duas máquinas, pode liberar UDP e deseja uma VPN
de camada 3, usar WireGuard existente é preferível a reimplementar uma VPN. O
modo Noise do `cf-p` só se justifica quando se quer manter o modelo de publicação
por porta/domínio, não exigir interface TUN/root e produzir um agente específico
menor. Se depender do WireGuard já presente no kernel for aceitável, o programa
de controle pode ser minúsculo, mas a solução deixa de ser um executável único e
independente: passa a depender do kernel, configuração de rede e privilégios.
