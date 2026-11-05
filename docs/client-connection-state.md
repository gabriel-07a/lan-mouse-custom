# Estado de conexão do cliente: veracidade no serviço/IPC e recomendações de UX

> **TL;DR (estado final)** — A UI final (`RowConnectionState`,
> `lan-mouse-gtk/src/client_row.rs:24-126`) deriva honestamente de `active` + `alive` +
> `active-addr` + `incoming-addr`: só mostra **“Connected”** com link de saída estabelecido
> (`active-addr`, gravado apenas após o handshake DTLS); `alive=false` em link vivo vira
> **`ConnectedDegraded`** (âmbar, “emulation disabled on the other PC”), nunca “reconnecting”;
> `Connecting` mostra **“Trying \<target\> — no answer yet”** e não afirma aprovação;
> entrada-only mostra **“Peer connected” / “Incoming session”** e nunca “receiving input”.
> O que ela **deliberadamente não afirma** (e o IPC não permite): frescor/liveness
> (`last_pong_at`), motivo do “no answer” (offline × firewall × DNS × certificado ×
> aprovação), input de fato aplicado (`Enter`/`Leave`) e last-seen. Fase 2: `ConnectionPhase`
> opcional + `last_error`/`remote_entered` + correções de corrida no serviço (§9).

Escopo: pipeline auditado em `main` @ `7e3e11c` (árvore limpa; `src/` e `lan-mouse-ipc/`
inalterados). A UI final está na working tree (sem commit): `client_row.rs` com 833 linhas;
citações por símbolo/linha do snapshot de 2026-11-05. Plano irmão:
[connection-permissions-plan.md](connection-permissions-plan.md) (propõe `LinkState` em
`DeviceView`, ver §6). Pontos do revisor independente (task-8) corrigidos nesta versão.

## 1. O que é “conectado de verdade”

- **Saída (nós controlamos):** `active_addr` é gravado **só** após `DTLSConn::new` concluir com
  certificado verificado/pinado (`src/connect.rs:479-542` → `:600-601` → `:617`). Logo
  `active_addr.is_some()` ⇒ *handshake DTLS concluído e peer aceitou nosso certificado*. É o
  sinal mais forte que existe — e não prova liveness.
- **Entrada (o peer nos controla):** aceite do DTLS no listener (`src/listen.rs:248-330`,
  `Accept` `:317-320`) grava `incoming_addr` (`src/service.rs:861-864`). Prova *sessão aberta*,
  não que o peer está enviando input.
- **Liveness agora:** não é exposto. `alive` vem do payload `Pong` preenchido com
  `emulation_active` **local do peer** (`lan-mouse-ipc/src/lib.rs:199-200`;
  `src/emulation.rs:315-321`, `:448`, `:492-499`).

## 2. Campos, quem escreve e o que significam

| Campo | Escrito | Limpo | Significado real | Confiável p/ “conectado”? |
|---|---|---|---|---|
| `active` | `activate_client` (`src/client.rs:72-81`; origem `activate_on_startup` `src/config.rs:515`) | `deactivate_client` (`client.rs:85-97`) | intenção do usuário (“habilitado”) | Não |
| `active_addr` | `connect.rs:617` (pós-handshake); a cada `Pong` `connect.rs:855` | `disconnect` (`connect.rs:185`), deactivate (`client.rs:90`), `set_hostname` (`client.rs:204`) | link de saída estabelecido | Handshake sim; liveness não |
| `alive` | `connect.rs:621` (**otimista, antes de qualquer Pong**); a cada `Pong` `connect.rs:856` | `disconnect` (`:186`), deactivate (`client.rs:91`) | emulação do peer ligada (último Pong) | Não p/ liveness |
| `rtt_ms` | só em `Pong` (`connect.rs:858-861`; média móvel `client.rs:425-439`) | disconnect/deactivate | RTT do último Pong, sem idade | Idade invisível |
| `resolving` | `service.rs:957` / `:961` via `DnsEvent` (`src/dns.rs:79-112`) | idem | resolução em andamento | — |
| `incoming_addr` | `service.rs:861-864` (aceite DTLS), `:817-820` (`AutoAuthorized`), `:723-728` (`Entered`) | `service.rs:730-745` (`Disconnected`) | sessão de entrada aberta (≠ controlando) | Parcial |
| `peer_commit`/`peer_caps_known` | `Hello`/`Pong` (`connect.rs:862-884`) | `disconnect` (`:187`) | versão/capacidades | — |
| `DeviceView.connected` | derivado `service.rs:1148-1163` = `alive \|\| incoming_addr \|\| incoming_conn_info` | idem | “alguma sessão viva” | Parcial e stale (R8) |

