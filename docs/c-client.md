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
