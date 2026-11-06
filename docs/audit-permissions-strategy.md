# Auditoria: estratégia de permissões e prompts cross-OS

Escopo: inventário dos pedidos de permissão/confirmação do lan-mouse (OS + app), classificação
(inevitável × persistível × reduzível × bug), riscos de silenciar demais e proposta priorizada.
Leitura somente, working tree em `ef5f41f`. Evidência `arquivo:linha`; hipóteses marcadas `[H#]`.
Rede/protocolo: ver `docs/connection-permissions-plan.md` (não duplicado aqui).

## 1. Resumo executivo

O app já só pergunta **uma vez por dispositivo** (grants persistidos em `config.toml`) e não pede
confirmação por evento. O atrito que o usuário relata ("pede permissão toda vez") vem de quatro
fontes, nenhuma delas "aprovação de device":

1. **Prompts de portal Wayland que não persistem**: o `InputCapture` portal está em v1 e não tem
   `restore_token`/`persist_mode`; a sessão é recriada a cada start do daemon e em mudanças de
   clientes/zonas (`input-capture/src/libei.rs:181-191, 49-68, 300-330, 609-627`; `README.md:801-807`).
2. **Ciclo de vida**: a GUI mata o daemon que criou ao sair (`src/main.rs:98-117`); cada reabertura
   recria as sessões de portal — sem systemd unit, o diálogo de captura aparece por *abertura do app*.
3. **Bug de repetição do diálogo do app**: o modal de autorização é reaberto a cada retry (~2 s) do
   peer e também para dispositivos **negados/revogados** (§4-D1/D2).
4. **macOS**: TCC é perdido a cada rebuild/update porque os artefatos são assinados ad-hoc
   (`scripts/copy-macos-dylib.sh:420-426`) e não há job macOS de assinatura/notarização no release
   (`.github/workflows/release.yml:23,53,117`). `[H2]`

Alvo atingível: **Ubuntu/GNOME Wayland** = 1 diálogo de captura por login (inevitável no portal v1) +
0 de emulação/clipboard após o 1º consent; **macOS** = 1 diálogo de Accessibility por identidade de
código estável; **Windows** = 1 UAC por instalação + 0 firewall. **App** = 1 aprovação por device
(com SAS) + re-aprovação só na troca de certificado.

## 2. Matriz: prompt → quando dispara → frequência → persiste? → proposta