`FrontendEvent::State` (`lan-mouse-ipc/src/lib.rs:538`) chega a cada mudança via
`notify_connected/notify_state_changed` (`connect.rs:107-116`) → `service.rs:326-364` →
`broadcast_client` (`service.rs:2282-2289`). O frontend **é atualizado na hora** em
handshake/queda; falta semântica, não entrega.

## 3. Linha do tempo

**Saída.** `Activate(true)` (`service.rs:2164-2213`) → `ensure_connected`
(`connect.rs:317-319`) → `start_connecting` (`:131-144`, no-op se já há `active_addr`) →
`connect_any` disca todos os IPs em paralelo (timeout 2 s: `:53`, `:544-575`; pin `:504-533`,
timeout `:534-541`) → sucesso grava `active_addr` e `alive=true` **otimista** (`:617-624`) e
emite `State`. `ping_pong` (`:712-766`) faz rodadas de 4 Pings/2 s e mede RTT; sem Pong fecha
após **2 rodadas** se o peer já respondeu e **30 rodadas (~60 s)** se nunca respondeu
(`:645-660`, `:692-709`); 4 erros de envio fecham (`:678-690`). `receive_loop` (`:829-917`)
regrava estado a cada `Pong` (`:851-861`) e, no erro, chama `disconnect` (`:916`) que limpa
`active_addr/alive/rtt/peer_commit` (`:181-193`) e reagenda com backoff 250 ms→5 s
(`:147-170`). `Disconnect` do usuário (`client.rs:85-97`) **não fecha o socket** (únicos
`close()` no watchdog `:728,749`).

**Entrada.** Verificação do certificado no handshake (`listen.rs:485-561`: kiosk `:501-504`,
deny `:515-518`, autorizado `:520-522`, pairing `:526-538`, regra de endereço `:544-556`,
senão recusa `:558`) → aceite → `EmulationEvent::Connected` (`emulation.rs:356-363`) →
`incoming_addr` **antes de qualquer Enter** (`service.rs:861-864`) → `Enter` →
`EmulationEvent::Entered` (`emulation.rs:251-262`; `service.rs:703-729`). `Leave`
(`emulation.rs:264-267`) **não gera evento**: o serviço não sabe que o peer saiu da tela.
Queda: `Closed`→`Disconnected` (`emulation.rs:382-392`) limpa `incoming_addr`
(`service.rs:730-745`); timeout de 3 s sem pacotes também (`emulation.rs:26`, `:420-430`).
O fluxo de entrada é mapeado para a linha por **IP** (`service.rs:723,733,861` →
`find_by_ip`, `client.rs:411-416`); só `DeviceView` usa fingerprint (`service.rs:1144-1147`).

## 4. Riscos de estado stale/mentiroso (ordenados)

- **R1 — `alive` é emulação, não liveness; e é otimista.** Escrito no handshake
  (`connect.rs:621`), antes de qualquer Pong; rodadas perdidas só fecham a conexão, não
  rebaixam `alive`. “Connected” stale por 2–6 s após queda (peer que já respondeu) e **até
  ~60 s para peer write-only** (legado/upstream). A UI final **corrigiu o rótulo**
  (`alive=false` com link = `ConnectedDegraded`, `client_row.rs:120`, badge `:50`), mas o dado
  continua sem frescor: o frontend não tem `last_pong_at` para rebaixar para “sem
  confirmação” (§9).
