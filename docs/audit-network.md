# Auditoria sênior de rede e protocolo — lan-mouse-custom

- **Data:** 2026-11-05
- **Commit auditado:** `ef5f41f` (HEAD; a árvore de trabalho tem alterações de outros agentes em `input-*/src`, não usadas como base).
- **Método:** análise somente leitura do código Rust, do `Cargo.lock` das dependências de rede (`webrtc-dtls 0.12.0`, `webrtc-util 0.11.0`) e testes empíricos com `target/debug/lan-mouse daemon` isolado (backends `dummy`, portas 44242/44243, `XDG_*` em `/tmp`), gerando tráfego UDP real com Python. Nenhum arquivo de código foi alterado.
- **Fora do escopo:** execução em Windows/macOS reais, peers oficiais upstream, rádio Wi-Fi físico e redes com NAT real; esses itens aparecem como hipótese.

## Resumo executivo

A camada de protocolo em si (serialização de `ProtoEvent`, compatibilidade com peers antigos, framing de clipboard) está em bom estado: nunca há pânico em datagrama truncado, ids 12–15 ficam reservados para clipboard, `Enter`/`Pong`/`Hello` têm extensões retrocompatíveis testadas, e o lado que conecta faz TOFU com pin de certificado. As falhas concentram-se no **ciclo de vida do listener UDP**, no **raciocínio de limites introduzido no hardening recente** e no **IPC local**.

**Top 3 riscos (P1):**

1. **P1-01 — DoS remoto não autenticado por crescimento de estado no listener.** O `webrtc-util` mantém um `HashMap` permanente por endpoint remoto (`ip:porta`) e `UdpConn::close()` é no-op. O rate-limit em `src/listen.rs` roda *depois* que esse estado já foi criado; os pacotes seguintes continuam sendo enfileirados (até 4 MiB por endpoint) sem leitor. Medido: RSS do serviço 48,2 MB → 68,4 MB com 4000 sockets e 3×4 KiB, **sem cair depois de fechar todos os sockets**. Um host na LAN pode rotacionar portas/fontes IPv6 e esgotar a memória; o limite `MAX_LIVE_CONNECTIONS` não protege contra isso.
2. **P1-02 — Listener nunca detecta sessão morta e nunca limpa `conns`.** Não há ping/watchdog no lado que escuta: `read_loop` só termina com erro de `recv`. Crash/kill -9/reboot/Wi-Fi drop sem `close_notify` deixam a entrada DTLS viva para sempre; após 128 acúmulos o listener recusa **todas** as conexões de entrada (log `too many live connections`) até reiniciar o serviço. `EMULATION_TIMEOUT` só solta teclas, não fecha a sessão.
3. **P1-03 — Janela de pairing "single use" não é atômica.** O verify lê a janela e a persistência a consome somente depois do handshake; dois handshakes concorrentes passam pelo verify e **ambos são adotados**. `StartPairing`/`pair` concede `allow_all` (controle total de entrada e saída), então um atacante na LAN pode correr com o pareamento legítimo.

Adicionais relevantes: rate-limit por IP é falsificável/compartilhado (P2-04), IPC Windows sem autenticação (P2-05), assimetria IPv6 (P2-07), descoberta IPv4-only sem porta 4245 no firewall (P2-08), troca de porta não persistida (P2-09) e troca de `AuthRequest` a cada `Pong` com escrita de auditoria (P2-10).

## Tabela de achados

