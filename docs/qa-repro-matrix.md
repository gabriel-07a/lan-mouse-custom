> **UPDATE (2026-11-06):** Lan Mouse is now **IPv4-only** (product decision).
> The IPv6-only rows and IPv6 failover steps below are historical; the current
> acceptance matrix is IPv4-only (E1) and dual-stack hosts must simply use IPv4.
>
# QA — reprodução e matriz de regressão dos 4 bugs reportados (v0.2.4-custom)

Missão (task-21): QA independente dos bugs reportados em Windows 11/10 + Ubuntu 26 —
(1) peer alterna IPv4/IPv6 na UI; (2) às vezes dois dispositivos conectados para o mesmo peer e
"Connected" falso; (3) cursor fica preso no client ao voltar; (4) configs antigas sobrevivem à
instalação. Este documento é a matriz manual + evidência da reprodução local automatizada.
Núcleo automatizado: `scripts/qa-two-instances.sh` (end-to-end em Linux) e `tests/qa_*.rs`
(regressão de contrato). Nenhum código de produção foi editado por este QA.

Donos das correções: **net-auditor** (task-17 — identidade/endereço: `src/client.rs`,
`src/connect.rs`, `src/discovery.rs`, `src/listen.rs`), **gtk-phase0** (task-18 — cursor:
`input-*/**`, `src/service.rs`, `src/emulation.rs`), **conn-state-analyst** (task-19 — estado
verídico: `lan-mouse-gtk/**`, `lan-mouse-ipc/src/lib.rs`), **gtk-phase1** (task-20 — instalação
limpa: `installer/windows/**`, `scripts/install-clean.sh`, `src/reset.rs`).

> Status no momento da escrita: correções no working tree (não commitadas) de task-17/18/19/20.
> Após essas correções, a reprodução local **passa 30/30** (incluindo `duplicate`: 1 linha por peer,
> sem client reverso na porta default 4242) e os 3 arquivos `tests/qa_*.rs` passam (5+7+3). O que
> ainda é necessário: validação final (`cargo fmt/test/clippy`) e a matriz manual E1–E5 × T1–T11 em
> máquinas reais — cursor, badge GTK, UAC/SmartScreen, `.deb` e redes reais só se validam lá.
> Limitações conhecidas: ~5 s de "connected" stale no CLI/service após matar o peer (watchdog
> deliberado; a UI usa `last_pong_at`/freshness) — ver §7.2.
> A matriz manual abaixo é o que precisa ser executado nas máquinas reais; a §1/§5 é o que já roda
> neste host. Reexecute `scripts/qa-two-instances.sh` + `cargo test` quando o tree estabilizar
> (comandos na §6).

## 1. Reprodução local automatizada (Linux, duas instâncias)

```sh
cargo build -p lan-mouse --no-default-features     # binário headless (daemon+CLI, backends dummy)
scripts/qa-two-instances.sh                        # todos os cenários
QA_KEEP=1 scripts/qa-two-instances.sh dual         # mantém dirs/logs p/ inspeção
QA_PORT_A=43421 QA_PORT_B=43422 scripts/qa-two-instances.sh   # portas alternativas
```

Cada instância roda com `HOME`/`XDG_CONFIG_HOME`/`XDG_STATE_HOME`/`XDG_CACHE_HOME`/
`XDG_RUNTIME_DIR` próprios (sem compartilhar config, cert, audit log ou socket IPC), `trusted_ips`
de loopback para autorização e backends `dummy`. Os logs debug ficam em
`target/qa-two-instances/<a|b>/daemon.log`.

**Evidência pré-fix já coletada (HEAD `2c8b201`, antes das correções):**

