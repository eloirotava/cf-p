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
CFP_SERVER_KEY=<chave-publica-fixada>
CFP_TOKEN=<psk-individual-de-32-bytes>
```

O cliente deve recusar conexão se a chave apresentada não for exatamente a
fixada. Rotação exige aceitar temporariamente chave antiga e nova ou reprovisionar
os clientes. Usar apenas a PSK sem fixar a identidade do servidor aumenta o dano
de vazamento do token e não é o desenho recomendado.

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
