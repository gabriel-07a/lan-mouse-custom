# Handoff como máquina de estados explícita — mapa e proposta

Status: **proposta** (nenhum código de produção alterado). Data: 2026-11-14.
Base: `main` em `74fc6cb` (0.2.19). Complementa
[`docs/analysis/handoff-bidirectional.md`](../analysis/handoff-bidirectional.md),
que explica *por que* os guards existem; este documento trata de *onde* o
estado vive e de como organizá-lo.

## 1. Problema

O handoff de borda (o cursor atravessando de uma máquina para a outra e
voltando) não tem um tipo que o represente. O estado está espalhado em três
tasks que conversam por canais, e cada uma guarda uma parte dele (às vezes uma
cópia da parte da outra):

| Onde | Campos / estruturas | Papel |
|---|---|---|
| `src/capture.rs` — `CaptureTask` | `active_client`, `state` (`WaitingForAck`/`Sending`), `enter: EnterState` (serial, deadline, retry), `enter_along`, `last_enter_pair`, `disconnected_since` | origem: Enter/Ack/retransmissão |
| idem | `residual_reverse_px`, `reverse_guard_until`, `handoff_inward_px`, `handoff_leading_until`, `handoff_begin_at` | origem: guard de resíduo reverso |
| idem | `remote_peer`, `remote_peer_since`, `enter_only_take_back_sent` | **receptor** (cópia do estado do service) |
| `src/emulation.rs` | `EnterDeduper` (último serial por addr), `pending_remote` do proxy | receptor: dedupe de Enter, arm do backend |
| `input-emulation` (`libei.rs`, `windows.rs`, `absolute.rs`) | `AbsoluteTracker` (âncora, `RETURN_MARGIN_PX`, `EDGE_ANCHOR_INSET`), `EdgeReturnLatch` (throttle 250 ms, 3 tentativas) | receptor: detecção de "empurrou para fora" |
| `src/service.rs` — `Service` | `incoming_conns`, `incoming_conn_info`, `incoming_peer_caps`, `return_requests: ReturnHandoff`, `return_serial`, `cursor_hide` | receptor: pedidos de retorno, sessões entrantes |

A sequência recente de commits (`7fa05a9`, `8aebfdf`, `a3e1ad8`, `ac728e5`) é
o sintoma disso: cada correção acrescenta um campo e um caso especial, e as
interações entre os campos só aparecem nos testes de integração
`tests/qa_handoff_*` (de 1 mil a 1,8 mil linhas cada).

### Achados concretos do mapeamento

1. **Quatro campos para um conceito só.** O guard de resíduo é um estado
   ("assentando, com X px para dentro, até T" ou "assentado"), mas usa quatro
   `Option`s que precisam ser setados e limpos juntos (`src/capture.rs:1172-1180`,
   `:1271-1340` e `release_capture` em `:1667`). Esquecer um deles em algum
   caminho já é um bug possível.
2. **Parte da condição é código morto.** `near_edge && (fixed_window ||
   position_guard)` (`src/capture.rs:1272`): as duas janelas começam no mesmo
   `Begin`, uma dura 200 ms e a outra 250 ms, então `fixed_window` implica
   `position_guard`. `reverse_guard_until` e `residual_reverse_px` só servem
   de telemetria.
3. **Estado espelhado entre tasks.** A verdade sobre "alguém está me
   controlando" mora no service (`incoming_conns`). A `CaptureTask` mantém uma
   cópia (`remote_peer`) via `CaptureRequest::SetRemotePeer`, mais o gate de
   take-back (`remote_peer_since`, `enter_only_take_back_sent`), que é lógica
   do papel *receptor* rodando dentro da task da *origem*. Se uma mensagem
   atrasar ou sumir, as cópias divergem.
4. **Deduplicação em camadas.** Um take-back passa por três filtros: o gate
   "uma vez por sessão" na capture (`should_report_enter_only_begin`), o envio
   duplo em `request_handoff` (`src/service.rs:1482-1483`) e o retry com
   budget em `ReturnHandoff` (300 ms, 8 tentativas). Cada camada foi uma
   correção pontual, e nenhuma delas sabe das outras.
5. **A exclusão mútua vem de checagens, não de tipos.** A regra do AGENTS.md
   ("um client está ativo ou inativo, nunca os dois") hoje é garantida por
   `is_suppressed_remote_begin` (`src/capture.rs:1099`). Nada impede que
   `active_client` e `remote_peer` estejam setados ao mesmo tempo.
6. **`handle_capture_event` faz tudo** (~450 linhas): filtro de capability,
   gate de take-back, início do handoff, escala de sensibilidade, guard,
   coalescing, trace, envio e tratamento de falha de rede.

