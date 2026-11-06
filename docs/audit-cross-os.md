# Auditoria cross-OS: matriz Linux/Windows 10/11/macOS e bugs por backend

> **TL;DR** — O caminho Windows está maduro (raw input + DPI lógico + absoluto
> determinístico), mas tem um **P0**: `send_input_safe` faz `loop` infinito quando `SendInput`
> retorna 0 (`input-emulation/src/windows.rs:290-300`), travando o `LocalSet` inteiro
> (captura, rede, IPC). **Key auto-repeat quebra quando o recebedor é Linux**: o dedupe em
> `input-emulation/src/lib.rs:156` descarta repetições e só Windows/macOS sintetizam repeat
> (`windows.rs:246-288`, `macos.rs:108-148`). **X11 não tem captura** (`input-capture/src/x11.rs:11-14`)
> apesar do README anunciar suporte — só emulação/clipboard; e clipboard X11 perde
> silenciosamente payloads >256 KiB (`input-clipboard/src/x11.rs:56-58,474-490`). macOS tem
> bug de bounds após hotplug (`input-capture/src/macos.rs:104-118`) e imagem só em
> `public.png` (`input-clipboard/src/macos.rs:99`). Foco Ubuntu↔Windows mais frágil:
> **Windows→Ubuntu (repeat)**, **janela elevada/UIPI (P0/P1)** e **DPI misto (hipótese)**.

Escopo: auditoria **somente leitura** de `main` @ `7e3e11c` + working tree (crates de backend
inalterados; `lan-mouse-gtk/` em redesign não afeta backends). Lidos: `input-capture/src/*`
(incl. `windows/event_thread.rs`, `windows/display_util.rs`), `input-emulation/src/*`,
`input-clipboard/src/*`, `input-event/src/scancode.rs`, `src/{main,capture,service,config,doctor}.rs`,
README/DOC.md, `scripts/vm-test-lab.sh` e `.github/workflows/*`. Outros agentes cobrem rede,
portais Ubuntu, TCC macOS, permissões Windows e estratégia; aqui o recorte é
**captura/emulação/clipboard por combinação**. Severidade: **P0** travamento/perda,
**P1** feature quebrada/input errado, **P2** degradado/latente, **P3** cosmético/doc.

## 1. O que cada OS seleciona de fato

Ordem de captura: `input-capture/src/lib.rs:372-393` (libei → layer-shell → X11 → Windows →
macOS; **sem fallback dummy** no auto). Emulação: `input-emulation/src/lib.rs:119-136`
(wlroots → libei → xdp → X11 → Windows → macOS → **Dummy** ao final: backend inexistente vira
log silencioso, nunca erro). Clipboard: `input-clipboard/src/lib.rs:358-398`.

| Sessão/OS | Captura efetiva | Emulação efetiva | Clipboard | Status real |
|---|---|---|---|---|
| Ubuntu GNOME/Wayland (45+) | libei (portal InputCapture v1) | libei (fallback xdp) | portal | ✓; prompt a cada daemon start (`README:806-810`); cursor local visível |
| Ubuntu KDE Wayland (6.1+) | libei | libei / xdp | portal | ✓ |
| Ubuntu wlroots (Sway/Hyprland) | layer-shell | wlroots | wlr-data-control | ✓; Wayfire exige `shortcuts-inhibit` (`README:59-62`) |
| Ubuntu X11 | **nenhuma (stub)** | X11 (XTest) | X11 (CLIPBOARD) | recebe ✓ / **não envia** ✗; clipboard >256 KiB perde |
| Windows 10/11 | windows (raw input + hooks) | windows (SendInput absoluto) | windows (CF_* + PNG) | ✓; P0 em `send_input_safe`, UIPI, repeat ok |
| macOS | macos (CGEventTap) | macos (CGEvent HID) | NSPasteboard | ✓ sob TCC; bounds/hotplug e imagem TIFF limitam |

## 2. Matriz host → cliente (captura do host × emulação do cliente)

Legenda: ✓ ok; ⚠ bug/limitação citada; ✗ não suportado. `R` = key repeat, `UIPI` = janela
elevada no cliente Windows, `clip` = x11>256 KiB, `TCC` = permissões macOS (outro agente).

