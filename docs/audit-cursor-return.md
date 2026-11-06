# Auditoria: cursor preso no client / caminho de retorno (v0.2.4-custom)

Sintoma reportado: o input chega ao client, mas ao mover o cursor de volta para o
server ele **não volta** — fica preso na tela do client (Windows 11 e Ubuntu 26).
Nos mesmos setups o peer alternava IPv4 ↔ IPv6 e às vezes apareciam dois
dispositivos conectados. Base: HEAD `2c8b201` + working tree multi-agente.

## 1. Como o retorno deveria funcionar

O handoff é simétrico: quem está capturando mantém o cursor local escondido e
envia `Enter`/`Input`; o lado controlado, quando o cursor injetado **atravessa a
borda por onde o peer entrou**, responde com `Leave` — o capturador recebe o
`Leave` em `src/capture.rs`, sintetiza key-ups/modifiers e solta o grab
(`src/capture.rs::release_capture`, ~linhas 830-905). Sem esse `Leave` o server
continua capturando para sempre (cursor escondido e preso no client).

## 2. Causas raiz encontradas

| # | Causa | Evidência (pré-fix) |
|---|---|---|
| C1 | **Windows client não conseguia detectar o retorno.** A emulação Windows injeta o cursor com posições **absolutas** (`MOUSEEVENTF_ABSOLUTE|VIRTUALDESK`, `input-emulation/src/windows.rs:96-180`). O detector de barreira do capture Windows é raw input **relativo** e descarta pacotes `MOUSE_MOVE_ABSOLUTE` (`input-capture/src/windows/event_thread.rs:643-647` no HEAD) — o movimento injetado que empurra o cursor contra a borda nunca chegava ao detector. Além disso `REMOTE_ACTIVE` desligava `may_activate()` por completo (linhas 536-544) e o *injection grace* de 120 ms ignorava justamente o movimento injetado. Resultado: o client nunca enviava o `Leave` de retorno. |
| C2 | **Estado de supressão travado.** `EmulationEvent::ReleaseNotify`/`Disconnected` só limpavam `remote_active` quando `incoming_conns` ficava vazio, e `ProtoEvent::Leave` (`src/emulation.rs:264-267` no HEAD) apenas removia o handle de emulação: a zona de captura de entrada e o flag remoto ficavam presos. Depois do primeiro retorno (ou de uma sessão morta), o capture local podia ficar suprimido para sempre. |
| C3 | **Multi-transporte duplicava o peer.** O listener já deduplica por fingerprint (task-17), mas o service ainda criava `incoming_conns`/zona de captura por `SocketAddr` (`add_incoming`) e o client reverso casava por IP puro (`client_with_ip(addr.ip())` → `find_by_ip`), **sem fingerprint e sem porta**. O reverso nascia na `DEFAULT_PORT` (4242), era ativado, e `activate_client` desativava o client configurado na mesma posição. Evidência: `scripts/qa-two-instances.sh duplicate` — A e B com 2 linhas, um client em `:4242`, o configurado `active: false`. |
| C4 | **Captura refletia o movimento injetado.** Enquanto um peer dirigia esta máquina, um `Begin` de barreira em um capture de saída podia iniciar um capture local a partir do movimento injetado, criando loop/ping-pong (era o motivo do flag remoto "tudo ou nada"). |

## 3. Correções implementadas

### 3.1 Retorno observado no lado que injeta (funciona com injeção absoluta)

- `input-emulation/src/lib.rs:37-56`: novo `PointerEdge` + `EdgeReturnNotifier`
  e `InputEmulation::set_edge_return_notifier` (default da trait é no-op, então
  backends que não conseguem detectar nada continuam compilando).
- `input-emulation/src/absolute.rs:107-140`: `overshoot_edge(screen, requested)`
  — a borda que o alvo **não clampado** ultrapassa (canto = maior overshoot).
- `input-emulation/src/windows.rs:96-311`: em `move_absolute`, o alvo é
  comparado com a tela antes do clamp; se o peer continua empurrando o cursor
  para fora, o backend chama `request_edge_return` (throttle de 250 ms; um
  `Leave` perdido é reenviado até o peer soltar).
- `src/emulation.rs:79-90, 276-292, 600-660, 685-693`: eventos
  `EmulationEvent::EdgeReturn(Position)` e `EmulationEvent::PeerLeft{addr,
  fingerprint}`; `Leave` recebido vira `PeerLeft` (antes só removia o handle de
  emulação).
- `src/service.rs:1118-1159`: `request_return(pos)` envia `Leave` para toda
  sessão de entrada cujo `pos` casa com a borda (o caminho de retorno);
  `peer_released` remove a zona de captura/sessão do peer e só então limpa o
  estado remoto.

### 3.2 Estado remoto e cooldowns no capture

- `input-capture/src/lib.rs:181, 331-337`: API `set_remote_peer(Option<Position>)`
  no lugar de `set_remote_active(bool)` — o capture passa a saber **por qual
  borda** o peer entrou.
- `input-capture/src/windows/event_thread.rs:79-88, 542-565, 566-630, 870-892`:
  a supressão agora é por borda: o movimento injetado continua ignorado em
  todas as bordas, **exceto** na borda de entrada do peer, que é o retorno
  (`edge_activation_allowed`). Ao tomar controle (`RemotePeer(Some)`), um capture
  local ativo é liberado (cursor restaurado) e o cooldown de release é limpo
  para o retorno não ser engolido.
- `src/capture.rs:324, 610-625, 925-936`: o capture guarda o peer remoto e
  ignora `Begin` de capture de **saída** enquanto um peer dirige a máquina
  (`is_suppressed_remote_begin`); só o capture de entrada (EnterOnly) reage e
  dispara o `Leave`. Sem isso o movimento injetado voltava como input (loop).