| cenário | resultado observado |
|---|---|
| `dual` (client com `127.0.0.1` + `::1`) | PASS: 1 linha, `active_addr` estável (10 amostras) e também estável após reiniciar o peer (mesmo endereço/família) |
| `duplicate` (A configurado em IPv4, B em IPv6) | **A: 2 linhas** (`id0` to 127.0.0.1 ok + `id1 ::1:4242` "connected (from [::1])"); **B: 2 linhas**, sendo a linha configurada **desativada** e um cliente reverso `127.0.0.1:4242` conectado — mesmo peer, port default 4242 (não o 42421/42422 real). **Corrigido após o patch de service**: reprodução de 2026-11-06 14:30 mostra **1 linha em A e 1 em B**, `connected (bidirectional)` correto e nenhum client em `:4242` |
| `ipv6_only` | conecta via `[::1]`, 1 linha — ok (não pode regredir) |
| `peer_down` | A continuou reportando `connected` por **~5,25 s** após o peer morrer (janela deliberada do watchdog; a UI da task-19 degrada por `last_pong_at`) — limitação conhecida §7.2 |
| `stale_config` | app carrega config antiga normalmente → se o instalador não limpar, devices antigos voltam |
| `reset_config` | PASS: `--reset-config` recusa com serviço rodando; `--dry-run` só lista; reset padrão **move para `lan-mouse.bak-<ts>`** (reversível, preserva arquivo não relacionado) e `--purge` apaga em definitivo |
| `isolation` | config/cert/audit ficam só nos XDG dirs de cada instância — ok |

Conclusão da reprodução: **(1)+(2) têm a mesma raiz** — identidade do peer indexada por IP
(`find_by_ip`, `client_with_ip`) e auto-adoção de "reverse client" quando a família de entrada não
bate com o IP configurado; o cliente reverso nasce com porta default 4242, ativa-se e, em colisão
de posição, **desativa** a linha configurada (o que também explica o estado "Connected" mentiroso e
contribui para o cursor preso). **(4)** é responsabilidade do instalador/reset. **(3)** não é
reproduzível sem captura/emulação real → matriz manual (§3).

## 2. Mapa sintoma → causa provável → dono → verificação

| # | Sintoma reportado | Causa/evidência (pré-fix; ver `git show 2c8b201:<arquivo>`) | Dono | Verificação automática | Manual |
|---|---|---|---|---|---|
| 1 | linha/subtítulo alterna IPv4 ↔ IPv6 | `ClientManager::get_client`/`find_by_ip` casam **só IP**; `state.ips` é `HashSet` e o frontend escolhia `ips.first()`; incoming por família diferente não casa | net-auditor (task-17) | `tests/qa_ip_family.rs`; `qa-two-instances.sh dual duplicate` | §3 T3/T4/T5 |
| 2 | dois dispositivos "conectados" p/ mesmo peer | `service.rs`: `client_with_ip(addr.ip()).is_none()` ⇒ `add_reverse_client(ip)` sem fingerprint/porta (default 4242) e `activate_client` desativa o client da mesma posição | net-auditor (fix de identidade) + possível patch em `service.rs` (fora do escopo exclusivo da task-17) | `qa-two-instances.sh duplicate` (esperado: 1 linha/lado, sem `:4242`) | §3 T2/T8 |
| 2b | "Connected" falso | `alive` é emulação do peer e vem otimista do handshake; não havia timestamp de Pong (`last_pong_at` foi adicionado à `ClientState` pela task-19) — janela de ~5,5 s medida no `peer_down` | conn-state-analyst (task-19) + service/heartbeat | `tests/qa_connection_state.rs`; `qa-two-instances.sh peer_down` (medida) | §3 T7 |
| 3 | cursor fica preso no client | alteração de família/duplicata deixa client desativado no meio da captura; `remote_active`, overlay do cursor e release de botões/teclas dependem do estado por (addr/posição) — precisa de sessão real (Windows hooks / X11) | gtk-phase0 (task-18) | não automatizável neste host | §3 T9 |
| 4 | configs/devices antigos sobrevivem à instalação | estado per-user (`%LOCALAPPDATA%\lan-mouse` etc.) não era removido pelo instalador; `--reset-config` não existia | gtk-phase1 (task-20) | `tests/qa_installer_clean.rs`; `qa-two-instances.sh stale_config` (pré-condição) | §3 T10 |

## 3. Matriz manual Windows ↔ Ubuntu 26

**Ambientes mínimos (E1–E5):**