| Host ↓ / Cliente → | Win10 | Win11 | Ubuntu GNOME | Ubuntu KDE | wlroots | Ubuntu X11 | macOS |
|---|---|---|---|---|---|---|---|
| **Ubuntu GNOME (libei)** | ✓ ⚠UIPI | ✓ ⚠UIPI | ⚠R | ⚠R | ⚠R | ⚠R ⚠clip | ✓ ⚠TCC |
| **Ubuntu KDE (libei)** | ✓ ⚠UIPI | ✓ ⚠UIPI | ⚠R | ⚠R | ⚠R | ⚠R ⚠clip | ✓ ⚠TCC |
| **Ubuntu wlroots (layer-shell)** | ✓ ⚠UIPI | ✓ ⚠UIPI | ⚠R | ⚠R | ⚠R | ⚠R ⚠clip | ✓ ⚠TCC |
| **Ubuntu X11** | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| **Windows 10/11** | ✓ ⚠UIPI | ✓ ⚠UIPI | **⚠R** | **⚠R** | **⚠R** | **⚠R** ⚠clip | ✓ ⚠TCC |
| **macOS** | ✓ ⚠TCC | ✓ ⚠TCC | ⚠R | ⚠R | ⚠R | ⚠R ⚠clip | ✓ ⚠TCC |

Leituras: (a) host X11 puro não envia para ninguém; (b) **R quebra quando o cliente é Linux**
(host Win/macOS reenvia repetições, o dedupe descarta e nenhum backend Linux sintetiza — F3);
(c) UIPI afeta qualquer host controlando Windows com janela elevada/QAC em foco (F1/F2; o
retorno de `SendInput` nesse caso precisa de validação — H1);
(d) receber no macOS exige TCC no alvo.

## 3. Achados (severidade, evidência, repro, correção)

### P0
- **F1 — loop infinito em `SendInput`.** `input-emulation/src/windows.rs:290-300` (runtime
  single-thread: `src/main.rs:142-148`): `loop { if SendInput(...) > 0 { break } }` sem
  sleep/limite. Qualquer retorno 0 — a doc da Microsoft diz que a função falha quando
  bloqueada por UIPI, sem explicar via `GetLastError`; há relatos de retorno >0 com injeção
  descartada *(validar em H1)* — trava o `LocalSet`: rede, IPC e watchdog param juntos e a UI
  congela a 100% de CPU. **Repro:** Ubuntu controla Windows, abrir UAC/instalador elevado e
  tentar mover/clicar. **Fix:** tentativa limitada com sleep/backoff, `EmulationError` após N
  falhas; nunca busy-loop no executor.

### P1
- **F2 — hooks não veem input destinado a processo elevado.** Hooks em
  `input-capture/src/windows/event_thread.rs:318-326`, callbacks `:660-713`. Por UIPI um
  processo medium-integrity não recebe input com o foreground elevated (UAC, regedit, shell
  admin): controlando Windows pelo Ubuntu, mouse/teclado congelam até sair da janela
  *(hipótese de SO; confirmar com H1)*. **Fix:** detectar foreground elevado e avisar no
  diagnóstico/toast; documentar execução elevada e ajustar atalho/instalador.
- **F3 — key auto-repeat quebrado para clientes Linux.** `InputEmulation::consume` descarta
  key-down repetido (`input-emulation/src/lib.rs:148-160` + `update_pressed_keys`
  `:224-236`); só Windows (`windows.rs:246-288`) e macOS (`macos.rs:108-148`) sintetizam
  repeat. `wlroots`, `libei`, `x11` e `xdp` não têm repeat, e layer-shell/libei capture não
  implementam `wl_keyboard.repeat_info` (`input-capture/src/layer_shell.rs:832-877`).
  **Repro:** Windows controla Ubuntu GNOME; segurar Backspace/seta → uma única tecla.
  **Fix:** repeat nos backends Linux (mesmo padrão de task do Windows) e/ou `repeat_info` no
  capture; testar nas 4 combinações do foco.
- **F4 — captura X11 é stub, docs dizem o contrário.** `input-capture/src/x11.rs:11-14`
  (`NotImplemented`; `error.rs:139-142`); README promete captura X11 (`README:45`, `:849`,
  `:873`, `:899`) e DOC.md nega (`DOC.md:172`). Efeito: sessão X11 só recebe; `CaptureStatus`
  fica Disabled sem explicação; `--capture-backend x11` falha. **Fix:** implementar (pointer
  barriers + XInput/XTEST) ou remover backend/feature/anúncio do README; até lá, avisar
  “receiving only”.
- **F5 — clipboard X11 perde payloads grandes nos dois sentidos.** Leitura: INCR é ignorado
  (`input-clipboard/src/x11.rs:56-58`, `:322-326`) — copiar de app que usa INCR não
  sincroniza. Escrita: `MAX_X11_PROPERTY = 256 KiB` e `reply_to_request` responde “não
  disponível” acima disso (`x11.rs:474-490`) — texto de 1 MiB vindo do Windows não cola em app
  X11 local. **Repro:** Windows→Ubuntu X11, copiar 512 KiB, colar → nada. **Fix:** INCR nos
  dois sentidos (ou fatiar) e reportar em `ClipboardStatus.last_error`.