- **R2 — “Waiting approval” deixou de ser afirmado, mas o motivo do “no answer” é
  desconhecido.** A UI final mostra `Connecting…` + “Trying \<target\> — no answer yet”
  (`client_row.rs:428-439`) e o tooltip sugere checar o outro PC (`:497-499`) — honesto. O
  `Connecting` ainda cobre peer desligado, daemon parado, firewall, DNS, porta errada, cert
  recusado **e** aprovação pendente, porque os erros são descartados no serviço
  (`connect.rs:140-142`, `:570-574`) e não há campo/evento de erro (§9).
- **R3 — “Receiving input” não é afirmado.** `incoming_addr` nasce no aceite DTLS
  (`service.rs:861`), antes do `Enter`; `Leave` não limpa; peer com emulação desligada ainda
  dá `Ack` e dispara `Entered`, mas descarta input (`emulation.rs:251-262`, `:501-508`). A UI
  final usa badge **“Peer connected”** e subtítulo **“Incoming session from …”**
  (`client_row.rs:55`, `:472-486`), e o tooltip diz que o input só é aplicado enquanto o peer
  entra na tela (`:519-527`). Continua faltando o estado real `Enter`/`Leave` no IPC (§9).
- **R4 — Corridas entre conexões.** `disconnect` limpa `active_addr/alive` sem checar se ainda
  é a conexão corrente (`connect.rs:181-193`); `Pong` de conexão antiga regrava `active_addr`
  sem checar o mapa (`:855`); erro de envio chama `disconnect` **sem fechar** o socket
  (`:421-423`) → flapping/fantasma. A UI final reflete o que o serviço publica: se o estado
  oscilar, o badge oscila junto.
- **R5 — Desabilitar não desconecta.** `deactivate_client` não fecha DTLS; o `receive_loop`
  antigo volta a preencher `active_addr/alive` em ~2 s com `active=false` (a UI final ignora
  `addr/alive` quando `!active`, `client_row.rs:112,588-591`, mas outros consumidores não).
  Pior: `send_now` de teardown (`capture.rs:870-892`) → `conn_for`→`start_connecting`, que não
  checa `is_active` (`connect.rs:197-230` vs `:131-144`), pode abrir conexão para cliente
  desativado.
- **R6 — Cancelar no handshake pode não cancelar.** `connect_to_handle` não revalida
  `is_active` após o handshake (`connect.rs:600-631`): desativar com tentativa em voo ainda
  grava `active_addr`/`alive` e emite `State` — o botão “Cancel” promete parar a tentativa.
- **R7 — Limpeza parcial.** `set_hostname` zera `active_addr` mas mantém `alive`/`rtt`
  (`client.rs:201-208`); `incoming_addr` não é limpo em deactivate (`client.rs:85-97`, ok por
  desenho) — mais um indício de que “um estado só” não fecha.
- **R8 — `Devices` e toast ainda otimistas/estale.** A linha de cliente agora deriva de
  `row_connection_state` (`client_row.rs:103-126`), mas o painel de Devices continua com
  derivação própria: `device_subtitle` (`window.rs:79-105`) mostra “Receiving” quando
  `DeviceView.connected && active` (falso: ativo ≠ recebendo), e `DeviceView.connected`
  (`service.rs:1148-1163`) agrega `alive || incoming_addr || incoming_conn_info`.
  `broadcast_devices` não é chamado no braço de conexão (`service.rs:326-364`; só `:359` ao
  aprender fingerprint): painel desatualizado até a próxima ação de política/comando. O toast
  “device connected” (`lan-mouse-gtk/src/lib.rs:437-442`) dispara em `DeviceConnected`, antes
  de qualquer `Pong`.