| ID | Par | Rede |
|---|---|---|
| E1 | Windows 10 ↔ Ubuntu 26 (lan) | IPv4-only (desabilitar IPv6 ou firewall bloqueando AAAA) |
| E2 | Windows 11 ↔ Ubuntu 26 (lan) | dual-stack (hostname do peer resolve A+AAAA) |
| E3 | Windows 10 ↔ Ubuntu 26 | IPv6-only (peer sem IPv4) |
| E4 | Windows 11 ↔ Ubuntu 26 | dual-stack + Wi-Fi instável/reconexão |
| E5 | Ubuntu 26 ↔ Ubuntu 26 (controle) | dual-stack |

**Testes (rodar em E1–E4; marcar PASS/FAIL e anexar print/log):**

| ID | Objetivo | Passos | Esperado | Evidência |
|---|---|---|---|---|
| T1 | instalação limpa | desinstalar versão antiga (ou máquina nova), instalar release; abrir app | zero devices na primeira abertura; `%LOCALAPPDATA%\lan-mouse` recriado só com default; certificado novo | print da lista vazia; `ls` do diretório |
| T2 | um único device por peer | configurar o peer uma vez; conectar; reconectar (botão Disconnect/Connect) 3× | **uma única linha** no Clientes e no painel Devices; nenhum "reverse client" extra | `lan-mouse cli list` nos dois PCs; screenshots |
| T3 | endereço estável | com peer conectado, monitorar o subtítulo/endereço por 60 s + 3 reconexões | endereço e família **não alternam**; escolhida uma família e a outra só como failover | screenshots em sequência; logs `debug` |
| T4 | IPv4-only | desabilitar IPv6 (win: propriedades da NIC; ubu: `sysctl -w net.ipv6.conf.all.disable_ipv6=1`) | conecta por IPv4, estado Connected verídico, sem tentativas IPv6 duplicando linhas | `ip -br a`; logs |
| T5 | failover de família | dual-stack conectado; bloquear a família ativa no firewall (`nft/ufw`/Windows Firewall) | reconecta pela outra família em até ~10 s, **sem** criar segunda linha; sem "Connected" durante o buraco | logs debug; `cli list` antes/depois |
| T6 | IPv6-only | desabilitar IPv4 (win/ubu) ou peer só com AAAA | conecta por IPv6 e permanece conectado (fix não pode forçar IPv4) | logs; `cli list` |
| T7 | Connected verídico | conectar; confirmar que input funciona; matar o daemon do peer (`taskkill /IM lan-mouse.exe` / `pkill lan-mouse`); observar o badge | badge deixa de dizer Connected/Connected(bidirectional) em ≤ ~6 s e mostra degradado/reconectando; nunca "Connected" com input quebrado; ao voltar, retorna sozinho | vídeo/prints com timestamps; logs |
| T8 | bidirecional sem duplicatas | ambos os PCs com o outro configurado, ativos e autorizados; cruzar borda nos dois sentidos | 1 linha por peer em cada lado; nenhum device duplicado no painel Devices; ao reconectar por outra família continua 1 linha | `cli list` + print Devices |
| T9 | cursor volta | entrar no client e voltar cruzando a borda **5× em cada sentido**; repetir após reconectar (Disconnect/Connect) e após failover de família (bloquear a família ativa no firewall 30 s); repetir com janela elevada/UAC em foco no client e após hotplug de monitor | cursor retorna em <100 ms; teclas/botões não ficam presos; sem "pulo" de volta imediato. Além disso (P1-1): um Begin suprimido durante o gesto de retorno deve liberar o backend (sem cursor escondido/overlay/`ACTIVE_CLIENT` remanescente — o próximo input local funciona imediatamente). PASS (logs client): `emulation observed a return hand-off through <edge>` e/ou `ignoring outgoing capture begin while a peer drives this machine`; PASS (logs server): `handing control back: remote pointer left through <pos>` + `peer at <addr> released its capture (return hand-off)`; sem `remote_active`/cursor escondido remanescente após cada retorno. Falhas: `request_return` sem sessão casando a borda (transporte/posição errados); sem log de retorno no client Ubuntu/GNOME (barrier do libei não dispara p/ motion injetado — limitação documentada); client só loga `ignoring outgoing capture begin` e nada mais (EnterOnly/Leave não chegou) | gravação de tela (30 s) nos dois sentidos + logs debug dos dois PCs (client e server) |
| T10 | reinstall sem devices antigos (backup reversível) | configurar 2 devices; desinstalar ou instalar release por cima (interativo: **Yes** move p/ backup; silencioso `/S`: move por default); reinstalar release; abrir | lista vazia (nenhum device antigo); estado antigo **não é apagado**: fica em `%LOCALAPPDATA%\lan-mouse.bak-<timestamp>` (e `%APPDATA%\lan-mouse.bak-<timestamp>` se existia) e o diálogo mostra esse path. Para restaurar: fechar o app, `ren "%LOCALAPPDATA%\lan-mouse" "%LOCALAPPDATA%\lan-mouse.novo"`, `ren "%LOCALAPPDATA%\lan-mouse.bak-<ts>" "%LOCALAPPDATA%\lan-mouse"` (e idem `%APPDATA%`), reabrir. `/KEEPUSERDATA` mantém no lugar (upgrade); `/PURGEUSERDATA` apaga de vez | prints do diálogo (com path do backup) e da lista vazia; `dir %LOCALAPPDATA%\lan-mouse*`; `dir %APPDATA%\lan-mouse*`; `lan-mouse --reset-config --dry-run`; `lan-mouse cli list` |
| T10b | upgrade preserva / reset explícito | instalar release por cima respondendo **No** (ou `/S /KEEPUSERDATA`); depois rodar `lan-mouse --reset-config --dry-run` e `--reset-config` | No: devices preservados (upgrade); dry-run lista paths; reset **move** para `lan-mouse.bak-<timestamp>` (reversível; nada de devices antigos na UI) e `--purge` apaga de vez. No Windows o diálogo já exibe o path `lan-mouse.bak-<stamp>` e explica o restore por rename; no upgrade o default é "start fresh" (Yes, reversível), então documentar no release notes | `cli list`; saída do reset; `ls` dos dirs `.bak-*`; print do diálogo |
| T11 | Ubuntu pacote | `apt install` novo `.deb` por cima do antigo; `apt purge`; reinstalar | upgrade mantém devices (por design); purge **não** apaga home e imprime como resetar; após reset, lista vazia | saída do `dpkg`/`postrm`; prints |

