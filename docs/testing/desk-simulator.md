# Simulador de mesa (`desk_sim`)

Testa o handoff do cursor entre PCs **sem mexer no mouse, sem segundo PC e sem
portal/hooks do SO**. Roda com `cargo test`, leva cerca de 1 s por cenário e
usa o código real do serviço.

Código: [`src/desk_sim/`](../../src/desk_sim/). Cenários:
[`src/desk_sim/scenarios.rs`](../../src/desk_sim/scenarios.rs).

## Para que serve

A maior parte dos bugs recorrentes do catálogo ([regressions.md](regressions.md):
R-01, R-03, R-15, R-16, R-23, R-25, os loops de `LeaveAt`) está na lógica de
quem controla o cursor, quando devolve e em que ponto entra. Essa lógica é
independente do SO. Antes, a única forma de ver o comportamento de ponta a
ponta era abrir dois PCs e arrastar o mouse até a borda.

O simulador faz esse teste manual automaticamente: monta uma "mesa" com PCs
virtuais, move um mouse simulado e verifica o que uma pessoa veria olhando os
monitores, ou seja, em que tela o cursor está e em que posição.

## Como funciona

```
 Desk::move_mouse                                   ScreenState de B (x, y)
       │                                                    ▲
 [SimCapture] → Service A (real) ──DTLS loopback──► Service B (real) → [SimEmulation]
       ▲                                                    │
 ScreenState de A (x, y)  ◄────────── LeaveAt ──────────────┘
```

| Peça | Máquina real | Simulador |
|---|---|---|
| Mouse | a mão do usuário | `Desk::move_mouse`: deltas de 4 px a cada 1 ms (mouse de 1000 Hz) |
| Monitor e cursor | tela física | `ScreenState`: largura, altura e posição do cursor |
| Backend de captura | libei, layer-shell, hooks do Windows | `SimCapture`: barreiras nas bordas da tela virtual |
| Backend de emulação | libei, wlroots, `SendInput` | `SimEmulation`: move o mesmo cursor e detecta o retorno pela borda |
| Rede | DTLS/UDP entre PCs | **DTLS/UDP real** em `127.0.0.1`, porta livre |
| `Service`, `CaptureTask`, listener, conexão, guards, `ReturnHandoff` | código real | **o mesmo código real** |

Cada PC roda o `Service` completo dentro do processo de teste, com a própria
pasta temporária (config, certificado, audit, socket IPC). O serviço sobe com
[`ServiceOptions`](../../src/service.rs) `isolated: true`, o que significa:

- sem beacon de discovery UDP, que anunciaria o teste na sua rede;
- sem o arquivo de cursor-hide do GNOME, que esconderia o **seu** cursor de
  verdade enquanto o teste roda;
- `trusted_ips = ["127.0.0.1"]`, então não aparece prompt de pareamento.

Os backends falsos entram pelas fábricas `CaptureFactory`/`EmulationFactory`
(`src/capture.rs`, `src/emulation.rs`) e por `InputCapture::from_backend` /
`InputEmulation::from_backend`. A captura e a emulação de um mesmo PC
compartilham um `ScreenState`, como numa máquina real, onde os dois backends
mexem no mesmo cursor.

### O contrato das telas falsas (sem manias de SO)

- **Captura:** o movimento local move o cursor. Se o cursor passa de uma borda
  com barreira, sai `BeginAt(along)` (`along` = posição normalizada ao longo da
  borda, de 0.0 a 1.0). A partir daí, o movimento local vira `Input` para o
  peer e o cursor local fica parado até `release`/`release_at`. O `release_at`
  recoloca o cursor na borda, na coordenada de retorno.
- **Emulação:** `begin_remote_control(edge, along)` coloca o cursor no ponto de
  cruzamento (no centro da borda se não vier coordenada). O movimento injetado
  é limitado à tela. Empurrar para além da borda de entrada dispara o pedido de
  retorno, com o mesmo throttle dos trackers reais (250 ms, no máximo 3 por
  contato).

## Rodar

```sh
cargo test --lib desk_sim                       # todos os cenários
cargo test --lib cross_to_the_right_and_back    # um cenário

# com os logs do serviço e da tela virtual
LAN_MOUSE_LOG_LEVEL=debug cargo test --lib desk_sim -- --nocapture
```