- **R9 — Identidade/endereço.** Sem pin (TOFU), `active_addr` pode apontar para outro Lan
  Mouse no IP reutilizado (`connect.rs:530-533`, `:604-615`) — “conectado ao device errado”.
  Entrada mapeada por IP falha em multihomed/NAT/dual-stack → linha pode não entrar em
  `Receiving` enquanto o peer controla (`service.rs:723,733,861`).
- **R10 — Pendências enganosas.** Revogado/negado que reconecta vira pendência (fingerprint
  gravado antes do deny: `listen.rs:508-509` vs `:515-518` → `Rejected` →
  `ConnectionAttempt` `emulation.rs:393-398`; `pending_upsert` não filtra negados
  `devices.rs:395-432`). Kiosk recusa em silêncio (`listen.rs:501-504`): peer fica em
  “Connecting” para sempre.

## 5. Máquina de estados: implementado agora × recomendado (fase 2)

A UI final implementa 6 estados derivados de `active`/`alive`/`active-addr`/`incoming-addr`
(`client_row.rs:103-126`) e escolhe uma ação única (`:173-183`). Tabela do comportamento
observável:

| Estado | Derivação | Badge (cor) | Spinner | Botão | Subtitle (resumo) |
|---|---|---|---|---|---|
| `Disabled` | `!active && !incoming` | “Disconnected” (cinza) | não | Connect | “Disconnected · Screen …” |
| `Connecting` | `active && !addr && !incoming` | “Connecting…” (âmbar) | **sim** | Cancel | “Trying \<host/ip:porta\> · Screen … — no answer yet” |
| `ConnectedDegraded` | `active && addr && !alive` | “Connected” (âmbar, `status-degraded`) | não | Disconnect | “Connected · addr · … — emulation disabled on the other PC” |
| `Connected` | `active && addr && alive && !incoming` | “Connected” (verde) | não | Disconnect | “Connected · addr · Screen · RTT” |
| `ConnectedBidirectional` | `active && addr && alive && incoming` | “Connected” (verde) | não | Disconnect | “Connected (bidirectional) · addr · …” |
| `Receiving` | `incoming && (!active || !addr)` | **“Peer connected”** (verde) | sim se `active` (saída pendente); não se `!active` | Cancel se `active`; senão “Also connect” | “Incoming session from \<addr\> · Screen … [— outgoing still being established]” |

Detalhes de fidelidade: **não existe estado “Reconnecting”** por design — um link que existe
não está sendo reestabelecido; tentativas ficam em `Connecting` (`client_row.rs:17-22`).
Spinner em `Connecting` e em `Receiving && active` (tentativa de saída pendente,
`is_pending(active)` em `client_row.rs:77-81`); `Connected`, `ConnectedDegraded` e
`Receiving && !active` não giram (link estabelecido ou nada pendente, `style.css:44-48`). Tooltips do badge e do botão são explícitos e não afirmam o que não se
sabe (`client_row.rs:492-528`, `:151-160`): “input is applied only while it enters your
screen”, “an incoming connection stays active”. A derivação ignora `addr/alive` stale quando
`!active` (teste `client_row.rs:582-592`, `:817-826`) e só usa “Connected” para link
estabelecido (testes `:665-680`).

**O que a UI final deliberadamente NÃO afirma** (portanto não pode ser cobrado dela):
liveness/frescor do último Pong, motivo do “no answer”, aprovação pendente no peer (só
sugerida no tooltip), input realmente aplicado e last-seen. Esses dados não existem no IPC —
ver §9.

## 6. Um campo explícito no IPC? Sim — aditivo e opcional

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPhase { #[default] Disabled, ResolvingDns, Connecting,
    WaitingApproval, Connected, Degraded, Error }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionError { NoAnswer, DnsFailed, NoAddress, Refused, CertificateMismatch,
    EmulationDisabled }