Captura de evidência por teste: screenshot do app (badge+subtítulo+linha), `lan-mouse cli list` dos
dois PCs, trecho do log `debug` com `addr`/`connected`/`disconnect`, e `lan-mouse doctor --json`
(`capture_probe/emulation_probe`, `local_fingerprint`) quando aplicável.

## 4. Coleta de logs (debug)

- **Windows** (release é GUI subsystem; capture stderr redirecionando):
  ```powershell
  $env:LAN_MOUSE_LOG_LEVEL = "debug"
  & "$env:ProgramFiles\Lan Mouse\lan-mouse.exe" 2> "$env:TEMP\lan-mouse-gui.log"
  # daemon separado, se preciso:
  & "$env:ProgramFiles\Lan Mouse\lan-mouse.exe" --config "$env:LOCALAPPDATA\lan-mouse\config.toml" daemon 2> "$env:TEMP\lan-mouse-daemon.log"
  & "$env:ProgramFiles\Lan Mouse\lan-mouse.exe" cli list
  & "$env:ProgramFiles\Lan Mouse\lan-mouse.exe" doctor --json
  ```
  Estado: `%LOCALAPPDATA%\lan-mouse\{config.toml,lan-mouse.pem,audit.jsonl}`.
- **Ubuntu 26**:
  ```sh
  LAN_MOUSE_LOG_LEVEL=debug lan-mouse 2>&1 | tee /tmp/lan-mouse-gui.log
  lan-mouse cli list; lan-mouse doctor --json
  ```
  Estado: `~/.config/lan-mouse/`, `~/.local/state/lan-mouse/` (audit/tokens), `~/.cache/lan-mouse/` (legado).
- **Sempre**: anotar OS/versão, família de endereço usada (`cli list`), se a rede é IPv4-only/dual/
  IPv6-only, e horário dos eventos (para casar log ↔ print).

## 5. O que é automatizado vs o que é só manual