O módulo só compila em testes Unix (`#[cfg(all(test, unix))]`, por causa do
socket IPC por caminho).

## Escrever um cenário

```rust
#[tokio::test(flavor = "current_thread")]
async fn meu_cenario() {
    init_log();
    LocalSet::new()                 // os serviços usam spawn_local
        .run_until(async {
            let desk = Desk::builder()
                .pc("A", 1920, 1080)                 // o primeiro PC tem o mouse
                .pc("B", 2560, 1440)
                .place("B", Position::Right, "A")    // B fica à direita de A
                .start()                             // sobe tudo e espera o link
                .await;

            desk.move_mouse(1200.0, 0.0).await;      // empurra para a direita
            desk.wait_focus("B").await;              // o cursor apareceu em B?
            desk.move_mouse(-1500.0, 0.0).await;     // volta
            desk.wait_focus("A").await;

            desk.hold("sem ping-pong", Duration::from_millis(500), |d| {
                d.focus() == Focus::On("A")
            })
            .await;
            assert_eq!(desk.pc("A").capture_begins(), 1, "{}", desk.report());
        })
        .await;
}
```

### API

| Chamada | O que faz |
|---|---|
| `Desk::builder().pc(nome, w, h)` | adiciona um PC com um monitor. O primeiro é o PC da "mão". |
| `.place(alvo, lado, de)` | `alvo` fica em `lado` de `de`, igual ao que se configura na GUI (vira um `[[clients]]` em `de`) |
| `.hand(Hand { step_px, interval })` | ritmo do mouse (padrão: 4 px a cada 1 ms) |
| `.start().await` | sobe os serviços e espera todo link configurado ficar `alive` (até 10 s) |
| `move_mouse(dx, dy)` / `move_mouse_on(pc, dx, dy)` | move o mouse físico do PC da mão / de outro PC |
| `focus()` / `focus_from(pc)` | `Focus::On(pc)` = onde o cursor está visível; `Focus::Lost` = o PC da mão está capturado mas ninguém está sendo dirigido (o bug do "cursor sumiu") |
| `wait_focus(pc)` / `wait_focus_from(mão, pc)` | espera o foco chegar (até 5 s) |
| `wait_until(descrição, timeout, cond)` | espera uma condição qualquer |
| `hold(descrição, duração, cond)` | garante que `cond` se manteve durante todo o intervalo (pega flapping e ping-pong) |
| `cursor(pc)` | posição do cursor na tela do PC |
| `pc(nome).capture_begins()` / `.remote_begins()` | quantas vezes o PC capturou / foi dirigido |
| `pc(nome).last_return()` / `.last_edge_return()` | onde o último release recolocou o cursor / a coordenada do último pedido de retorno |
| `pc(nome).state()` | `ScreenState` completo, incluindo `log` com todos os `ScreenEvent` |
| `report()` | estado de todos os PCs, últimos eventos com tempo, links e pasta de arquivos |

Toda espera tem prazo; não há `sleep` para sincronizar. Quando um prazo
estoura, a mensagem de pânico já traz o `report()`.

### Cuidado: o movimento é assíncrono

O movimento sai do PC A, passa pela rede e chega em B um pouco depois. Ler o
cursor de B logo depois de `move_mouse` pode pegar o valor no meio do caminho.
Compare com o que o próprio sistema registrou: por exemplo,
`last_edge_return()` em B é a coordenada que B mandou de volta, e
`last_return()` em A é onde A recolocou o cursor.

## Lendo uma falha

Exemplo ilustrativo do formato (não é de uma execução real):

```
desk-sim: timed out after 5s waiting for: A's cursor on A
focus: Lost
  A 1920x1080 cursor=(1919, 540) captured=Some(Right) remote_edge=None ...
    +   812 ms  CaptureBegin { edge: Right, along: 0.5 }
    client Right:41237 active=true alive=true addr=Some(127.0.0.1:41237) error=None
    files: /tmp/lan-mouse-test-XXXX
  B 2560x1440 cursor=(0, 719) captured=None remote_edge=None ...
    +   815 ms  RemoteBegin { edge: Left, along: Some(0.5) }
    +  1290 ms  EdgeReturn { edge: Left, along: 0.5 }
    +  1291 ms  RemoteEnd
```