pub struct ClientState { /* … */
    #[serde(default)] pub phase: Option<ConnectionPhase>,   // None = serviço antigo
    #[serde(default)] pub last_error: Option<ConnectionError>,
    #[serde(default)] pub connected_since: Option<u64>,     // unix s
    #[serde(default)] pub last_pong_at: Option<u64>,        // frescor do batimento
    #[serde(default)] pub remote_entered: bool,             // Enter sem Leave
    #[serde(default)] pub attempts: u32,                    // tentativas de conexão atuais
}
```

- **Sem `Reconnecting` no enum**: a UI final decidiu colapsar retry em `Connecting`
  (`client_row.rs:17-22`); se o serviço quiser distinguir para log/telemetria, use
  `last_error`/`attempts` em vez de um estado visível.
- **`Option<T>` + `#[serde(default)]` é obrigatório**: um enum obrigatório com
  `#[serde(default)]` faria GUI nova + serviço velho exibir “Desativado” para tudo. O padrão
  `Option` já existe no arquivo (`peer_incoming_caps: Option<u8>`, `lib.rs:238`).
- **Campos novos são compatíveis**: serde_json ignora desconhecidos por padrão, então GUI/CLI
  antigas continuam funcionando; GUI nova com serviço velho cai na derivação legada.
  `IPC_VERSION` fica 1; `Hello` é só aviso (`lib.rs:63-69`; GUI `window.rs:337-349`).
- **Variante nova de `FrontendEvent` é mais arriscada**: a GUI ignora linha indecifrável
  (`lan-mouse-gtk/src/lib.rs:245-261`), mas a CLI propaga `e?` e aborta
  (`lan-mouse-cli/src/lib.rs:319-320`, `:636-645`). Antes de variante nova, tolerância na CLI.
- **Custo**: serviço classifica o erro em `connect_to_handle`/`connect_any` (já distingue
  `Timeout` de `Dtls(...)`, `connect.rs:562-574`) e grava via `ClientManager` como
  `set_alive` (~60-100 LOC + testes); IPC tipos + teste de payload antigo (precedente
  `lib.rs:953-964`); frontend consome `phase` onde hoje deriva (~80 LOC). **Não exige mudança
  no protocolo entre peers** (`PROTO_VERSION` fica 1).

**Decisão:** fase 1 **entregue** — a UI final deriva honestamente das propriedades atuais
(só “Connected” com link estabelecido; “no answer yet”; “Peer connected”). Fase 2 = os campos
acima para erro/frescor/`Enter` e paridade com `Devices` (§9). Alinhado ao `LinkState` de
`connection-permissions-plan.md:254`, mas **como `Option<ConnectionPhase>`**.

## 7. Recomendações (status: o que já está feito × o que resta)

1. **[somente frontend — FEITO] Invariantes da UI verídica** (não regredir): badge só
   “Connected” para `Connected`/`ConnectedBidirectional`/`ConnectedDegraded`; `Receiving` =
   “Peer connected” e nunca “receiving input”; `Connecting` = “no answer yet” e nunca
   “waiting for approval”; spinner quando há tentativa pendente (`Connecting` ou `Receiving && active`); `alive=false` em link vivo =
   `ConnectedDegraded` âmbar, nunca reconnection. Cobertura: `client_row.rs:581-799`
   (badge, classes, spinner, ações, derivação, target IPv6).
2. **[fase 2, frontend] Frescor visível sem IPC.** Cada `State` com `rtt`/`alive` é um
   batimento (~2 s): guardar o instante na row e, após ~6 s sem batimento, rebaixar para
   `ConnectedDegraded`/“sem confirmação do outro lado” com “última resposta há Xs”; com
   `resolving=false && ips.is_empty() && hostname` mostrar “DNS: não resolveu \<host\>”.
   Idealmente com `last_pong_at` (§6) para não depender de heurística de UI.
3. **[serviço, P1] Parar de engolir erro de conexão.** `connect_to_handle`/`connect_any` só
   logam (`connect.rs:140-142`, `:570-574`): emitir `FrontendEvent::Error` na transição (sem
   spam) com motivo grosseiro: sem resposta / recusado / certificado / sem endereço.