**Automatizado neste host (Linux):** identidade por peer/endereço (`tests/qa_ip_family.rs`),
contrato de `ClientState` incluindo `last_pong_at`/compatibilidade IPC
(`tests/qa_connection_state.rs`), limpeza do instalador Windows por análise do script
(`tests/qa_installer_clean.rs`), e o fluxo real de 2 instâncias com famílias mistas
(`scripts/qa-two-instances.sh`: dual, duplicate, IPv6-only, peer-down, stale-config, reset-config, isolation).
**Só manual (precisa de SO/sessão real):** cursor voltando (hooks/overlay Windows, X11/Wayland),
veracidade visual do badge/linha GTK, instalação/UAC/SmartScreen, pacote `.deb`/purge, Wi-Fi/rede
real, promoção de janelas admin (UIPI), e a matriz E1–E4 por completo.

## 6. Critérios de aceite pós-fix (rodar quando o working tree estabilizar)

```sh
cargo fmt --all -- --check
cargo test -p lan-mouse --no-default-features --test qa_ip_family \
  --test qa_connection_state --test qa_installer_clean
cargo test --workspace
cargo clippy --workspace --all-targets
scripts/qa-two-instances.sh          # todos os cenários devem terminar com 0 falhas
```

- `duplicate`: A e B com **1 linha cada** e sem cliente em `:4242` (port default). Hoje
  **falha** porque o `src/service.rs` ainda cria o client reverso por IP; decisão pendente (§7.1).
  Enquanto o patch não entra, o critério de aceite visível é a UI (uma linha por fingerprint,
  task-19) — validar manualmente em T2/T8; o harness mantém o critério do service.
- `peer_down`: registrar o tempo de mentira; aceitável ≤ ~6 s (heartbeat/freshness da task-19).
- `tests/qa_ip_family.rs`: mapear A+AAAA para o mesmo handle, sem duplicar clientes, inclusive sob
  churn de DNS e união `fix_ips ∪ dns_ips`.
- `tests/qa_installer_clean.rs`: uninstall/instalação limpa removem `%LOCALAPPDATA%\lan-mouse` e
  `%APPDATA%\lan-mouse`, pedem confirmação no modo interativo e respeitam `/KEEPUSERDATA`.
- `tests/qa_connection_state.rs`: `last_pong_at` (liveness) independente de `alive` (emulação) e
  campos futuros/ausentes tolerados por serde.
- Matriz E1–E4 com T1–T11 PASS (ou desvios justificados/abertos).

### 6.1 Resultado da validação final (working tree, não commitado)

Snapshot: `HEAD 2c8b201` + working tree (`git diff` sha256 `beed8fa6…`, estável antes/depois da
suíte; 35 arquivos alterados/untracked de task-17..20 + fixes finais do Lead: guard do busy-loop
(`src/emulation.rs`) e release no Begin suprimido (`src/capture.rs`); nenhum commit feito por este QA).