| ID | Sev | Achado | Evidência principal | Impacto |
|----|-----|--------|---------------------|---------|
| P1-01 | P1 | Estado por endpoint nunca liberado no listener UDP; buffers crescem sem leitor | `src/listen.rs:194-217,248-252`; `webrtc-util/src/conn/conn_udp_listener.rs:198,227-228,283-285`; `webrtc-util/src/buffer/mod.rs:11-13,55-56` | DoS de memória remoto sem autenticação |
| P1-02 | P1 | Listener sem keepalive/timeout; `conns` acumula sessões mortas e estoura o cap de 128 | `src/listen.rs:211-217,600-677`; `src/emulation.rs:26,420-431` | Queda de entrada permanente após N desconexões sujas |
| P1-03 | P1 | Pairing de uso único com race entre verify e consumo; grants `allow_all` | `src/listen.rs:279-315,526-538`; `src/service.rs:546-550` | Bypass de controle de acesso (adoção dupla) |
| P2-04 | P2 | Rate-limit por IP: spoofável, compartilhado atrás de NAT, conta tentativas multi-endereço e é burlável com rotação IPv6 | `src/listen.rs:83-84,190-210`; `src/connect.rs:553-561`; README:620-623 | Lockout de peer legítimo; bypass por /64 |
| P2-05 | P2 | IPC Windows em TCP loopback sem autenticação, sem cap de conexões e com JSON sem limite de linha | `lan-mouse-ipc/src/listen.rs:52-59,146-178`; README:606-612 | Processo/usuário local controla o serviço (Windows) |
| P2-06 | P2 | `broadcast` do IPC é sequencial e sem timeout; frontend lento/travado trava o loop do serviço | `lan-mouse-ipc/src/listen.rs:104-123`; `src/service.rs:632-636` | Serviço deixa de processar estado/IPC |
| P2-07 | P2 | Listener IPv6 só no Linux; DNS descarta `scope_id` (link-local IPv6 inutilizável) | `src/listen.rs:565-586`; `src/dns.rs:125-128`; `src/connect.rs:486-497,593-596` | Rede IPv6-only: Windows/macOS só inicia, não recebe |
| P2-08 | P2 | Descoberta IPv4-only, não autenticada, com anúncios falsificáveis; firewall/README não abrem 4245/udp | `src/discovery.rs:33,102-123,384-394,467-489`; `firewall/lan-mouse.xml:7`; README:659 | Lista de rede vazia em firewalld; injeção de device/fingerprint falso |
| P2-09 | P2 | `ChangePort` não persiste a porta; restart volta à porta antiga | `src/service.rs:2215-2222`; `lan-mouse-gtk/src/window.rs:643` | Peers configurados para a porta nova deixam de conectar |
| P2-10 | P2 | `AuthRequest` por cada `Pong` (4/rodada, ~2/s) + auditoria + broadcast de estado repetidos | `src/connect.rs:851-870`; `src/emulation.rs:269-308`; `src/service.rs:867-887`; `src/audit.rs:113-130` | Tráfego/CPU/IO desnecessários; log de auditoria poluído |
| P2-11 | P2 | Bookkeeping de conexões de entrada é indexado por `addr`, não por identidade do `conn` | `src/listen.rs:401-408,415-427,671-675` | Não determinístico se `ip:porta` se repetir |
| P2-12 | P2 | Alocação imediata do tamanho declarado no Offer de clipboard; sem cap por fingerprint | `input-clipboard/src/transfer.rs:233-266`; `input-clipboard/src/lib.rs:109-112` | Peer pareado abre N sessões e amplifica memória (10 MiB/peer) |
| P2-13 | P2 | Dial paralelo sem cap: N endereços (DNS/fix_ips) = N sockets+handshakes por tentativa | `src/connect.rs:544-575,588-601` | Consumo de recursos sob DNS hostil/config extensa |
| P3-14 | P3 | `PROTO_VERSION` é anunciado mas nunca comparado/gateado | `lan-mouse-proto/src/lib.rs:19`; `src/connect.rs:813-827` | Mudança de serialização futura sem negociação |
| P3-15 | P3 | `DOC.md` descreve servidor TCP de requests que não existe no código atual | `DOC.md:43-46` | Documentação enganosa sobre o protocolo |
| P3-16 | P3 | `motion_batch_ms` documentado como default 4, código usa 2 | README:674; `src/config.rs:599` | Divergência doc/código |
| P3-17 | P3 | `hello_sent`/`auth_sessions` não são limpos no disconnect | `src/connect.rs:96,181-193,813-827` | Hello omitido se o mesmo `addr` for reutilizado |

## P1 em detalhe

### P1-01 — Listener retém estado por endpoint e buffers sem leitor (DoS remoto não autenticado)

**Evidência.** `bind_listener` usa `webrtc_util::conn::conn_udp_listener::listen` (`src/listen.rs:565-586`). Nessa implementação, o primeiro datagrama de um `(ip,porta)` remoto cria um `UdpConn`, é inserido em `HashMap<String, Arc<UdpConn>>` (`webrtc-util/src/conn/conn_udp_listener.rs:198,227-228`) e **nunca é removido** (não existe `remove` no arquivo); `UdpConn::close` é `Ok(())` sem efeito (`:283-285`). O `read_loop` da lib continua fazendo `conn.buffer.write(...)` para esse endpoint (`:164-170`), e o `Buffer` cresce até `MAX_SIZE = 4 MiB` (`webrtc-util/src/buffer/mod.rs:11-13,55-56`). O hardening em `src/listen.rs:190-217` aplica rate-limit e fecha o `conn` **depois** desse estado existir; o `close()` no-op não impede o buffering.