### P2
- **F6 — DPI misto/multi-monitor no host Windows (hipótese).** Captura enumera monitores por
  `EnumDisplaySettingsW` (`event_thread.rs:754-796`) enquanto hooks entregam pixels físicos
  PMv2: `dmPosition` e `dmPels*` podem divergir em escalas diferentes, desalinhando
  barreiras/cursor pinado; a emulação já usa `EnumDisplayMonitors/GetMonitorInfo`
  (`input-emulation/src/windows.rs:335-357`), a captura não. O scale é fixado no `activate`
  (`event_thread.rs:498-503`) e não acompanha mudança de DPI durante a captura. **Fix:** usar
  `EnumDisplayMonitors` na captura, reconsultar em `WM_DISPLAYCHANGE`/`WM_DPICHANGED`; testar
  L com 100/150/200%.
- **F7 — raw input absoluto ignorado no Windows.** `event_thread.rs:629-633` descarta
  `MOUSE_MOVE_ABSOLUTE` (tablets, alguns trackpads, RDP/VM): motion pode congelar nesses
  dispositivos. **Fix:** converter absoluto em delta relativo ao último ponto; testar
  caneta/RDP.
- **F8 — bounds de captura no macOS só crescem.** `Bounds::default()` + `update_bounds` só faz
  `min/max` (`input-capture/src/macos.rs:41-47`, `:104-118`): remover/rearranjar monitor deixa
  borda antiga válida → cliente inalcançável ou warp para área inexistente; só reinício
  resolve. **Fix:** recomputar a união do zero em cada reconfiguração; testar hot-unplug.
  No mesmo arquivo, o cursor local é escondido só no display principal
  (`macos.rs:144-150`; multi-monitor pode deixá-lo visível — confirmar H3).
- **F9 — clipboard macOS só imagem `public.png`.** `input-clipboard/src/macos.rs:58-60`,
  `:99-106`: apps que oferecem apenas TIFF (Preview, screenshots, Safari) não sincronizam
  imagem. **Repro:** copiar imagem do Preview no macOS → colar no Windows (nada). **Fix:**
  fallback `public.tiff`→PNG (ImageIO) respeitando limites.
- **F10 — `CF_HTML` malformado vira HTML cru.** `input-clipboard/src/cf_html.rs:29-47` cai em
  `blob.to_vec()` (com header `Version:0.9...`) quando os offsets são inválidos; o peer cola o
  header. **Fix:** se o blob começa com header, remover a parte textual ou descartar o formato
  HTML (preferir texto).
- **F11 — repeat com taxa fixa e em modificadores.** Windows usa 500 ms/32 ms
  (`windows.rs:45-46`), macOS valores próprios (`macos.rs:108-148`), ignorando a configuração
  do usuário/sender; `spawn_repeat_task` também dispara para modificadores (Ctrl re-enviado).
  **Fix:** ler `SPI_GETKEYBOARDDELAY/SPEED` no Windows e repetir só teclas não-modificadoras.
- **F12 — sessão libei reinicia em hotplug de input.** `input-capture/src/libei.rs:46-70`: em
  GNOME todo `DeviceAdded/Removed` derruba e recria a sessão (custo CreateSession/EIS;
  Hyprland vaza keymap fd — motivo do override `LM_RESTART_SESSION_ON_DEVICE_CHANGE`). Efeito:
  mouse USB conectado/desconectado durante o uso pode pausar input e re-disparar prompt.
  **Fix:** manter barreiras estáveis quando só um device some; documentar o override.
- **F13 — CI/docs desalinhados.** `.github/workflows/rust.yml` compila/testa em
  Ubuntu/Windows 2022/2025/macOS, mas não roda harness de input; `release.yml:95-105` só faz
  smoke `doctor --probe` no Windows (sem equivalente Linux; sem job de release macOS — só
  cachix). README:55-57 afirma que wlroots não trata modificadores, mas `wlroots.rs:229-255`
  atualiza mods a partir de Key events (caveat provavelmente obsoleto); DOC.md:117 diz
  “wlroots/X11 clipboard não implementado”, o código tem `wayland.rs` e `x11.rs`. **Fix:**
  matriz única de suporte no README + smoke `doctor --probe --json` por release e teste
  manual roteirizado com `scripts/vm-test-lab.sh`.

### P3
- **F14 — macOS side buttons e cursor oculto no Windows.** Back/forward postados como
  `OtherMouse` com `MOUSE_EVENT_BUTTON_NUMBER` 3/4 (`input-emulation/src/macos.rs:429-505`);
  apps que ignoram o campo não navegam *(H3)* — testar Safari/Finder/VSCode. O overlay de
  cursor oculto (`event_thread.rs:376-412`, `:507-510`) pode não cobrir fullscreen exclusivo
  (testar DX exclusive).