4. **[serviço, P1] Corrigir corridas e ciclo de vida (sem IPC).** `disconnect` só limpa se
   `active_addr == addr` (`connect.rs:181-193`); `Pong` só grava se `conns[addr]` é a conexão
   corrente (`:855`); erro de envio fecha o conn (`:421-423`); após handshake revalidar
   `is_active` e fechar se cancelado (`:600-631`); `start_connecting`/`connect_to_handle`
   respeitarem `is_active` (`:131-144`, `:577-643`); `set_hostname` limpar `alive/rtt`
   (`client.rs:201-208`); **fechar o DTLS em deactivate/revoke** (hoje `client.rs:85-97` só
   muda estado e o `receive_loop` repõe `active_addr`; revoke mantém o link de propósito
   `service.rs:1390-1392`).
5. **[IPC + serviço, P2] `ConnectionPhase` opcional** (§6) — o que torna
   `WaitingApproval`, `Error(motivo)`, `ResolvingDns` e idade do batimento fiéis.
6. **[serviço, P2] Entrada por fingerprint + `remote_entered`.** Usar
   `incoming_conn_info` (já traz fingerprint, `service.rs:1052-1065`) em vez de `find_by_ip`
   (`:723,733,861`) para corrigir multihomed/NAT/dual-stack; propagar `Enter` **e** `Leave`
   até o serviço — hoje `Leave` morre em `emulation.rs:264-267`, então “input aplicado” nunca
   pode ser afirmado.
7. **[serviço, P2] `Devices` ao vivo e derivação única (R8).** `broadcast_devices`
   throttleado nas mudanças de conexão (`service.rs:326-364` hoje não chama) e `DeviceView`
   usando a mesma fase da linha; corrigir `window.rs:79-105` (“Receiving” com
   `connected && active` é falso), o ícone (`window.rs:856-866`) e o toast otimista
   (`lan-mouse-gtk/src/lib.rs:437-442`).
8. **[frontend + serviço, P3] Pendências e bloqueios honestos.** Filtrar fingerprints
   `denied` em `pending_upsert` (`devices.rs:395-432`) para revogado não virar “novo pedido”
   (`listen.rs:508-518`); expor `accept_incoming` em `Diagnostics` e mostrar “modo quiosque”
   (recusa silenciosa em `listen.rs:501-504`).

## 8. Matriz de teste de rede (UI final)

Duas máquinas (ou `scripts/vm-test-lab.sh`), cliente ativo, `LAN_MOUSE_LOG_LEVEL=debug`.
Simulação: `nft add rule inet filter output udp dport 4242 drop` (ida), `… input …` (volta),
`tc qdisc add dev <if> root netem loss 20% delay 50ms` / `tc qdisc del dev <if> root`.
Coluna “UI final” = comportamento verificado pelo código/teste atual; “Falta” = fase 2 (§9).