**Repro (validado nesta máquina).** Serviço isolado em 127.0.0.1:44242 e:

```python
import socket, time
socks=[]
for _ in range(4000):
    s=socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind(("127.0.0.1",0))
    s.sendto(b"\x16"+b"H"*40, ("127.0.0.1",44242)); socks.append(s)   # 1 datagrama
for _ in range(3):
    for s in socks: s.sendto(b"\x16"+b"D"*4092, ("127.0.0.1",44242))
    time.sleep(1)
for s in socks: s.close()
time.sleep(2)  # RSS do serviço permaneceu em 68,4 MB (baseline 48,2 MB)
```

O log confirma `rate limiting handshakes from 127.0.0.1 (... dropped so far)` enquanto o RSS segue crescendo e **não retorna** após fechar todos os sockets. Em LAN isso é remoto e não requer certificado: basta rotacionar portas de origem (e endereços IPv6 de um /64 para escapar do limite por IP), com até 4 MiB retidos por endpoint.

**Correção sugerida.** (a) trocar `listen()` por `ListenConfig { accept_filter: ... }` (`webrtc_util::conn::conn_udp_listener::ListenConfig`) e fazer a checagem de rate-limit/forma do datagrama **antes** da criação do `UdpConn`, pois o filter roda em `get_udp_conn` antes da alocação; (b) como a lib não expõe remoção, ou patchar/forkar `webrtc-util` para remover do mapa no `close()`/drop e expirar por inatividade, ou recriar o socket do listener periodicamente quando não houver sessões DTLS vivas; (c) limitar o `Buffer` (`set_limit_size`) por conexão. Alternativa de menor esforço: validar primeiro byte DTLS (22/20) no filter e recusar endpoints que não progridem em 1 s.

### P1-02 — Ausência de watchdog no listener: sessões mortas ocupam o cap de 128

**Evidência.** `read_loop` (`src/listen.rs:600-677`) bloqueia em `conn.recv()` e só remove a entrada quando o `recv` falha; não há envio de `Ping` do lado que escuta (ele só responde `Pong` em `src/emulation.rs:315-321`). `EMULATION_TIMEOUT` (3 s, `src/emulation.rs:26`) apenas remove o handle de emulação e solta teclas (`:420-431`), sem fechar o DTLS. O cap `MAX_LIVE_CONNECTIONS = 128` conta `conns.len()` incluindo essas sessões mortas (`src/listen.rs:86,211-217`).

**Cenário de repro.** `sudo nft add rule inet filter output udp dport 4242 drop` no peer + `kill -9` do lan-mouse (ou `tc qdisc add dev wlan0 root netem loss 100%` seguido de retorno da rede): o listener nunca recebe `close_notify`; a entrada fica para sempre. Repita o ciclo (Wi-Fi instável faz isso naturalmente; um peer pareado malicioso pode abrir 128 sessões de propósito, pois não há cap por fingerprint) e o log passa a `too many live connections, dropping handshake from ...` para todo peer novo até reiniciar. As mesmas entradas mortas também fazem `reply_raw` enviar para endereços antigos (`src/listen.rs:401-408`).

**Correção sugerida.** Watchdog por sessão de entrada simétrico ao do lado que conecta (`src/connect.rs:645-660,712-765`): pingar a cada ~2 s e fechar após 2–3 rodadas sem `Pong`; expirar por inatividade (ex.: 60 s sem eventos/pong); e contabilizar o cap por fingerprint/identidade, não só por sessão.

### P1-03 — Race na janela de pairing: duas adoções para uma janela "de uso único"

**Evidência.** O veredito de auto-autorização é gravado no callback de verificação (`src/listen.rs:526-538`) mas a janela só é consumida depois que o handshake termina (`src/listen.rs:279-315`), em outra task, sem CAS. Dois handshakes simultâneos durante a janela passam ambos pelo verify enquanto `window.is_open()` é verdadeiro; cada task carrega seu próprio `auto_source` e ambos inserem o fingerprint em `authorized` e emitem `AutoAuthorized`. `StartPairing` usa `DirectionPolicy::allow_all()` (`src/service.rs:546-550`).

