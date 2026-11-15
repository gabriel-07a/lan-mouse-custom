# Simulador de mesa (`desk_sim`)

Testa o handoff do cursor e o Ctrl+C/Ctrl+V entre PCs **sem mexer no mouse,
sem segundo PC e sem portal/hooks do SO**. Roda com `cargo test` em poucos
segundos e usa o código real do serviço.

Código: [`src/desk_sim/`](../../src/desk_sim/). Cenários:

- [`scenarios.rs`](../../src/desk_sim/scenarios.rs): handoff básico;
- [`os_scenarios.rs`](../../src/desk_sim/os_scenarios.rs): handoff com as
  manias de GNOME e Windows;
- [`clipboard_scenarios.rs`](../../src/desk_sim/clipboard_scenarios.rs):
  copiar e colar texto, imagem e cópias com vários formatos.

## Para que serve

A maior parte dos bugs recorrentes do catálogo ([regressions.md](regressions.md):
R-01, R-03, R-15, R-16, R-23, R-25, os loops de `LeaveAt`) está na lógica de
quem controla o cursor, quando devolve e em que ponto entra. Essa lógica é
independente do SO. Antes, a única forma de ver o comportamento de ponta a
ponta era abrir dois PCs e arrastar o mouse até a borda.

O simulador faz esse teste manual automaticamente: monta uma "mesa" com PCs
virtuais, move um mouse simulado, copia e cola, e verifica o que uma pessoa
veria olhando os monitores: em que tela o cursor está, em que posição e o que
um Ctrl+V cola.

## Como funciona

```
 Desk::move_mouse                                   ScreenState de B (x, y)
       │                                                    ▲
 [SimCapture] → Service A (real) ──DTLS loopback──► Service B (real) → [SimEmulation]
       ▲                                                    │
 ScreenState de A (x, y)  ◄────────── LeaveAt ──────────────┘

 Desk::copy ─► clipboard em memória de A ─► ClipboardTask A (real) ══► ClipboardTask B (real) ─► clipboard de B ─► Desk::paste
```

| Peça | Máquina real | Simulador |
|---|---|---|
| Mouse | a mão do usuário | `Desk::move_mouse`: deltas de 4 px a cada 1 ms (mouse de 1000 Hz) |
| Monitor e cursor | tela física | `ScreenState`: largura, altura e posição do cursor |
| Backend de captura | libei, layer-shell, hooks do Windows | `SimCapture`: barreiras nas bordas da tela virtual |
| Backend de emulação | libei, wlroots, `SendInput` | `SimEmulation`: move o mesmo cursor e detecta o retorno pela borda |
| Clipboard do sistema | portal, Wayland, X11, Windows | `SharedClipboard` em memória, um por PC |
| Manias do SO | o próprio SO | `Personality`: `baseline`, `gnome`, `windows` |
| Rede | DTLS/UDP entre PCs | **DTLS/UDP real** em `127.0.0.1`, porta livre |
| `Service`, `CaptureTask`, listener, conexão, guards, `ReturnHandoff`, `ClipboardTask`, sync de clipboard | código real | **o mesmo código real** |

Cada PC roda o `Service` completo dentro do processo de teste, com a própria
pasta temporária (config, certificado, audit, socket IPC). O serviço sobe com
[`ServiceOptions`](../../src/service.rs) `isolated: true` e com backends
injetados, o que significa:

- sem beacon de discovery UDP, que anunciaria o teste na sua rede;
- sem o arquivo de cursor-hide do GNOME, que esconderia o **seu** cursor de
  verdade enquanto o teste roda;