- **F15 — listener de clipboard Windows é singleton.**
  `input-clipboard/src/windows.rs:240-270`: `NOTIFY.set` antes de confirmar o thread — um
  retry após falha sempre erra “listener already running”. **Fix:** setar a cell só após
  `ready`.

## 4. Cenários de teste por combinação

Pré-requisito: duas máquinas (ou `scripts/vm-test-lab.sh`), release + `LAN_MOUSE_LOG_LEVEL=debug`.

| # | Combinação / cenário | Como reproduzir | Esperado / achado coberto |
|---|---|---|---|
| 1 | Ubuntu GNOME → Win10/11, 4 bordas, 2 monitores e teclado | arrastar; segurar Backspace/setas; chords Alt+Tab, Win, Ctrl+C | handoff imediato; repeat ok (receiver Windows); F1 com janela elevada; F6 em 100%+150% |
| 2 | Win10/11 → Ubuntu GNOME/KDE | segurar tecla; digitar; chords | **F3** (sem repeat); F12 em hotplug USB |
| 3 | Win10/11 → Ubuntu wlroots/Sway | idem + `shortcuts-inhibit` no Wayfire | F3; F13 (caveat de modifiers) |
| 4 | Win10/11 → Ubuntu X11 | idem + copiar/colar texto 100 KiB e 1 MiB | F3; F5 (clip grande), XTest ok |
| 5 | Host X11 → qualquer destino | ativar cliente a partir de sessão X11 | F4: captura Disabled; só recebe |
| 6 | Windows com UAC/Task Manager em foco × cliente Ubuntu | abrir UAC/regedit e tentar controlar | F1 (host trava), F2 (cliente não recebe) |
| 7 | Ubuntu → macOS e macOS → Ubuntu/Windows | TCC concedido; repetir tecla; scroll natural | F8/F9/F14; scroll depende do setting |
| 8 | DPI misto 100/125/150/200 nos 4 cantos | mover entre monitores e cruzar bordas | F6; velocidade lógica deve ser igual |
| 9 | Layout L e monitores negativos (acima/esquerda) | arrastar para bordas internas/externas | só borda externa é barreira; F8 no hot-unplug |
| 10 | Clipboard matriz | texto/HTML/PNG/TIFF, 100 KiB/1 MiB, formato secreto | F5/F9/F10; TIFF não sincroniza; secretos nunca |
| 11 | Hotplug monitor e wake/suspend | desplug/replug; suspender host e cliente | F6/F8; reconexão sem teclas presas e cursor restaurado |

## 5. Correções recomendadas (ordem de valor)

1. **P0 — trocar `send_input_safe` por tentativa limitada** (F1), sem bloquear o executor; é o
   único achado que trava o app inteiro. Testar com UAC/elevated.
2. **P1 — repeat nos backends Linux** (F3) no mesmo padrão do Windows; teste obrigatório
   Win→Ubuntu e Wayland→Wayland.
3. **P1 — decidir X11** (F4): implementar captura ou remover anúncio (`README:45/849/873/899`,
   `--capture-backend x11`, feature `x11_capture`) e alinhar DOC.md:172 com aviso “receiving
   only”.
4. **P1 — UIPI/elevated** (F2): mensagem no diagnóstico/UI e documentação de execução elevada;
   avaliar helper no instalador Windows.
5. **P1 — clipboard silencioso**: INCR/large no X11 (F5) e fallback TIFF no macOS (F9); expor
   `last_error`. **P2 — hardening** Windows (F6/F7), hotplug macOS/Wayland (F8/F12), docs/CI (F13).

## 6. Hipóteses não confirmadas (como confirmar)

- **H1 (F2/F1)**: hooks/injeção sob foreground elevado — teste `runas`/UAC com log trace,
  elevado × não elevado. **H2 (F6)**: `EnumDisplaySettingsW` mistura espaços DPI em PMv2 —
  logar `dmPosition`, `dmPels*` e `GetMonitorInfo` juntos em layout 100%+150%.
- **H3 (F8/F14/F15)**: cursor visível no secundário do macOS, side buttons ignorados por app e
  overlay em fullscreen exclusivo — testes manuais (2 displays, Safari/VSCode, jogo exclusive).
- **H4 (velocidade/repeat)**: compositores podem aplicar aceleração a motion libei (≠
  XTest/Windows absoluto) e libei capture pode emitir repeats — medir antes de F3 e calibrar
  `sensitivity` (expectativa, não bug).

## 7. Limitações e handoff

Auditoria estática (sem runtime Windows/macOS neste host): F1/F3 são determinísticos por
leitura; F2/F6/F8(parcial)/F14/F15 precisam de teste. Fora de escopo (outros docs/agentes):
handshake/permissões/rede (`docs/connection-permissions-plan.md`), portais Ubuntu, TCC macOS,
permissões Windows e estratégia. Sem mudança de código; único arquivo: este doc.