**Cenário de repro.** `lan-mouse cli pair --seconds 60` e, na mesma janela de ~1 s, dois peers novos conectam simultaneamente (dois VMs de `scripts/vm-test-lab.sh gen-configs`, ou um atacante que já monitora o beacon e conecta no mesmo instante que o device legítimo). `lan-mouse cli history` mostra dois `grant_created source=pairing`; ambos ficam com controle total.

**Correção sugerida.** Consumir/reservar a janela de forma atômica **no callback de verify** (ex.: `Mutex<Option<PairingWindow>>` auxiliar com transição `Open -> Reserved(conn_id)`), devolvendo a reserva se o handshake falhar; ou serializar handshakes enquanto a janela estiver aberta (semáforo de 1).

## P2 em detalhe

- **P2-04 — Rate-limit por IP.** `HANDSHAKES_PER_IP=20/60 s` (`src/listen.rs:83-84`) é aplicado por `IpAddr`, depois do accept. Consequências: (i) spoofing de origem na LAN consome o orçamento alheio — 20 datagramas/min com source do peer travam as reconexões legítimas dele (`hping3 -2 --spoof <ip-do-peer> -p 4242 <vitima>`, requer L2/root); (ii) N máquinas atrás do mesmo NAT (ou mesmo IP corporativo) compartilham 20/min; (iii) cada tentativa do lado que conecta disca **todos** os endereços em paralelo (`src/connect.rs:553-561`), então um hostname com vários A/AAAA consome vários créditos por tentativa — a afirmação do README:620-623 ("normal reconnects stay well below that") não vale nesses casos; (iv) IPv6: cada endereço de um /64 tem orçamento próprio. **Fix:** aplicar o limite dentro do `accept_filter` (antes da alocação — ver P1-01), cobrar só handshakes que falham, agregar IPv6 por /64 e separar orçamento de reconexão de peers já pareados (ex.: por fingerprint pós-handshake).
- **P2-05 — IPC Windows.** `TcpListener::bind("127.0.0.1:5252")` (`lan-mouse-ipc/src/listen.rs:52-59`) aceita qualquer processo local sem crivo (`:146-178`); no Unix há `SO_PEERCRED` + UID (`:134-144`), no Windows não há equivalente. Um processo local pode enviar, por exemplo, `{"StartPairing":600}` e depois conectar um device próprio, obtendo `allow_all`, além de poder ocupar a porta 5252 para impedir o serviço de subir. Também não há cap de conexões nem limite de linha (`BufReader::lines()` sem máximo) → DoS local por memória/fds. O README:606-612 documenta a limitação, mas ela permite escalada entre usuários Windows. **Fix:** named pipe com security descriptor restrito ao usuário, ou token aleatório em arquivo com ACL; limite de conexões e de linha (ex.: 1 MiB).
- **P2-06 — Broadcast IPC bloqueante.** `broadcast` escreve em cada frontend sequencialmente com `await` e sem timeout (`lan-mouse-ipc/src/listen.rs:104-123`); o serviço consome via `handle_frontend_pending().await` no próprio `select!` (`src/service.rs:632-636`). Um GUI travado/pausado que não lê o socket congela o processamento de estado do serviço (entrada continua fluindo por tasks separadas, mas IPC/discovery/config param). **Fix:** tarefa de escrita dedicada por cliente com fila limitada e timeout.
- **P2-07 — IPv6 assimétrico.** `bind_listener` só tenta `[::]` no Linux; Windows/macOS escutam apenas `0.0.0.0` (`src/listen.rs:565-586`). Em rede IPv6-only os peers desses OS conseguem discar (connect usa `[::]:0`, `src/connect.rs:486-497`) mas ninguém consegue conectar neles → controle unidirecional. Além disso `dns.rs:125-128` converte `SocketAddr` em `IpAddr` e o connect remonta `SocketAddr::new(ip, porta)` (`src/connect.rs:593-596`), perdendo `scope_id` — endereços link-local `fe80::` resolvidos nunca conectam. **Fix:** dois listeners (V4+V6) por plataforma ou socket dual-stack quando disponível; manter `SocketAddr`/`scope_id` do DNS até o connect.
- **P2-08 — Descoberta.** `local_interfaces()` e o scan só consideram IPv4 (`src/discovery.rs:102-123,238-299`); o beacon responde a qualquer um e aceita `LMANNC01` de qualquer origem sem autenticação, preenchendo hostname/versão/fingerprint na UI (`:409-489`, `:384-394`). Anúncio forjado pode semear um "device" com fingerprint controlado pelo atacante e induzir o usuário a autorizá-lo. O firewall empacotado abre **só** 4242/udp (`firewall/lan-mouse.xml:7`) e o README só cita 4242 (README:659) — em host com firewalld a lista de rede fica vazia mesmo funcionando o controle manual, porque 4245/udp fica bloqueado; o instalador Windows usa regra por programa e cobre 4245. **Fix:** abrir 4245/udp em `firewall/lan-mouse.xml` e documentar; tratar anúncio como dica não confiável (não exibir fingerprint como pronto para autorizar, ou exigir SAS); limitar taxa de respostas do beacon e responder apenas a MAGIC (já faz) com cookie para mitigar reflexão (8 bytes → ~200 bytes).
- **P2-09 — Porta não persiste.** `ChangePort` só chama `emulation.request_port_change` (`src/service.rs:2215-2222`) e não há `Config::set_port`; o GUI chama `ChangePort` (`lan-mouse-gtk/src/window.rs:643`) sem `SaveConfiguration`. Após reiniciar, a porta volta para o valor do TOML/default 4242 enquanto os peers foram configurados para a nova porta → link não reconecta. **Fix:** atualizar `config.port` e `save_config()` no `PortChanged` bem-sucedido.
- **P2-10 — Chatter de permissões.** O lado que conecta envia `AuthRequest` a **cada** `Pong` com bit `PERMISSIONS` (`src/connect.rs:851-870`); o listener responde `AuthResponse` a cada um (`src/emulation.rs:269-308`), o serviço audita `peer_permissions` e rebroadcasta estado (`src/service.rs:867-887`), e `AuditLog::append` abre o arquivo a cada evento (`src/audit.rs:113-130`). Com 4 pings/rodada de ~2 s isso dá ~2 req/s por peer de tráfego e escrita em disco permanentes (log rotaciona em 5 MiB, mas polui o histórico e gera I/O). **Fix:** enviar `AuthRequest` uma vez por conexão (e em mudança de política), deduplicar `AuthResponse` e auditar só transições.
- **P2-11 — Bookkeeping por addr.** `reply_raw` envia para **todas** as entradas com o mesmo `addr` (`src/listen.rs:401-408`); `disconnect` (`:415-427`) e a limpeza do `read_loop` (`:671-675`) removem a **primeira** entrada que casa com `addr`. Se um `ip:porta` for reutilizado (NAT simétrico/keepalive, socket rebind) sobram entradas antigas e datagramas duplicados. **Fix:** indexar por identidade do `Arc<DTLSConn>` (ou um id de sessão monotônico) em vez de `SocketAddr`.
- **P2-12 — Amplificação de memória no clipboard.** `from_offer` aloca imediatamente `vec![0; size]` para cada representação antes de receber qualquer chunk (`input-clipboard/src/transfer.rs:233-266`), com teto de 10 MiB por snapshot (`input-clipboard/src/lib.rs:109-112`). O listener aceita até 128 sessões e não limita sessões por fingerprint (P1-02), então um device **pareado** pode abrir N conexões e enviar offers de 8 MiB → ~N×10 MiB. **Fix:** cap de sessões por fingerprint (2 é suficiente), alocação preguiçosa/limitada por chunk e orçamento global de transferências.
- **P2-13 — Fan-out de dial.** `connect_any` cria um `JoinSet` com um `connect()` (e um socket UDP) por endereço (`src/connect.rs:544-575`); a lista vem de `fix_ips` (config/CLI, sem limite) + DNS (`lookup_host`, potencialmente dezenas de registros, `src/connect.rs:588-601`). DNS hostil na LAN (ou rebinding) infla sockets/handshakes por tentativa. **Fix:** cap de endereços por tentativa (ex.: 8), preferência por família (happy eyeballs) e limite no tamanho de `ips` configurado.