## 2. Os estados que já existem (implícitos)

Do ponto de vista de **uma** máquina, existem estes estados:

```text
                    LocalBegin(h, along)                 Ack(serial) ok
        ┌──────────────────────────────┐        ┌──────────────────────┐
        │                              ▼        │                      ▼
     ┌──────┐                   ┌──────────────┐                 ┌──────────────────┐
     │ Idle │◄──────────────────│ Outgoing     │                 │ Outgoing         │
     └──────┘  timeout 2s /     │ AwaitingAck  │                 │ Driving          │
        ▲  ▲   send fail /      │ {h,serial,   │                 │ {h, guard}       │
        │  │   release bind     │  along}      │                 │ guard: Settling  │
        │  │                    └──────────────┘                 │   {inward,until} │
        │  │                                                     │      | Settled   │
        │  └─────────────────────────────────────────────────────┤                  │
        │      Leave(serial≠0) / LeaveAt / Leave(0) de peer legado│                  │
        │      grace expirou / release bind / Destroy            └──────────────────┘
        │
        │ PeerLeft (Leave do peer)              PeerEnter(addr,pos,serial,along)
        │ / Disconnected / retry esgotado            │
     ┌──┴──────────────────┐                         ▼
     │ Incoming Returning  │◄── EdgeReturn ── ┌──────────────────────┐
     │ {addr,serial,along, │    ou take-back  │ Incoming Driven      │
     │  attempts}          │    (EnterOnly    │ {addr, edge, since,  │
     └─────────────────────┘     Begin após   │  take_back_sent}     │
                                 dwell 300ms) └──────────────────────┘
```

`Outgoing*` e `Incoming*` são mutuamente exclusivos; hoje isso é só uma
convenção.

### Eventos de entrada

| Evento | Origem atual |
|---|---|
| `LocalBegin { handle, along }` | backend de captura (`CaptureEvent::Begin/BeginAt`) |
| `LocalMotion { dx, dy }` | backend de captura |
| `IncomingCaptureBegin { handle }` | backend de captura em zona `EnterOnly` |
| `PeerAck(serial)`, `PeerLeave(serial)`, `PeerLeaveAt { serial, along }` | `conn.recv()` em `do_capture_session` |
| `PeerEnter { addr, pos, serial, along }` | `handle_enter` em `emulation.rs` |
| `PeerLeft { addr }` | `ProtoEvent::Leave` no listener |
| `EdgeReturn { pos, along }` | backend de emulação (`EdgeReturnNotifier`) |
| `Tick(now)` | `enter_retry` (25 ms) e `retry_return_requests` |
| `SendFailed`, `Disconnected { addr }`, `ReleaseBind`, `Destroy(handle)` | rede, teclas, frontend |

### Ações de saída

`SendEnter{serial,along}`, `SendAck(serial)`, `SendLeave{serial,along}`,
`ReleaseCapture{along}`, `ForwardMotion` / `DropMotion`, `ArmEmulation{edge,along}`,
`EndRemoteControl`, `CursorPark` / `CursorUnpark`, `ReleaseKeys`, `Notify(...)`.

## 3. Proposta

### 3.1 Duas máquinas puras ("sans-IO"), uma por papel

Uma máquina única, dona de tudo, dentro do service seria o desenho mais limpo
no papel. Mas ela obrigaria todo `Motion` a passar pelo canal do service, o
que soma latência no caminho quente. Proposta:

- **`handoff::Outgoing`** (papel origem) — fica com a `CaptureTask`.
  Absorve `active_client`, `state`, `EnterState`, `enter_along`,
  `last_enter_pair` e os quatro campos do guard, que viram um enum só:

  ```rust
  enum Guard { Settling { inward_px: f64, until: Instant }, Settled }
  enum Outgoing {
      Idle,
      AwaitingAck { handle, serial, along, deadline, next_retry, guard: Guard },
      Driving { handle, along, guard: Guard },
  }
  impl Outgoing {
      fn on(&mut self, now: Instant, ev: OutEvent, peer: PeerCaps) -> Actions;
      fn filter_motion(&mut self, now: Instant, dx: f64, dy: f64, edge: Position) -> MotionVerdict;
  }
  ```

  `filter_motion` é síncrono, sem alocação e sem canal: o caminho quente
  continua igual.

- **`handoff::Incoming`** (papel receptor) — fica com o `Service`.
  Absorve `return_requests`, `return_serial`, o gate de take-back (que sai da
  `CaptureTask`) e a decisão de `remote_peer`. A capture passa a só reportar
  todo `Begin` em `EnterOnly`; quem decide é a máquina. O espelho que sobra na
  capture é um único `Option<Position>` derivado, enviado pela máquina como
  ação, nunca escrito por mais ninguém.