- **sem portal de clipboard**: o clipboard é em memória. Com o backend real,
  cada PC simulado abria a janela "Área de trabalho remota" do GNOME na sua
  tela (veja [Popups do portal](#popups-do-portal-área-de-trabalho-remota));
- `trusted_ips = ["127.0.0.1"]`, então não aparece prompt de pareamento.

Os backends falsos entram pelas fábricas `CaptureFactory`, `EmulationFactory`
e `ClipboardFactory` (`src/capture.rs`, `src/emulation.rs`, `src/clipboard.rs`)
e por `InputCapture::from_backend` / `InputEmulation::from_backend`. A captura e
a emulação de um mesmo PC compartilham um `ScreenState`, como numa máquina
real, onde os dois backends mexem no mesmo cursor.

### O contrato das telas falsas (`Personality::baseline`)

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

### Manias de SO (`Personality`)

Cada campo descreve o que o backend do SO **entrega para o lan-mouse**, então
o que está sendo testado são os guards do próprio serviço. Os números vêm de
traces reais ([handoff-bidirectional.md](../analysis/handoff-bidirectional.md)
§4.2 e os modelos em `tests/qa_handoff_bidirectional.rs`).

| Personalidade | Lado | Mania |
|---|---|---|
| `gnome()` | origem | **backswing de ativação**: logo depois do `BeginAt`, o compositor manda -2 px e -5 px de volta |
| `gnome()` | origem | **backswing pós-movimento**: depois do primeiro movimento do usuário, mais -17 px de volta |
| `gnome()` | receptor | **`Begin` com jitter**: enquanto o peer dirige, o movimento injetado a até 2 px de uma borda com barreira faz o portal de captura reportar `Begin` ali (no máximo um a cada 25 ms) |
| `windows()` | receptor | **cruzamento injetado visto pela captura**: os hooks enxergam o movimento injetado, então passar da borda de entrada gera um `Begin` de captura além do `EdgeReturn` da emulação |

Uso: `.personality("A", Personality::gnome())`. Cada mania que dispara aparece
no `report()` como `Quirk("...")`.

## Rodar

```sh
cargo test --lib desk_sim                       # todos os cenários
cargo test --lib desk_sim::os                   # só os cenários com manias de SO
cargo test --lib desk_sim::clipboard            # só clipboard
cargo test --lib -- --ignored desk_sim          # achados em aberto (falham hoje)

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
                .personality("A", Personality::gnome())
                .personality("B", Personality::windows())
                .start()                             // sobe tudo e espera o link
                .await;

            desk.move_mouse(1200.0, 0.0).await;      // empurra para a direita
            desk.wait_focus("B").await;              // o cursor apareceu em B?
            desk.move_mouse(-1500.0, 0.0).await;     // volta
            desk.wait_focus("A").await;

            desk.copy_text("A", "olá");              // Ctrl+C em A
            desk.wait_paste("B", &texto("olá")).await; // Ctrl+V em B cola "olá"?
        })
        .await;
}
```

### API

| Chamada | O que faz |
|---|---|
| `Desk::builder().pc(nome, w, h)` | adiciona um PC com um monitor. O primeiro é o PC da "mão". |
| `.place(alvo, lado, de)` | `alvo` fica em `lado` de `de`, igual ao que se configura na GUI (vira um `[[clients]]` em `de`) |
| `.personality(pc, Personality::gnome())` | manias de SO do PC (padrão: `baseline`) |
| `.config(pc, toml)` | TOML extra no `config.toml` do PC, em tabelas inteiras (ex.: `[clipboard]\nmax_image_bytes = 65536`) |
| `.hand(Hand { step_px, interval })` | ritmo do mouse (padrão: 4 px a cada 1 ms) |
| `.start().await` | sobe os serviços e espera todo link configurado ficar `alive` (até 10 s) |
| `move_mouse(dx, dy)` / `move_mouse_on(pc, dx, dy)` | move o mouse físico do PC da mão / de outro PC |
| `focus()` / `focus_from(pc)` | `Focus::On(pc)` = onde o cursor está visível; `Focus::Lost` = o PC da mão está capturado mas ninguém está sendo dirigido (o bug do "cursor sumiu") |
| `wait_focus(pc)` / `wait_focus_from(mão, pc)` | espera o foco chegar (até 5 s) |
| `copy(pc, snapshot)` / `copy_text(pc, texto)` / `copy_image(pc, png)` | Ctrl+C no PC |
| `paste(pc)` | o que um Ctrl+V no PC colaria agora |
| `wait_paste(pc, &esperado)` | espera o Ctrl+V do PC colar exatamente `esperado` (até 10 s) |
| `wait_until(descrição, timeout, cond)` | espera uma condição qualquer |
| `hold(descrição, duração, cond)` | garante que `cond` se manteve durante todo o intervalo (pega flapping e ping-pong) |
| `cursor(pc)` | posição do cursor na tela do PC |
| `pc(nome).capture_begins()` / `.remote_begins()` | quantas vezes o PC capturou / foi dirigido |
| `pc(nome).last_return()` / `.last_edge_return()` | onde o último release recolocou o cursor / a coordenada do último pedido de retorno |
| `pc(nome).clipboard.applied()` | cópias vindas de peers que o PC gravou no clipboard |
| `pc(nome).state()` | `ScreenState` completo, incluindo `log` com todos os `ScreenEvent` |
| `report()` | estado de todos os PCs, clipboard, últimos eventos com tempo, links e pasta de arquivos |

Toda espera tem prazo; não há `sleep` para sincronizar. Quando um prazo
estoura, a mensagem de pânico já traz o `report()`.

### Cuidado 1: o movimento é assíncrono

O movimento sai do PC A, passa pela rede e chega em B um pouco depois. Ler o
cursor de B logo depois de `move_mouse` pode pegar o valor no meio do caminho.
Compare com o que o próprio sistema registrou: por exemplo,
`last_edge_return()` em B é a coordenada que B mandou de volta, e
`last_return()` em A é onde A recolocou o cursor.

### Cuidado 2: "não aconteceu" precisa de tempo para acontecer

Um teste do tipo "isto **não** pode chegar em B" só vale se, sem a proteção,
aquilo chegaria dentro da janela do `hold`. O primeiro teste de limite de
imagem usava 9 MB com o limite padrão de 8 MB e passava mesmo com **todas** as
checagens de tamanho desligadas: 9 MB em pedaços de 1 KiB não terminam de
transferir em 800 ms. A versão atual configura um limite de 64 KiB e usa uma
imagem de 100 KiB, que chegaria rápido se não fosse barrada. Antes de confiar
num teste negativo, quebre a proteção e veja o teste falhar.

## Lendo uma falha

Exemplo ilustrativo do formato (não é de uma execução real):

```
desk-sim: timed out after 5s waiting for: A's cursor on A
focus: Lost
  A [gnome] 1920x1080 cursor=(1919, 540) captured=Some(Right) remote_edge=None ...
    clipboard: Text "olá" (applied from peers: 0)
    +   812 ms  CaptureBegin { edge: Right, along: 0.5 }
    +   812 ms  Quirk("activation backswing")
    client Right:41237 active=true alive=true addr=Some(127.0.0.1:41237) error=None
    files: /tmp/lan-mouse-test-XXXX
  B [windows] 2560x1440 cursor=(0, 719) captured=None remote_edge=None ...
    +   815 ms  RemoteBegin { edge: Left, along: Some(0.5) }
    +  1290 ms  EdgeReturn { edge: Left, along: 0.5 }
    +  1291 ms  RemoteEnd
```

Nesse exemplo, B pediu o retorno e encerrou o controle remoto, mas A continuou
capturado. O bug está no caminho `LeaveAt` → captura de A. Rode de novo com
`LAN_MOUSE_LOG_LEVEL=debug ... -- --nocapture` para ver os logs `handoff:` dos
dois serviços intercalados.

## Cenários atuais

### Handoff básico (`scenarios.rs`)

| Cenário | O que garante |
|---|---|
| `cross_to_the_right_and_back` | entra em B na altura proporcional (1080 → 1440), volta para A na borda certa e na altura em que saiu de B, sem ping-pong por 500 ms |
| `repeated_round_trips_stay_clean` | 5 idas e voltas seguidas sem acumular estado (serials, guards, retornos pendentes) |
| `cross_upwards_and_back_with_a_slow_hand` | layout vertical (bordas de cima e de baixo), mão lenta de 1 px por evento |
| `the_return_goes_to_the_peer_that_drives` | 3 PCs (A \| B \| C), A e C dirigem B um de cada vez. Cada volta vai para quem estava dirigindo, e o outro não é puxado |

### Manias de SO (`os_scenarios.rs`)

| Cenário | O que garante |
|---|---|
| `gnome_to_windows_rests_at_the_border_without_churn` (e as outras 3 combinações) | o usuário cruza 4 px e para: o cursor fica em B por 600 ms, volta para A uma vez só, B nunca passa a dirigir A e a captura de B não fica presa |
| `mixed_round_trips_stay_clean` | 4 idas e voltas GNOME → Windows sem acumular estado |
| `a_gnome_receiver_returns_with_the_coordinate` | **`#[ignore]`, achado em aberto R-27** (veja abaixo) |

### Clipboard (`clipboard_scenarios.rs`)

| Cenário | O que garante |
|---|---|
| `copied_text_pastes_on_the_other_pc_in_both_directions` | texto com acentos e emoji vai de A para B e de B para A (B só tem a sessão de entrada de A) |
| `copied_image_pastes_byte_for_byte` | imagem de 700 KiB (cerca de 700 pedaços na rede) chega idêntica |
| `a_rich_copy_keeps_every_format` | uma cópia com texto + HTML + PNG chega com todos os formatos |
| `the_latest_copy_wins` | duas cópias seguidas (imagem e depois texto): cola a última, mesmo correndo com a transferência da primeira |
| `the_receiver_refuses_an_image_over_its_limit` | o receptor recusa uma imagem acima do próprio limite, mantém o que tinha, e a cópia seguinte passa normalmente |
| `the_sender_does_not_offer_an_image_over_its_limit` | o emissor nem oferece uma imagem acima do próprio limite, e a cópia seguinte passa normalmente |

## Ele pega bug? (verificação por mutação)

Para cada grupo de cenários, quebrei o código de produção de propósito, rodei o
simulador e restaurei o código:

| Mutação | Resultado |
|---|---|
| `src/capture.rs`: trocar `release_at(pos, along)` por `release()` (o bug R-25: cursor volta sem a coordenada) | **pegou**: 2 cenários básicos falham |
| `src/capture.rs`: desligar o guard de backswing da origem | **pegou**: os 3 cenários com origem GNOME falham. Os cenários básicos, sem manias, **não** pegam: sem backswing, o guard não é exercitado |
| `input-clipboard`: `Limits::accepts` sempre `true` (o emissor não checa tamanho) | **pegou**: `the_sender_does_not_offer_an_image_over_its_limit` |
| `input-clipboard/src/transfer.rs`: desligar a checagem de tamanho do receptor | **pegou**: `the_receiver_refuses_an_image_over_its_limit` |
| `src/emulation.rs`: reportar o `EdgeReturn` pela borda oposta | **não pegou, e é o esperado**: B só guarda a sessão que está dirigindo agora, e `service_core::return_targets` devolve para ela quando só existe uma |
| `src/capture.rs`: `REMOTE_ENTRY_DWELL` = 0 (o receptor não ignora o jitter logo depois da entrada) | **não pegou, e é informação útil**: o guard da origem descarta o movimento para trás perto da borda, então o cursor de B nunca volta a menos de 2 px da borda dentro dos 300 ms. Com a build atual nos dois lados, a janela do receptor é proteção redundante; ela importa com uma origem antiga sem o guard (0.2.18), que o simulador não roda |

## Achado em aberto: R-27

Com `Personality::gnome()` no receptor, o simulador mostrou que o cursor pode
voltar para a origem **sem a coordenada de retorno**:

1. B (GNOME) está sendo dirigido por A há mais de 300 ms (`REMOTE_ENTRY_DWELL`);
2. o usuário leva o cursor até a borda de entrada de B;
3. o portal de captura reporta `BeginAt(along)` na barreira `EnterOnly`;
4. a `CaptureTask` repassa isso como take-back em `ICaptureEvent::CaptureBegin(handle)`,
   que **não carrega a coordenada**, e `src/service.rs` chama
   `request_handoff(addr, None)`;
5. esse pedido chega antes do `EdgeReturn` da emulação (que tem a coordenada),
   e A recoloca o cursor no ponto de entrada antigo.

`a_gnome_receiver_returns_with_the_coordinate` reproduz isso de forma
determinística: falhou em 5 de 5 execuções. Ele está marcado como `#[ignore]`
até a correção, e está registrado em [regressions.md](regressions.md). A
correção provável é levar o `along` do `BeginAt` no `ICaptureEvent::CaptureBegin`
até o `request_handoff`.

Ressalva: o achado depende do modelo de jitter do GNOME (`Begin` a até 2 px da
barreira). O comentário em `input-emulation/src/lib.rs` diz que o portal "não
reporta barreiras de forma confiável" para movimento injetado, então às vezes
ele reporta. Vale confirmar num Ubuntu real com `LAN_MOUSE_HANDOFF_TRACE=1`.

## Popups do portal "Área de trabalho remota"

**Sintoma:** durante sessões de desenvolvimento, janelas "Área de trabalho
remota" (com "Permitir acesso à área de transferência") apareciam e sumiam
sozinhas na tela.

**Causa:** todo `Service` cria um `ClipboardTask`, que chama
`input_clipboard::default_backend()`. No Wayland, isso abre o
`PortalClipboard`, uma sessão RemoteDesktop só de clipboard, e o GNOME mostra o
diálogo. Quem subia serviços durante o desenvolvimento:

- o simulador (um diálogo por PC simulado: o cenário de 3 PCs abria 3);
- os scripts `scripts/qa-two-instances.sh` e `scripts/qa-handoff-shutdown.sh`,
  que sobem daemons com captura e emulação `dummy`, mas com o clipboard real.

**Correção:** o simulador injeta um clipboard em memória
(`ServiceOptions::clipboard_backend`), e os scripts sobem o daemon com
`--no-clipboard`, que já existia: com o clipboard desligado, o `ClipboardTask`
espera e nunca cria o backend.

**Verificação:** `dbus-monitor` na sessão, filtrando chamadas para
`org.freedesktop.portal.Desktop`, com uma leitura de propriedade como controle
positivo (o monitor viu essa leitura):

- `cargo test --workspace` (930 testes): **0** chamadas `CreateSession`,
  `SelectDevices`, `Start` ou `RequestClipboard`;
- `scripts/qa-two-instances.sh dual`: **0** chamadas, cenário passou.

Para conferir de novo:

```sh
dbus-monitor --session "type='method_call',destination='org.freedesktop.portal.Desktop'" &
cargo test --workspace
# nenhuma linha com member=CreateSession deve aparecer
```

## Limitações

- **Relógio real.** O DTLS precisa dele, e o `Service` usa `std::time::Instant`.
  Por isso cada cenário leva de 0,2 s a 1 s, e as esperas são prazos. Um relógio
  virtual exige transporte em memória e tempo injetado (ver
  [handoff-state-machine.md](../design/handoff-state-machine.md), §3.1).
- **Manias são modelos.** As personalidades reproduzem o que os traces
  mostraram, não o SO inteiro. Faltam, por exemplo: DPI misto, várias telas por
  PC, a emulação do Windows reportando overshoot em qualquer borda (R-01) e o
  receptor antigo 0.2.18.
- **A mão do PC dirigido não é simulada.** Mover o mouse físico de B enquanto
  A dirige B (o take-back deliberado) ainda não existe.
- **Clipboard em memória.** A conversão de formato de cada SO (PNG ↔ DIB no
  Windows, MIME do portal) não passa pelo simulador; ela tem testes próprios em
  `input-clipboard`. O simulador cobre o sync, a transferência e os limites.
- **Teclado e botões** chegam na emulação (`ScreenState::injected_other`), mas
  nenhum cenário verifica isso ainda.
- **Portas.** A porta livre é reservada e liberada antes de o listener fazer
  bind, então existe uma janela pequena de corrida. O listener faz bind em
  `0.0.0.0` durante o teste. Só `127.0.0.1` é confiável, então nenhum outro
  host da rede conecta sem aprovação.
- **Só Unix.** O socket IPC por caminho (`AsyncFrontendListener::new_at`) não
  existe no Windows.

## Próximos passos sugeridos

1. **Corrigir o R-27** e tirar o `#[ignore]` do cenário.
2. **Replay de trace:** transformar um JSONL de
   [handoff-trace.md](handoff-trace.md) gravado num bug real em roteiro de
   `move_mouse`, para que o bug achado na mão vire teste permanente.
3. **Take-back e peer caindo:** a mão em B durante o controle remoto, e
   derrubar o serviço de um PC no meio do handoff ou de uma transferência de
   clipboard (R-15, R-23).
4. **Teclado:** teclas e modificadores presos quando o handoff acontece com
   uma tecla segurada.
5. **Relógio virtual**, depois da migração para as máquinas de estado puras.