### 3.3 Um client lógico por peer (reverse client / IPv4+IPv6)

- `src/service.rs:99, 366, 2371-2405`: clientes reversos criados
  automaticamente são marcados em `auto_reverse_clients`; quando o client
  configurado aprende o fingerprint por TOFU (`conn_state_rx`), o reverso é
  mesclado (`merge_auto_reverse_client`: herda `incoming_addr`, desativa/
  remove a duplicata e emite `Deleted`).
- `src/service.rs:2313-2405`: matching por **fingerprint primeiro**
  (`client_with_fingerprint`, `client_for_peer`, `client_for_incoming`); IP é só
  dica para entradas legadas sem certificado pinado
  (`reverse_client_compatible` recusa reutilizar um client pinado para outro
  certificado).
- `src/service.rs:2226-2300` (`add_reverse_client`): não inventa porta — usa a
  porta anunciada no discovery; sem porta conhecida o client é criado
  **inativo** (antes discava `DEFAULT_PORT` 4242). Nunca muda posição/porta/
  estado de um client já configurado.
- `src/service.rs:2423-2452` (`activate_client`): não desativa o client da
  mesma posição quando é o **mesmo fingerprint** (era isso que desligava o
  client configurado no meio da captura).
- `src/service.rs:1160-1210` (`add_incoming`) + `Entered`: uma identidade tem
  no máximo uma sessão/zona de captura; troca de transporte remove a antiga
  (`remove_incoming` + `IncomingDisconnected`).

## 4. Mudança de comportamento (intencional)

Enquanto um peer dirige esta máquina, um `Begin` de barreira em capture de
saída é tratado como pedido de retorno (Leave para o peer), não como novo
capture local. O controle mútuo volta a ser possível normalmente depois que o
peer solta (o `Leave` limpa o estado remoto via `PeerLeft`).

## 5. Testes e validação executados

- `input-emulation/src/absolute.rs`: 4 testes novos de `overshoot_edge`
  (4 bordas, alvo interno, canto, clamp) — passam em Linux.
- `input-capture/src/windows/event_thread.rs`: teste de
  `edge_activation_allowed` (roda no CI Windows).
- `src/capture.rs`: teste de `is_suppressed_remote_begin` (Linux).
- `src/service.rs`: testes de `reverse_client_compatible`.
- Windows cross-check: `cargo check --target x86_64-pc-windows-gnu -p
  input-capture -p input-emulation` validado com um ajuste **temporário** de
  `build.rs` (o `cfg!(unix)` do build script usa o host, não o target; revertido
  — `build.rs` não está no escopo desta task).
- `cargo test --workspace`: tudo verde (incl. 171 testes da lib do root,
  49 input-clipboard, 26 input-emulation, 19/23 input-capture etc.).
- QA end-to-end (`scripts/qa-two-instances.sh`, backends dummy):
  **29/29 checks OK**; `duplicate` agora `4/4` (A=1 linha, B=1 linha,
  conectado bidirecional, **nenhum `:4242`**), `dual` e `ipv6_only` sem
  regressão.
- `cargo test -p lan-mouse --no-default-features --test qa_ip_family --test
  qa_connection_state --test qa_installer_clean`: 7/3/5 passam.

## 6. Repro manual (o que ainda precisa de máquina real)

Windows 11/10 ↔ Ubuntu 26, release com `LAN_MOUSE_LOG_LEVEL=debug`. Para cada
direção e para as duas famílias (E1 IPv4-only, E2 dual-stack, E3 IPv6-only):
1. Configurar o peer **uma vez** dos dois lados (posições espelhadas), autorizar.
2. Cruzar a borda 5× em cada sentido (T9 de `docs/qa-repro-matrix.md`).
3. Repetir depois de reconectar pelo botão Disconnect/Connect e depois de trocar
   a família ativa (bloquear a família atual no firewall → failover).
4. Abrir janela elevada (UAC) no client e repetir; testar hotplug de monitor.

Log esperado no client ao voltar: `emulation observed a return hand-off through
Left` → `remote pointer pushed past the Left edge: requesting hand-off`; no
server: `handing control back: remote pointer left through ...` e
`peer at <addr> released its capture (return hand-off)`; o cursor local deve
reaparecer em < 100 ms. Se o server continuar preso, o log mostra se o `Leave`
não chegou (`request_return` sem sessão de entrada casando a borda) ou se o
capture ainda está ativo (`ignoring outgoing capture begin ...`).

## 7. Pendências / limitações conhecidas

- **Ubuntu/GNOME (libei) como client**: a emulação libei injeta movimento
  relativo e não consegue saber a posição do cursor (compositor não expõe), então
  o retorno depende do barrier do portal de InputCapture disparar para o
  movimento injetado. Se o mutter não disparar, o retorno por software não tem
  como ser detectado hoje — precisa de teste real; o fallback documentado é o
  release-bind do capture (chord) ou extensão GNOME.
- **Windows**: o caminho de retorno absoluto foi validado por compilação e
  lógica, mas não executado em Windows real nesta máquina; a matriz T8/T9 cobre.
- **Mutter/GNOME**: sessão de captura recriada em device change ainda pode pedir
  consentimento (task-13 documenta a limitação v1 do portal).
- Duplicatas já existentes em configs antigas: o merge TOFU remove o reverso
  criado pelo service; duplicatas **manuais** no `config.toml` continuam
  precisando de limpeza pelo usuário/GTK (task-19 agrega na UI).
- O `RemoteSession`/reverse client com porta desconhecida fica **inativo** e
  precisa que o usuário informe o endereço para ativar (antes discava 4242 e
  fingia "connected").