- **Exclusão mútua.** O service chama `Incoming::on(PeerEnter)`, e a ação
  resultante inclui `ReleaseCapture`. A `Outgoing` só aceita `LocalBegin` se o
  `remote_peer` derivado for `None`. A regra continua dividida entre as duas
  máquinas, mas em um ponto só de cada lado e coberta por teste de
  propriedade (§3.3).

- **Tempo injetado.** Toda função recebe `now: Instant`, e nenhuma chama
  `Instant::now()` ou `tokio::time`. Os testes ficam determinísticos, sem
  `sleep` e sem runtime.

- **Trace e logs** saem das máquinas como dados (`Action::Trace(...)`), e o
  `handoff_trace` continua sendo chamado pela task. Isso tira de
  `handle_capture_event` uma boa parte das linhas, que hoje são só trace.

### 3.2 Compatibilidade com peers antigos

Os ramos de compatibilidade viram parâmetros explícitos (`PeerCaps`) em vez de
`if`s espalhados:

| Capability | Ramo legado que existe hoje |
|---|---|
| `ENTER_ACK` ausente | pula `AwaitingAck` e vai direto para `Driving` |
| `SERIALIZED_LEAVE` ausente | `Leave(0)` é pedido de release, não confirmação |
| `RETURN_COORDINATE` / edge coordinate ausente | `Enter` em vez de `EnterAt`, `along = None` |

**Decisão pendente:** se o fork passar a exigir versão mínima no handshake
(`39bd452` já valida versão/build), esses ramos e os testes
`qa_handoff_old_receiver*.rs` podem ser apagados antes da migração, e as
máquinas ficam bem menores. Isso fere a regra "aditivo ou gated" do AGENTS.md
só no sentido de recusar peers velhos, não de quebrar o formato de wire.

### 3.3 Testes

- Os testes unitários de `enter_state_tests` e os de guard em
  `capture_handle_tests` (`src/capture.rs:1851+`, `:2387-2720`) migram para
  testes puros da `Outgoing`, sem `FakeConnection` e sem tokio.
- **Teste de propriedade** (proptest) com sequências aleatórias de eventos
  e tempo, verificando invariantes:
  1. nunca `Outgoing ≠ Idle` com `remote_peer = Some`;
  2. todo `AwaitingAck` termina em `Driving` ou `Idle` em até `ENTER_TIMEOUT`;
  3. no máximo um retorno pendente por `addr`, com serial não nulo e crescente;
  4. motion para fora nunca é encaminhada em `Settling` perto da borda, e
     motion para dentro nunca é descartada;
  5. `ReleaseCapture` sempre vem acompanhado de `ReleaseKeys`.
- Os `tests/qa_handoff_*.rs` continuam como testes de integração de ponta a
  ponta e não mudam, para servir de rede de segurança comportamental.

## 4. Plano de migração (cada passo compila e passa nos testes)

| Passo | Conteúdo | Risco |
|---|---|---|
| 0 | Este documento + decisão sobre peers antigos | — |
| 1 | `src/handoff/outgoing.rs`: extrair `EnterState` e o guard (enum `Guard`), remover a parte morta da condição (achado 2). A `CaptureTask` delega. Migrar os testes unitários. | baixo: refactor local, testes existentes cobrem |
| 2 | `Outgoing` passa a ser dona de `active_client`/`state`; `handle_capture_event` vira "traduz evento → `on()` → executa ações". | médio: caminho quente; medir com `LAN_MOUSE_HANDOFF_TRACE` antes e depois |
| 3 | `src/handoff/incoming.rs`: `ReturnHandoff`, `return_serial` e o gate de take-back saem de `service.rs`/`capture.rs`; unificar as três camadas de dedupe (achado 4). | médio: muda o local de uma decisão entre tasks |
| 4 | Teste de propriedade com as invariantes da §3.3. | baixo |
| 5 | (Se decidido) remover os ramos de peer legado. | baixo depois dos passos 1–4 |

Nenhum passo muda o protocolo de wire. Os passos 2 e 3 exigem rodar a matriz
manual T1–T5 de `docs/analysis/handoff-bidirectional.md` §7 no setup real
Ubuntu ↔ Windows antes do merge.

### Fora de escopo

- Os limiares dos backends (`RETURN_MARGIN_PX`, `EDGE_RETURN_MIN_OVERSHOOT`,
  `ENTRY_ANCHOR_MARGIN`) continuam nos backends: são geometria específica de
  cada SO, não estado do handoff.
- `cursor_hide` continua no service; a máquina só emite `CursorPark`/`Unpark`.