| ID | Camada | Prompt | Dispara quando | Frequência real | Persiste? | Evidência | Proposta |
|---|---|---|---|---|---|---|---|
| L1 | Linux Wayland | InputCapture portal ("capturar entrada") | start do daemon; recriação de sessão | 1×/start + 1× por mudança de cliente/zona `[H1]` | **Não** (portal v1) | `input-capture/src/libei.rs:181-191, 49-68, 300-330, 609-627`; `src/capture.rs:369-372, 401-408`; `README.md:801-807` | Reduzir recriações; 1 sessão por login; acompanhar portal upstream |
| L2 | Linux Wayland | RemoteDesktop portal (emulação) | start do daemon | 1× no 1º uso; depois silencioso | **Sim**, token em `~/.cache/lan-mouse/remote-desktop.token` | `input-emulation/src/libei.rs:55-64, 68-85, 87-122`; `src/emulation.rs:546-572` | Mover p/ `XDG_STATE_HOME`, 0600; reusar no fallback Xdp |
| L2b | Linux Wayland | RemoteDesktop portal (fallback Xdp) | start quando libei falha | **toda vez** (bug) | **Não** (código ignora token) | `input-emulation/src/xdg_desktop_portal.rs:33-50` vs `input-emulation/src/libei.rs:87-122` | Copiar read/write token do backend libei |
| L3 | Linux Wayland | RemoteDesktop + Clipboard portal (clipboard) | start com clipboard habilitado (default) | 1× no 1º uso; depois silencioso | **Sim**, token em `~/.cache/lan-mouse/clipboard.token` (0644) | `input-clipboard/src/portal.rs:32, 68-121, 410-431`; `src/clipboard.rs:110-145`; `README.md:686-696` | `XDG_STATE_HOME` + 0600 + escrita atômica; migrar cache |
| L4 | Linux | UFW/firewalld (sem prompt; regra manual) | — | — | firewalld XML só após ação manual | `README.md:158-180, 354` | Doctor detecta/enfileira regra; doc por distro |
| L5 | macOS | TCC Accessibility alert | 1ª execução / grant ausente | 1× por identidade de código; **volta a cada update** por assinatura ad-hoc `[H2]` | **Sim** (TCC), se a identidade for estável | `lan-mouse-gtk/src/macos_privacy.rs:260-308, 77-81, 130-152`; `lan-mouse-gtk/src/window.rs:1520-1533`; `lan-mouse-gtk/src/window/imp.rs:236-265`; `scripts/copy-macos-dylib.sh:420-426` | Assinatura estável (Developer ID/self-signed persistente) + notarização |
| L6 | macOS | Input Monitoring / Post Event | após AX; TCC lista o bundle | 1× | Sim (mesma identidade) | `lan-mouse-gtk/src/macos_privacy.rs:200-230, 292-307` | Manter; só re-registrar se ausente |
| L7 | macOS | Gatekeeper/quarentena | 1º open do `.app` baixado | 1× por download | Não | `README.md:124-133` | Notarizar (elimina o `xattr -rd` manual) |
| L8 | Windows | UAC (instalador) | install/update | 1× | — | `installer/windows/lan-mouse-custom.nsi:23` | Manter (instalação por máquina) |
| L9 | Windows | Defender Firewall prompt/bloqueio | 1º bind UDP 4242 | 0 na instalação MSI/NSIS (regra auto) | Regra OS `private,domain` | `installer/windows/lan-mouse-custom.nsi:68-71, 96-98`; `tests/packaging.rs:43` | Perfil Public: opção explícita + diagnóstico no doctor `[H3]` |
| L10 | Windows | SmartScreen | execução do instalador | 1× (sem EV cert) `[H3]` | reputação OS | não há evidência no repo | Notarização/reputação de distribuição |
| A1 | App | Aprovação de device (SAS + matriz de direções/caps) | 1ª conexão de cert desconhecido | 1× por device (pós-aprovação persiste) | **Sim** (`[[devices]]`) | `src/listen.rs:485-575, 331-338`; `src/emulation.rs:393-397`; `src/service.rs:655-701`; `lan-mouse-gtk/src/window.rs:1191-1256, 1555-1640`; `src/crypto.rs:44-57`; `README.md:469-531, 565-579` | Manter; corrigir repetição (D1/D2) e onboarding |
| A2 | App | Troca de certificado | cert novo p/ device/endereço conhecido | 1× por troca | decisão persiste | `src/service.rs:655-684`; `lan-mouse-gtk/src/lib.rs:382-396` | Manter (é o alerta de impersonation) |
| A3 | App | Pairing window (single-use) + review | ação do usuário (`pair`/botão) | 1× por pareamento | janela não persiste; grants sim | `src/service.rs:1957`; `lan-mouse-gtk/src/lib.rs:410-419`; `README.md:772-787` | Completar painel de review (hoje só toast) |
| A4 | App | `allow --once` (grant de sessão) | aprovação com `--once` | re-pergunta na próxima sessão **por design** | não | `README.md:502-505`; `lan-mouse-cli/src/lib.rs:544-555`; `lan-mouse-ipc/src/lib.rs:790` | Manter; explicitar na UI |
| A5 | App | Address rules (auto-grant por IP/CIDR) | conexão de IP coberto por regra | 0 prompts | **Sim** (`[[address_rules]]`) | `README.md:533-563`; `src/listen.rs:543-557` | Manter; aviso forte p/ prefixos amplos; nunca default-on |
| A6 | App | Kiosk mode | `accept_incoming=false` | 0 prompts (rejeita) | config | `README.md:581-590`; `src/config.rs:895-901` | Manter |
| A7 | App | Clipboard | sem prompt por uso; consent do portal 1× | 1× portal; caps no pareamento | token + caps | `README.md:469-497, 686-696` | Manter (sem confirmação por cópia) |
| A8 | App | Peer upstream (legacy) | sem cap `PERMISSIONS` | fluxo normal de aprovação, 1× | sim | `README.md:817-836`; `lan-mouse-proto/src/lib.rs:42-51, 139-160` | Manter |

Persistência atual: `config.toml` (devices/rules/denied), `audit.jsonl` (estado), tokens de portal
(cache), chave/cert (config), TCC/firewall (OS). `src/audit.rs:60-87` já usa `XDG_STATE_HOME`.

## 3. Classificação

**(a) Inevitáveis por design** — L1 (InputCapture v1 sem persist), L5/L6 (TCC exige UX do OS), L7
(sem notarização), L8 (UAC), indicador persistente "remote desktop ativo" do GNOME `[H6]`. Em (a) só
se reduz frequência.