## P3 / divergências de documentação

- **P3-14:** `PROTO_VERSION` só é enviado no `Hello` e exibido como diagnóstico; nenhum código compara a versão do peer (`lan-mouse-proto/src/lib.rs:19`; envio em `src/connect.rs:813-827`). A compatibilidade real depende só dos bits de capability — mudanças futuras não aditivas não têm gate.
- **P3-15:** `DOC.md:43-46` ainda descreve um "simple tcp server ... accepts requests" na mesma porta do UDP; o código atual só tem DTLS/UDP de eventos e o IPC local (TCP apenas no Windows, porta 5252). Corrigir o DOC.
- **P3-16:** README:674 diz `motion_batch_ms` default `4`; o código usa `2` (`src/config.rs:599`).
- **P3-17:** `hello_sent`/`auth_sessions` são indexados por `SocketAddr` e não são removidos em `disconnect` (`src/connect.rs:96,181-193`); se o mesmo `ip:porta` for reutilizado numa reconexão, `send_hello_once` não envia `Hello` de novo (`:813-827`), deixando versão/capabilities do peer desconhecidas.

## Cobertura da missão (rastreio)

1. Descoberta/mDNS/DNS: P2-07, P2-08, P2-13 (sockets, bind, resolução, spoofing/DoS de discovery; item 16 do checklist).
2. DTLS/UDP: P1-01, P1-02, P2-10, P2-11 (MTU: chunk de clipboard 1024 B < `DEFAULT_MTU` 1200 em `lan-mouse-proto/src/clipboard.rs:21-26` e `webrtc-dtls/src/config.rs`; perda/reordenação cobertas por watchdog e retransmissão de `Enter`; NAT = limitação documentada).
3. Reconexão/estado: P1-02, P2-04, P3-17 (backoff 250 ms→5 s em `src/connect.rs:55-57,147-170`; rate-limit recente não tem regressão funcional nos testes unitários, mas carrega os problemas de P2-04).
4. IPC frontend: P2-05, P2-06, P3-15 (Unix com `SO_PEERCRED` correto; Windows sem auth; JSON por linha sem limite).
5. Config/permissões de rede: P1-03, P2-08, P2-09 (kiosk, denies expiráveis e address rules revisados; ver "controles corretos" abaixo).
6. Matriz de rede: seção seguinte.