| comando | resultado |
|---|---|
| `cargo fmt --all -- --check` | **PASS** (exit 0) |
| `cargo test --workspace` | **PASS** — 395 passed, 0 failed (1 ignored: X11 sem display) |
| `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** (exit 0, 0 warnings) |
| `cargo test -p lan-mouse --lib edge_return_guard_tests` | **PASS** — 2/2 (`closed_edge_return_channel_is_disabled`, `closed_edge_return_arm_does_not_busy_loop`) |
| `cargo test -p lan-mouse --lib capture::enter_state_tests` | **PASS** — 7/7, incl. `a_remote_peer_only_allows_the_incoming_capture_to_begin` (predicado do P1-1) |
| `cargo test -p lan-mouse --test qa_ip_family --test qa_connection_state --test qa_installer_clean` | **PASS** — 5 + 7 + 3 |
| `scripts/qa-two-instances.sh` | **PASS — 30/30 checks** (dual, duplicate, ipv6_only, peer_down, stale_config, reset_config, **no_spin**, isolation) |

**P0 busy-loop (security-auditor → fix do Lead em `src/emulation.rs`)**: verificado por inspeção
(`edge_return_notifier_active` = `!rx.is_closed()`), pelos 2 unit tests acima e por um teste de
caixa-preta novo no harness (`no_spin`): daemon idle com backend Dummy gastou **160 ms de CPU em
3 s (~5%)**, muito abaixo do que um spin (≈100% de um core) mostraria.

**P1-1 latch de cursor no gesto de retorno (security-auditor → fix do Lead em `src/capture.rs`)**:
Begin suprimido agora chama `capture.release().await?` (src/capture.rs:615-631), desfazendo a
ativação que o backend possa ter feito; predicado coberto por
`a_remote_peer_only_allows_the_incoming_capture_to_begin` e o workspace passa. **Não é reproduzível
neste host** (módulo `capture` privado + backend Dummy nunca gera Begin com peer remoto dirigindo);
validar manualmente na T9: após cada retorno não pode sobrar `ACTIVE_CLIENT`/overlay/cursor
escondido, e o próximo input local deve funcionar na hora.

Evidência dos bugs corrigidos no repro Linux: `duplicate` → 1 linha por peer, sem `:4242`,
`connected (bidirectional)` correto; `dual` → endereço estável (inclusive após reconexão);
`ipv6_only` → conecta por `[::1]`; `reset_config` → backup reversível + `--purge`; instalação
(`tests/qa_installer_clean.rs`) → move para `lan-mouse.bak-*`, nunca apaga sem `/PURGEUSERDATA`.
Limitação medida e aceita: `peer_down` ~5,2 s de "connected" stale no CLI/serviço (watchdog
deliberado; a UI usa `last_pong_at`/freshness).

**Manual-only** (não executável neste host): matriz E1–E5 × T1–T11 em máquinas reais (cursor T9,
badge GTK T7, UAC/SmartScreen e instalação Windows T1/T10, `.deb`/`apt purge` T11, rede real
Wi-Fi/NAT/firewall T3–T6). Ver §3/§4 e os docs dos donos
(`docs/audit-cursor-return.md`, `docs/audit-connected-truth.md`, `docs/audit-ip-family.md`,
`docs/audit-fresh-install.md`).

## 7. Achados do QA (estado final)

1. **[RESOLVIDO — task-17+task-18/service]** O "reverse client" duplicado foi corrigido no
   `src/service.rs` (patch autorizado pelo Lead, aplicado pelo gtk-phase0): identidade por
   `fingerprint` antes de criar o reverso (`client_with_fingerprint`/`attach_incoming`), porta nunca
   adivinhada (usa porta do discovery; senão o client fica inativo), `activate_client` não desativa
   client da mesma posição quando é o mesmo peer, e `merge_auto_reverse_client` remove o client
   automático quando o configurado aprende o fingerprint. Evidência: cenário `duplicate` PASS,
   `tests/qa_ip_family.rs` 5/5, 157/157 unit tests do net-auditor.
2. **[LIMITAÇÃO ACEITA — task-19/service]** `active`+`active_addr` sozinhos ainda podem parecer
   "connected" por ~5 s no CLI/service após o peer morrer (watchdog deliberado p/ jitter de Wi-Fi).
   A UI usa `last_pong_at` + freshness (6 s) + endpoint sticky (3 s) e degrada antes; a matriz T7
   valida o badge manualmente. Follow-up documentado; não bloqueia a release.
3. **[RESOLVIDO com backup reversível — task-20]** Instalador agora **move** o estado antigo para
   `%LOCALAPPDATA%\lan-mouse.bak-<timestamp>` (e `%APPDATA%` idem) em vez de apagar; prompt explica
   restauração por rename; `/KEEPUSERDATA` mantém, `/CLEANUSERDATA` força o clean e
   `/PURGEUSERDATA` (ou `--purge`) apaga permanentemente. Observação de QA: no upgrade interativo o
   default do prompt continua "start fresh" (agora reversível) — validar T10/T10b e, se o produto
   quiser, sugerir default "keep (upgrade)" quando já houver instalação detectada.
4. **[EM VALIDAÇÃO MANUAL — task-18]** Cursor: fix no tree (dedupe de sessões incoming por
   fingerprint, retorno de borda `EdgeReturn`/`PeerLeft`, `set_remote_peer` por posição, dedupe de
   `incoming_conn_info` no service + input-capture/emulation); sem sessão real de input neste host,
   T8/T9 da matriz são os testes decisivos. Rodar **antes/depois** do patch de identidade para
   separar as causas.