**(b) Persistíveis corretamente (e hoje não estão ou estão no lugar errado)** — L2b (reusar token),
L3/L2 (mover de `~/.cache` para `XDG_STATE_HOME`, 0600, atomicamente — cache é apagável), L5 (com
identidade de assinatura estável), L9 (regra de firewall no instalador já cobre private/domain;
Public precisa decisão explícita), A3 (persistir review), onboarding de autostart (systemd unit no
`.deb`/Windows startup shortcut já existe: `nsi:75-77`).

**(c) Reduzíveis no app** — D1/D2 abaixo; recriação de sessão de captura (L1) por mudança de
cliente/zona; inicialização ansiosa dos backends (capture/emulation sobem no start mesmo sem nenhum
device: `src/capture.rs:369-372, 401-408`, `src/emulation.rs:546-572`) → prompts antes do 1º
pareamento; onboarding único explicando "≤3 diálogos do OS e por quê"; consolidar as **duas** sessões
RemoteDesktop (emulação e clipboard) em uma, se o portal permitir `[H4]`; doctor exibindo estado de
cada consent (já faz para clipboard: `src/doctor.rs:145-158`).

**(d) Bugs de repetição** — D1: modal reaberto a cada ~2 s enquanto o peer tenta (dedupe de 2 s em
`src/emulation.rs:393-397`; retry 250 ms–5 s em `src/connect.rs:56-57`; GTK abre/refecha o modal a
cada `ConnectionAttempt`: `lan-mouse-gtk/src/lib.rs:376-384`, `window.rs:1578-1600`) — reseta
seleção/foco. D2: device **negado/revogado** continua gerando `Rejected` → `ConnectionAttempt` →
modal; o deny só corta o handshake (`listen.rs:509` registra antes do deny em `512-519`; `331-338`
emite `Rejected`; handler do service não filtra negados: `src/service.rs:686-701`). D3: sessão
InputCapture recriada a cada `create`/`destroy` de cliente e mudança de zona → novo diálogo `[H1]`.
D4: fallback Xdp sem token (L2b). D5: tokens em `~/.cache` e 0644 (`portal.rs:424-431`,
`input-emulation/src/libei.rs:77-85`). D6: daemon morre com a GUI → prompts a cada reabertura
(`src/main.rs:98-117`). D7: macOS ad-hoc → re-grant por update `[H2]`.

## 4. Riscos de silenciar demais / princípios

- **Não** auto-aprovar device desconhecido por beacon/descoberta: o fingerprint anunciado por UDP
  não prova posse de chave (hoje "add from discovery" já autoriza o fingerprint anunciado:
  `lan-mouse-gtk/src/window.rs:489-510`); manter SAS + matriz de caps no 1º pareamento.
- **Não** transformar pairing em "sempre aberto" nem em default: single-use + review (A3).
- **Não** conceder escopo novo sem prompt (escalation deve manter `PendingView`).
- **Não** trocar fingerprint por IP: address rules com caps explícitas, aviso para prefixos amplos
  (`README.md:559-563`) e deny > rule > pairing (`listen.rs:512-530`).
- **Não** persistir tokens com permissão de grupo/mundo, nem em `export-policy` (policy não inclui
  tokens; manter assim: `README.md:592-604`).
- **Não** rodar elevado no Windows p/ evitar UAC, remover checagem de TCC ou pedir `xattr -rd` como
  solução permanente (notarizar). Clipboard: consent do portal é do usuário; manter token quitado ao
  app e o toggle explícito, sem leitura contínua silenciosa sem indicador de estado.

## 5. Proposta priorizada (com esforço e critério de aceite)

1. **P0 (S) — D1+D2: parar o "prompt storm" do app.** Deduplicar modal por fingerprint (atualizar
   contador `attempts`, não recriar janela); não emitir `ConnectionAttempt`/modal para fingerprint
   na deny-list (filtrar no handler do service antes de `pending_upsert`; auditar 1×). Aceite: device
   desconhecido tentando 60 s → **1** janela; device negado → **0** janelas e 1 linha de auditoria.
2. **P0 (S) — persistir consents de portal corretamente.** `XDG_STATE_HOME/lan-mouse/*.token` com
   0600/escrita atômica (migrar de `~/.cache`), reutilizar token no backend Xdp e no clipboard;
   doctor passa a mostrar consent de captura/emulação/clipboard (estender `doctor.rs:145-158`).
   Aceite: restart do daemon após 1º consent → 0 diálogos; limpar `~/.cache` → sem regressão.