Nesse exemplo, B pediu o retorno e encerrou o controle remoto, mas A continuou
capturado. O bug está no caminho `LeaveAt` → captura de A. Rode de novo com
`LAN_MOUSE_LOG_LEVEL=debug ... -- --nocapture` para ver os logs `handoff:` dos
dois serviços intercalados.

## Cenários atuais

| Cenário | O que garante |
|---|---|
| `cross_to_the_right_and_back` | entra em B na altura proporcional (1080 → 1440), volta para A na borda certa e na altura em que saiu de B, sem ping-pong por 500 ms |
| `repeated_round_trips_stay_clean` | 5 idas e voltas seguidas sem acumular estado (serials, guards, retornos pendentes) |
| `cross_upwards_and_back_with_a_slow_hand` | layout vertical (bordas de cima e de baixo), mão lenta de 1 px por evento |
| `the_return_goes_to_the_peer_that_drives` | 3 PCs (A \| B \| C), A e C dirigem B um de cada vez. Cada volta vai para quem estava dirigindo, e o outro não é puxado |

## Ele pega bug? (verificação por mutação)

Antes de confiar no simulador, quebrei o código de produção de propósito e
rodei os cenários:

| Mutação | Resultado |
|---|---|
| `src/capture.rs`: trocar `release_at(pos, along)` por `release()` (o bug R-25: cursor volta sem a coordenada) | **pegou**: 2 cenários falham com "A released without a return coordinate" |
| `src/emulation.rs`: reportar o `EdgeReturn` pela borda oposta | **não pegou, e é o esperado**: B só guarda a sessão que está dirigindo agora, e `service_core::return_targets` devolve para ela quando só existe uma. Pelo log, o serviço avisa "did not match the live edge; using the only incoming session" e devolve certo |

A segunda linha também é informação útil: ela mostra que, no uso com um
usuário, a borda reportada no retorno não decide nada, quem decide é o
fallback.

## Limitações (v1)

- **Relógio real.** O DTLS precisa dele, e o `Service` usa `std::time::Instant`.
  Por isso cada cenário leva cerca de 1 s e as esperas são prazos. Um relógio
  virtual exige transporte em memória e tempo injetado (ver
  [handoff-state-machine.md](../design/handoff-state-machine.md), §3.1).
- **Sem manias de SO.** As telas seguem só o contrato básico. As manias
  conhecidas ainda não estão modeladas: Windows disparando 1 a 3 px depois da
  borda, o warp residual de ativação do GNOME, `CaptureBegin` com jitter, a
  emulação do Windows reportando overshoot em qualquer borda (R-01).
- **A mão do PC dirigido não é simulada.** Mover o mouse físico de B enquanto
  A dirige B (o "take-back" via captura `EnterOnly`) ainda não existe.
- **Só movimento.** Teclado, botões, scroll e clipboard chegam na emulação
  (`ScreenState::injected_other`), mas nenhum cenário verifica isso ainda.
- **Portas.** A porta livre é reservada e liberada antes de o listener fazer
  bind, então existe uma janela pequena de corrida. O listener faz bind em
  `0.0.0.0` durante o teste. Só `127.0.0.1` é confiável, então nenhum outro
  host da rede conecta sem aprovação.
- **Só Unix.** O socket IPC por caminho (`AsyncFrontendListener::new_at`) não
  existe no Windows.

## Próximos passos sugeridos

1. **Personalidades de SO** no `ScreenState`, como `like_windows()` e
   `like_gnome()`, reaproveitando os modelos que hoje estão soltos em
   `tests/qa_handoff_bidirectional.rs` (`OldWindowsReceiver0218`, residual de
   ativação). Com elas, os bugs de churn bidirecional viram cenários.
2. **Replay de trace:** transformar um JSONL de
   [handoff-trace.md](handoff-trace.md) gravado num bug real em roteiro de
   `move_mouse`, para que o bug achado na mão vire teste permanente.
3. **Take-back e peer caindo:** a mão em B durante o controle remoto, e
   derrubar o serviço de um PC no meio do handoff (R-15, R-23).
4. **Relógio virtual**, depois da migração para as máquinas de estado puras.