| # | Cenário | Simulação | UI final (correto) | Falta (fase 2) |
|---|---|---|---|---|
| 1 | Peer desligado | desligar máquina | `Connecting…` + “Trying \<target\> — no answer yet”; nunca “Connected” | motivo “sem resposta” |
| 2 | Lan Mouse fechado no peer | `kill` do daemon | idem (falha rápida via ICMP porta inacessível) | motivo “verifique se está aberto” |
| 3 | Firewall drop saída | `nft … output … drop` | idem | motivo “sem resposta” |
| 4 | Firewall assimétrico (só ingress) | drop input | `Receiving` “Peer connected / Incoming session … — outgoing still being established”; nunca “Connected” de saída | dois indicadores explícitos |
| 5 | DNS falha | hostname `peer.invalid` | `Connecting…` “Trying \<host\>:porta — no answer yet” (não afirma aprovação) | “DNS não resolveu” (`resolving`/`ips` ou `last_error`) |
| 6 | Porta errada | porta 1 no config | idem, mostrando `:1` no target | motivo “porta sem resposta” |
| 7 | Cert recusado / pin mismatch | trocar cert do peer / pin errado | `Connecting…` sem afirmação de causa; na máquina que recusa: pendência/`CertificateChanged` | motivo “certificado recusado” |
| 8 | Aprovação pendente no peer | peer sem o fingerprint | `Connecting…` + tooltip “if it shows a pending request, approve it there”; na máquina que aprova: Requests + Review; após aprovar, “Connected” em 2-3 s | estado `WaitingApproval` explícito |
| 9 | Cabo/Wi-Fi cai (100% loss) | desplug / `loss 100%` | até o watchdog: “Connected” (stale); depois `Connecting…` (não existe “Reconnecting”); input solto após `connect_grace_ms`; reconecta sozinho | frescor/last-seen para avisar antes |
| 10 | Perda parcial + latência (10-30%, 50-100 ms) | `netem loss 20% delay 50ms` | permanece “Connected”; RTT sobe; sem falso estado de queda (4 Pings/rodada) | indicador de qualidade/idade do RTT |
| 11 | Entrada-only | desativar saída; peer ativo | `Receiving`: “Peer connected” + “Incoming session from \<addr\>”; **nunca** “receiving input” | `remote_entered` p/ mostrar input aplicado |
| 12 | Bidirecional | ambos ativos/autorizados | `ConnectedBidirectional`: badge “Connected” + “(bidirectional)”; Enter/Leave não derruba | dois indicadores por direção |
| 13 | Emulação desligada no peer | desligar emulação no outro PC | `ConnectedDegraded` âmbar: “Connected · … — emulation disabled on the other PC”; sem spinner (link vivo) | link para diagnóstico/emulação no peer |
| 14 | Peer legado/upstream (write-only) | release upstream no peer | “Connected” otimista (addr+alive otimistas) por até ~60 s; depois `Connecting…` | frescor p/ “sem confirmação” após ~6 s |
| 15 | Quiosque no receptor | `accept_incoming=false` | peer: `Connecting…` (sem pendência); nossa UI sem aviso | `accept_incoming` no `Diagnostics` (“modo quiosque”) |
| 16 | Revogado reconectando | revogar e reiniciar peer | peer: `Connecting…`; na nossa UI não deveria virar pendência nova | filtrar `denied` em `pending_upsert` |
| 17 | Suspender/retomar | suspender >6 s | `Connected` stale → `Connecting…`; volta a “Connected” sem reiniciar | last-seen/frescor |
| 18 | Multihomed / IPv6 | peer conecta de IP diferente | pode ficar em `Connecting…` mesmo com sessão de entrada real | entrada por fingerprint |

## 9. Implementado vs recomendado (fase 2)

**Implementado agora** (UI final na working tree; sem commit; testes em `client_row.rs:558-833`):

- 6 estados derivados **só** das propriedades publicadas pelo serviço (`row_connection_state`,
  `client_row.rs:103-126`) — a row não inventa link; `alive` documentado como “emulação do
  peer, não liveness” (`client_object.rs:69-72`); sem estado “Reconnecting” por design
  (`client_row.rs:17-22`).
- Badge/cor/spinner/botão/subtitle/tooltip conforme §5; “Connected” só para link estabelecido
  (testes `:665-680`), classe antiga sempre removida (`:682-694`), spinner quando há tentativa pendente
  (`:732-740`), ação única correta por estado (`:742-777`).
- `ConnectedDegraded` âmbar (“emulation disabled on the other PC”); `Connecting` “no answer
  yet” (sem afirmar aprovação); `Receiving` “Peer connected”/“Incoming session” e tooltip
  “input applied only while it enters your screen”.
- Switch de enable removido; um botão por estado (Connect / Cancel / Disconnect / Also
  connect) com tooltips honestos; CSS novo (`status-degraded`, `client-degraded`,
  `status-spinner`).