3. **P1 (S) — D6: desacoplar daemon da GUI.** Instalar/habilitar systemd user unit por default no
   `.deb` (unit já existe: `service/lan-mouse.service`) ou opção "continuar em segundo plano" e
   reconectar à instância existente (`src/main.rs:98-104`). Aceite: abrir/fechar a GUI 10× → 0 novos
   diálogos de captura; `doctor` indica daemon gerenciado externamente.
4. **P1 (M) — L1/D3 no GNOME/KDE:** não recriar sessão a cada mudança de cliente; debounce/agrupar
   atualizações e usar atualização de barreiras no portal se disponível; documentar que v1 = 1
   diálogo/login (`README.md:801-812`) e abrir issue upstream p/ persist. Aceite: adicionar/remover
   2 clientes + hotplug de monitor → 0 diálogos (ou ≤1/login); medir `CreateSession` por login.
5. **P1 (M) — L5: identidade de assinatura estável no macOS.** Job macOS no release com
   Developer ID + notarização (ou cert self-signed persistente documentado para builds locais);
   manter `Info.plist`/usage strings (`build-aux/macos-lsui-element.plist`; `Cargo.toml:116`).
   Aceite: atualizar o `.app` mantendo o mesmo bundle id → AX preservado (sem re-toggle); artefato
   assinado verificável.
6. **P2 (S) — onboarding único + Windows Public profile** `[H3]`: painel de 1ª execução explica os
   até 3 diálogos do OS e onde cada consent persiste; inicialização preguiçosa dos backends (não pedir
   consent antes do 1º device). Windows: testar os 3 perfis de rede e oferecer opt-in explícito para
   Public no instalador. Aceite: perfil limpo → 0 diálogos até o 1º pareamento; inbound ok nos 3
   perfis; `doctor --json` expõe 1 campo por consent e por regra de firewall.
7. **P3 (M/oportunista) — consolidar as 2 sessões RemoteDesktop** `[H4]` e verificar se o Clipboard
   portal exige seleção teclado/pointer `[H5]`.

## 6. Critérios de aceite e validação por OS

- **Ubuntu 24.04/25.10 (GNOME Wayland) e KDE 6 Wayland**: 1º run ≤3 diálogos com onboarding;
  restart do daemon → 0; add/remove cliente + hotplug → 0 `[H1]`; testar token presente × ausente.
- **Ubuntu X11 / wlroots**: 0 prompts de OS; regressão de que nenhum aviso novo aparece.
- **macOS 14/15 (arm+intel)**: 1º run = 1 alerta AX; grant → relaunch → 0; rebuild/update do mesmo
  bundle preserva AX (critério do item 5); quarentena eliminada por notarização.
- **Windows 10/11**: 1 UAC no install; 0 prompt de firewall no 1º run; uninstall remove regra;
  perfis private/domain/public documentados e diagnosticados.
- **Transversal**: contador de diálogos por cenário (matriz manual + teste de integração); unit de
  token (path `XDG_STATE_HOME`, 0600, migração), unit do filtro de negados, teste GTK de que retry
  não recria o modal, `tests/packaging.rs` cobrindo systemd unit no `.deb` e firewall, assinatura
  macOS verificada no CI.

## 7. Hipóteses a validar (marcadas no texto)

- `[H1]` cada `CreateSession` do InputCapture v1 abre diálogo no GNOME/KDE (o `README.md` afirma;
  medir recriações por uso típico). `[H2]` ad-hoc/strip faz TCC perder grant a cada update (validar
  build assinado × ad-hoc no mesmo bundle id).
- `[H3]` Windows Public sem regra: bloqueio silencioso × prompt; SmartScreen sem EV certificate.
- `[H4]` uma sessão RemoteDesktop para emulação+clipboard. `[H5]` `select_devices(Keyboard|Pointer)`
  é obrigatório só para clipboard. `[H6]` GNOME mostra indicador persistente não removível.

## 8. O que NÃO silenciar

1º pareamento (SAS + capacidades), troca de certificado, escalonamento de escopo, deny/revogação,
consent de clipboard do OS, auditoria de quem controlou/quando, e avisos de address rules amplas.
Esses são os pontos onde o silêncio vira risco de segurança; o resto é atrito removível.
