# Transporte QUIC (branch experimental)

Esta branch reserva o trabalho de QUIC fora da implementação WSS estável. QUIC
é um transporte criptografado sobre UDP; ele não é simplesmente “UDP cru” e não
é o mesmo recurso que encaminhar datagramas UDP dentro do túnel WSS.

## Objetivo

Oferecer dois transportes intercambiáveis:

```text
cfp-client --transport wss   -> WSS sobre TCP/443 (compatibilidade)
cfp-client --transport quic  -> QUIC sobre UDP/443 ou UDP/4443 (desempenho)
```

As rotas TCP, UDP e HTTP devem continuar com a mesma configuração. O transporte
é somente o caminho entre `cfp-client` e `cfp-server`.

## Por que vale a pena

- streams QUIC independentes evitam o head-of-line blocking entre conexões;
- uma perda em um stream não paralisa todos os outros streams;
- datagramas QUIC podem carregar rotas UDP sem forçar retransmissão TCP;
- TLS 1.3 e multiplexação fazem parte do protocolo QUIC;
- troca de rede pode ser suportada posteriormente por connection migration.

Para rotas TCP, cada conexão pública deve usar um stream bidirecional QUIC. Para
rotas UDP, a opção preferida é QUIC DATAGRAM. O protocolo precisa incluir no
datagrama o identificador da rota e da associação de origem, pois datagramas
QUIC podem ser perdidos, duplicados ou chegar fora de ordem.

## Limitações e implantação

QUIC exige saída UDP. Redes que liberam apenas TCP/443 podem bloquear QUIC sem
afetar HTTPS tradicional, então WSS continua necessário como fallback. O modo
`auto` deverá tentar QUIC por poucos segundos e então usar WSS.

O Caddy atualmente ocupa TCP/443 e pode também ocupar UDP/443 para HTTP/3. Há
duas implantações seguras:

1. iniciar com `cfp-server` em UDP/4443 e liberar essa porta; ou
2. dedicar UDP/443 ao `cfp-server` e desativar HTTP/3 no Caddy.

TCP/443 do Caddy e UDP/443 do túnel são sockets diferentes e podem coexistir,
desde que o Caddy não esteja usando UDP/443. O Caddy não faz reverse proxy do
QUIC proprietário deste projeto como faz com WebSocket; o listener QUIC termina
diretamente no `cfp-server`, que precisa de certificado e chave.

## Segurança

- autenticar o token dentro do primeiro stream de controle, depois do handshake;
- validar o certificado e o hostname no cliente, sem opção insegura padrão;
- negociar uma versão de protocolo via ALPN, por exemplo `cfp/1`;
- limitar streams, datagramas, peers UDP, tamanho de payload e handshakes por IP;
- impedir 0-RTT para autenticação e operações que abrem rotas, evitando replay;
- manter as mesmas validações e allowlists planejadas para WSS.

## Plano de implementação

1. Extrair a sessão e o roteamento atuais de `main.rs` para uma interface de
   transporte independente.
2. Manter WSS como implementação de referência e fallback.
3. Adicionar listener QUIC com ALPN `cfp/1`, autenticação e um stream de controle.
4. Mapear conexões TCP para streams bidirecionais QUIC independentes.
5. Mapear UDP para QUIC DATAGRAM, com fallback opcional para streams QUIC quando
   datagramas não forem negociados.
6. Adicionar `transport: wss`, `quic` e `auto` ao cliente, preservando o comando
   atual como `wss` por compatibilidade.
7. Testar perda, reordenação, MTU, reconexão, fallback e limites antes de chamar
   o transporte de estável.

Não se deve encaixar QUIC diretamente no loop de frames WebSocket atual: isso
manteria toda a sessão em um único stream confiável e perderia justamente os
benefícios de streams independentes e datagramas.
