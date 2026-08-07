# Segundo cliente mínimo em C

Um cliente C faz sentido para appliances, initramfs e máquinas antigas, mas há
dois objetivos diferentes:

1. **arquivo executável pequeno**, usando TLS, DNS e libc dinâmicos do sistema;
2. **arquivo autossuficiente estático**, que precisa carregar essas bibliotecas.

O primeiro pode ficar em dezenas de KiB depois de `-Os`, LTO e `strip`. O segundo
normalmente não ficará em “alguns KiB”: autenticação de certificados, TLS 1.3,
cifras, hashing, entropia, sockets e WebSocket precisam existir em algum lugar.
Por exemplo, no ambiente de desenvolvimento atual, somente os arquivos estáticos
do OpenSSL ocupam aproximadamente 1,2 MiB (`libssl.a`) e 9,6 MiB
(`libcrypto.a`) antes de o linker remover seções não usadas.

## Perfis propostos

### `cfp-client-c-dynamic`

- Linux/POSIX;
- libc e OpenSSL do sistema;
- WSS/TCP apenas;
- validação obrigatória do certificado e hostname;
- token por variável de ambiente ou arquivo;
- `AUTH`, `OPEN`, `OPEN_OK`, `OPEN_ERROR`, `DATA` e `CLOSE`;
- múltiplos sockets com `poll(2)`;
- sem YAML, painel, HTTP router ou servidor;
- provável menor arquivo, mas não é portátil nem autossuficiente.

### `cfp-client-c-static`

- musl;
- biblioteca TLS configurada com somente TLS cliente, TLS 1.3, roots e cifras
  necessárias;
- bundle de CAs precisa ser externo ou embutido, e seu tamanho deve ser contado;
- mesmo subconjunto de protocolo do perfil dinâmico;
- autossuficiente, mas a meta realista deve ser medida em centenas de KiB ou
  poucos MiB, não em poucos KiB.

## Requisitos que não podem ser removidos

- conferir a cadeia do certificado contra uma CA confiável;
- conferir que o certificado pertence ao hostname solicitado;
- obter entropia criptográfica do sistema;
- usar máscara aleatória em todos os frames WebSocket enviados pelo cliente;
- validar status `101`, `Upgrade`, `Connection` e `Sec-WebSocket-Accept`;
- impor limite de frame e de conexões;
- tratar leituras e escritas parciais;
- nunca oferecer uma opção padrão que desative TLS para economizar espaço.

Um binário de poucos KiB só seria plausível terceirizando quase tudo para
bibliotecas dinâmicas já instaladas, removendo validações de segurança ou usando
outro processo como TLS/WebSocket. Nesse caso o arquivo parece pequeno, mas o
cliente implantado como um todo não é.

## Compatibilidade com o servidor

O cliente C deve falar exatamente o protocolo de `cfp-protocol`; não precisa de
uma configuração própria. O servidor continua enviando o destino no frame
`OPEN`. A primeira versão deve implementar TCP sobre WSS. UDP e QUIC ficam para
etapas posteriores, porque aumentariam consideravelmente o código e a superfície
de teste do agente mínimo.

Antes de escrever a implementação, o codec de frames e vetores de teste devem
ser extraídos para casos independentes de Rust. CI deverá executar o cliente C
contra o servidor Rust e publicar separadamente tamanho do arquivo, dependências
dinâmicas (`readelf -d`) e tamanho total do pacote estático.

Use `scripts/measure-c-tls-floor.sh` para medir no host a diferença entre um
executável C mínimo ligado dinamicamente e estaticamente à biblioteca TLS. Essa
medição ainda é um piso: não contém handshake, WebSocket nem multiplexação.

Para um perfil ainda menor sem X.509, TLS ou WebSocket, a recomendação é um
transporte Noise/TCP com chave do servidor fixada e PSK por cliente. O desenho e
seus requisitos de segurança estão em
[`minimal-secure-transport.md`](minimal-secure-transport.md).
Para manter a mesma experiência dos outros clientes, a chave pública e a PSK
podem vir empacotadas em uma única credencial `CFPM1...`; o agente as separa
internamente, sem pedir duas chaves ao operador.
O orçamento inicial do perfil Noise estático é de 73–250 KiB, com meta de
aceitação abaixo de 256 KiB. Aproximar-se de 100 KiB é plausível, porém somente a
medição do cliente completo poderá confirmar isso por arquitetura.

## Quando não construir este cliente

O número de referência de 1,9 MB vem de um cliente ARMHF próprio, escrito em Rust
para conversar com a infraestrutura da Cloudflare; não é o `cloudflared` oficial
em Go, que pertence a outra implementação e tem outro orçamento de tamanho. Um
agente próprio e funcional nessa faixa já é pequeno para quase toda VPS, SBC,
roteador Linux ou cartão SD. Trocar 1,9 MB por 100–250 KiB economiza menos de 2 MB
de armazenamento, mas cria outro transporte criptográfico, implementação C,
matriz de builds, atualizações e responsabilidade de segurança.

O cliente C/Noise só deve avançar se houver um requisito mensurável, por exemplo:

- firmware ou initramfs com orçamento rígido abaixo de 1 MB;
- milhares de dispositivos e atualização por enlace muito estreito;
- flash realmente limitada;
- plataforma sem Rust e sem bibliotecas dinâmicas adequadas;
- tempo de inicialização ou memória medidos e incompatíveis com o agente atual.

Por ser código próprio, a comparação correta não é apenas “1,9 MB contra mais de
30 MB”: também é preciso comparar cobertura de protocolo, reconexão, atualização,
testes e segurança com o cliente oficial. O binário menor pode deliberadamente
implementar um subconjunto, o que é uma vantagem válida desde que documentada.

Tamanho do arquivo isolado não basta. Antes da decisão, medir em ARMHF: RSS após
autenticação, pico de RAM com streams, CPU em repouso, tempo de conexão, tamanho
compactado da atualização e dependências dinâmicas. Se 1,9 MB cabe e essas
métricas são aceitáveis, endurecer o cliente WSS existente oferece mais valor do
que manter um segundo protocolo.

Prioridades antes do cliente ultramínimo: heartbeat, backpressure, allowlist
local, testes ponta a ponta, limites contra abuso, atualização segura e revisão
do painel administrativo. O perfil C permanece uma opção especializada, não a
justificativa principal do projeto.