**Recomendado para fase 2 (não implementado):**

| Pendência | Onde | Efeito de não ter |
|---|---|---|
| `last_pong_at`/frescor (last-seen) | serviço + IPC (`§6`) | “Connected” stale 2–6 s pós-queda e até ~60 s p/ peer write-only; UI não rebaixa p/ “sem confirmação” |
| `ConnectionPhase` opcional + `last_error` (`NoAnswer`/`DnsFailed`/`Refused`/`CertificateMismatch`) | serviço + IPC | “no answer yet” não distingue offline × firewall × DNS × cert × aprovação; sem `WaitingApproval`/`Error(motivo)` |
| `remote_entered` (`Enter`/`Leave`) | serviço + IPC | `Receiving` nunca pode afirmar input aplicado; serviço não sabe que o peer saiu (`emulation.rs:264-267`) |
| Devices panel ao vivo + derivação única (R8) | `window.rs:79-105`, `service.rs:1148-1163`, `:326-364` | painel mostra “Receiving/Connected/Offline” otimista/estale e diverge da linha; toast “device connected” (`lib.rs:437-442`) antes de `Pong` |
| Entrada por fingerprint | `service.rs:723,733,861` | multihomed/NAT/dual-stack podem não exibir sessão de entrada real |
| Fechar DTLS em deactivate/revoke + corridas/cancelar handshake | `client.rs:85-97`, `connect.rs:181-193`, `:855`, `:421-423`, `:600-631` | estado pode ser repovoado/limpo por conexão antiga; “Cancel” pode conectar depois do cancelamento |
| Medidor de expiração de “no answer” | frontend (item 7.2) + IPC | usuário não sabe há quanto tempo está tentando nem se vale tentar de novo |

Pendências de serviço listadas em §4 (R4–R7, R9, R10) continuam abertas; a UI final apenas
não as mascara mais na linha de cliente.

## 10. Segunda rodada — perguntas abertas

O único comando é `FrontendRequest::Activate(handle, bool)` (`lan-mouse-ipc/src/lib.rs:706`):
não existe “cancelar esta tentativa mantendo habilitado” nem “retirar o pedido pendente que
deixei no peer”. Pontos a decidir:

1. **`Connecting`**: “Cancelar” deve desabilitar (`Activate(false)`) ou só parar a tentativa?
   Só a 1ª é implementável hoje — e ela **mente** enquanto não houver a guarda de `is_active`
   (§7-4): a tentativa em voo conecta depois do cancelamento (`connect.rs:600-631`).
   Recomendação: `Activate(false)` = desabilitar, com guarda no serviço; “Tentar de novo” só
   em `Error`.
2. **Só-entrada**: “Cancelar” com `Receiving && active` (`row_action`,
   `client_row.rs:173-183`) desliga o cliente inteiro; o tooltip **promete** que “an incoming
   connection stays active” (`client_row.rs:157`) — hoje isso vale porque deactivate não limpa
   `incoming_addr` nem derruba o listener. Decidir se a row deve oferecer também “encerrar a
   sessão de entrada” (revogação do peer) sem remover o device.
3. **`Reconnecting` deixou de existir**: “Disconnect” durante `Connecting` = desabilitar. A UI
   não mostra tentativas/backoff (250 ms→5 s). Vale expor “tentando há Xs / próxima tentativa”
   ou é ruído?
4. **Pendência no outro PC**: após cancelar, o pedido continua listado lá
   (`PendingRequests`) até TTL/aprovação; não há mensagem para retirá-lo. Aceitamos TTL curto
   no pending do listener ou adicionamos `AuthCancel` aditivo (só p/ peers com `PERMISSIONS`)?
5. **`ConnectedDegraded`**: o botão é “Disconnect” (reconectar não adianta — a emulação está
   off no peer). Deve virar aviso com ação “diagnóstico”/“habilitar emulação no outro PC” em
   vez de desconectar? (Hoje o subtitle já explica o motivo.)