## Controles que estão corretos (não regredir)

- Kiosk (`accept_incoming=false`) é verificado no callback de certificado, cobrindo authorized, address rules e pairing (`src/listen.rs:497-504`).
- Deny list tem precedência sobre authorized/address rule/pairing e respeita expiração (`src/listen.rs:511-518`; `src/devices.rs:368-380`).
- TOFU do lado que conecta fixa o fingerprint apresentado e falha se o host trocar de certificado (`src/connect.rs:504-533,604-615`).
- Device desconhecido recebe default deny (`incoming_policy`, `src/listen.rs:686-701`); todo evento passa por `event_allowed` (`:704-727`).
- Compatibilidade upstream bem tratada: ids 12–15 reservados (`lan-mouse-proto/src/lib.rs:227-231`), `Enter` sem serial decodifica como 0, `Pong`/`Hello` aceitam bytes extras e o teste de truncamento garante ausência de pânico (`lan-mouse-proto/src/lib.rs:262-372,662-736`).
- IPC Unix rejeita outro UID via `SO_PEERCRED` (`lan-mouse-ipc/src/listen.rs:134-144`) e o socket vive em `$XDG_RUNTIME_DIR` 0700 (README:608-610).
- Clipboard tem chunking, ack e retransmissão com limites de tamanho no Offer (`lan-mouse-proto/src/clipboard.rs:21-29`; `input-clipboard/src/transfer.rs:233-281`).
- Address rules largas (> /24 IPv4, > /64 IPv6) são sinalizadas na UI (`src/trusted.rs:72-83`; `src/service.rs:1784-1790`).

## Matriz de rede (comportamento observado no código)

| Cenário | Comportamento / risco | Evidência |
|---------|----------------------|-----------|
| Wi-Fi + Ethernet / multi-homed | Dial disca todos os IPs configurados em paralelo; scan e link-kind são heurísticos por interface (nomes/prefixos); ARP só no Linux | `src/connect.rs:544-575`; `src/discovery.rs:102-171,204-227` |
| IPv4-only | Caminho principal, funciona (listener 0.0.0.0 em Windows/macOS e dual-stack no Linux) | `src/listen.rs:565-586` |
| IPv6-only / dual-stack | Linux escuta IPv6; Windows/macOS não recebem; link-local sem scope_id falha; discovery não vê IPv6 | P2-07; `src/discovery.rs:102-123` |
| CGNAT / NAT doméstico | Sem port-forward não há entrada; ambos os lados precisam iniciar para bidirecional; docs dizem LAN | README:653-668; `src/connect.rs:486-497` |
| Firewall simétrico (UDP bloqueado) | Sem conexão; `connect_any` falha em ~2 s por endereço e o backoff reconecta (250 ms → 5 s) | `src/connect.rs:53,55-57,147-170` |
| Firewall assimétrico (A→B ok, B→A não) | Link A→B funciona; reverse falha; hello/permissions têm fallback pelo lado que escuta (`PeerHello`), mas não há retry de entrada | `src/emulation.rs:333-345`; `src/connect.rs:626-631` |
| Perda 20% | Handshake DTLS pode estourar `ADDRESS_TIMEOUT` (2 s) em links ruins; com link ativo, 2 rodadas de ping (~4 s) fecham; reconexão respeita backoff e o rate-limit de 20/min pode morder | `src/connect.rs:53,645-660,712-765`; P2-04 |
| Perda 100% / suspensão-retomada | Lado que conecta fecha após watchdog e reconecta; **lado que escuta não fecha** (P1-02) e acumula estado; após retomada o reconnect cria nova porta e nova entrada | P1-02; `src/listen.rs:600-677` |
| Troca de IP/hostname | Re-resolução só ocorre em `activate_client`; link ativo mantém o addr antigo até cair; hostname descoberto atualiza só o nome de exibição | `src/service.rs:954-975,2072-2085,2164-2180` |
| Dois peers atrás do mesmo NAT | Compartilham o orçamento de handshake por IP no listener; descoberta vê o IP do NAT e não os hosts internos | P2-04; `src/discovery.rs:238-299` |

## O que não consegui confirmar (hipótese)

- Comportamento real em Windows/macOS (IPC 5252, firewall e IPv6) — analisado apenas por código e `cfg`, sem execução nas plataformas.
- Ausência de timeout ocioso no `webrtc-dtls 0.12.0`: inferida da config/`recv` (não há campo de idle timeout), mas não testada contra um DTLS conn real de longa duração.
- P2-11 (duplicidade por `ip:porta` igual): depende de reutilização de porta de origem/NAT; não reproduzido, pois o `connect()` sempre binda porta efêmera nova.
- P3-17 (Hello omitido em reconexão com mesmo addr): plausível, mas não reproduzido.
- Números de P2-10/P2-12: estimados pelo código (4 pings/rodada, tetos de clipboard), não medidos com peer real.
- Interop com binários upstream oficiais: validada só por leitura das extensões retrocompatíveis (`Pong` com bytes extras, `Enter` sem serial, ids 12–15) e pelos testes do crate.
- Se `MAX_LIVE_CONNECTIONS=128` é atingível em uso normal: depende da frequência de quedas sujas; cenário descrito é o pior caso por design.

## Correções priorizadas

1. **P1-01:** patch/fork do `webrtc-util` para liberar o mapa de conns em `close()`/drop + expiração; mover rate-limit para `accept_filter`; limitar `Buffer`.
2. **P1-02:** watchdog de ping e timeout ocioso por sessão de entrada; cap por fingerprint; limpar `conns`/estado derivado por identidade de sessão.
3. **P1-03:** reserva atômica da janela de pairing no callback de verify (rollback no handshake falho).
4. **P2-09:** persistir porta; **P2-08:** abrir 4245/udp e tratar beacon como não confiável; **P2-07:** dual-stack no listener de todas as plataformas e preservar `scope_id`.
5. **P2-05/P2-06:** autenticar o IPC Windows (named pipe+ACL ou token), limitar conexões/linhas e desacoplar o broadcast do loop do serviço.
6. **P2-04/P2-13/P2-12:** limites agregados e por identidade (IP+/64, cap de dials, cap de sessões/transferências por fingerprint).
7. **P2-10/P2-11:** deduplicar `AuthRequest/AuthResponse`, auditar só transições, indexar conexões por identidade.
8. **P3-14/15/16/17:** gate de `PROTO_VERSION`, corrigir `DOC.md`/README e limpar mapas por `addr` no disconnect.
