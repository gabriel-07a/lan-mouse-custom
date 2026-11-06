# Auditoria de permissões macOS (TCC) — lan-mouse-custom

> Escopo: base limpa em `ef5f41f` (2026-11-05), auditoria somente leitura. Nenhum código, workflow ou assinatura foi alterado.
> Método: leitura do código, dos metadados de bundle e do CI. Não houve execução em macOS; o que depende do comportamento do sistema está marcado `H*` e listado na §7.
> Base Apple: [TN3179 — local network privacy](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy), [notarização](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution), [PPPC](https://developer.apple.com/documentation/devicemanagement/privacy-preferences-policy-control), [Quartz Event Services](https://developer.apple.com/documentation/coregraphics/quartz-event-services).

## 1. Resumo executivo

- O app pede **Accessibility (AX)** explicitamente; **Input Monitoring** e **Post Event** são registrados/checados, mas o código assume que o grant de AX os cobre no macOS 13+ (`lan-mouse-gtk/src/macos_privacy.rs:3-8`). Essa premissa não foi testada aqui (`H1`).
- **Automation** não é solicitada em runtime: não há envio de Apple Events; existe só um handler de `kAEReopenApplication` (`lan-mouse-gtk/src/macos_status_item.rs:244-271`) e uma `NSAppleEventsUsageDescription` enganosa (`build-aux/macos-lsui-element.plist:7-8`).
- **Screen Recording** não é usada nem declarada: não há API de captura de tela no repositório. Ela não deve ser pedida.
- **Local Network** é usada de fato (UDP 4245 de descoberta + porta de controle DTLS/UDP), mas não há `NSLocalNetworkUsageDescription` nem tratamento de negação; no macOS 15+ isso pode virar um prompt/negativa não gerenciado (`H3`).
- O risco P0 é de distribuição: o release só publica Linux e Windows (`.github/workflows/release.yml:21-23,52-53,114-116`), não há codesign/notarização/entitlements, e o README manda remover o quarantine (`README.md:130`). Isso produz TCC frágil e fricção de Gatekeeper.

## 2. Inventário: o que é pedido e onde

### 2.1 Accessibility (AX)

- Prompt: só a GUI, uma vez por processo, em `fire_initial_prompts` (`lan-mouse-gtk/src/macos_privacy.rs:260-263`), via `AXIsProcessTrustedWithOptions` com `kAXTrustedCheckOptionPrompt` (`:276-289`); chamado no startup da GUI (`lan-mouse-gtk/src/lib.rs:301-302`).
- Checagem silenciosa no daemon: captura (`input-capture/src/macos.rs:747-752`) e emulação (`input-emulation/src/macos.rs:174-177`). A falha vira erro tipado (`input-capture/src/error.rs:152-155`; `input-emulation/src/error.rs:149-152`).
- Uso real: tap ativo `Session`/`HeadInsertEventTap`/`Default` (`input-capture/src/macos.rs:560-567`), com recuperação de timeout (`:459-479`) e tratamento de revogação (`:482-505`). A GUI monitora AX a cada 1 s (`lan-mouse-gtk/src/macos_privacy.rs:97-124`) e encerra o app se o grant for revogado (`lan-mouse-gtk/src/lib.rs:321-325`).

### 2.2 Input Monitoring e Post Event

- Input Monitoring: `CGRequestListenEventAccess` + tap de sessão descartável para forçar registro no painel (`lan-mouse-gtk/src/macos_privacy.rs:211-230`); preflight no daemon de captura (`input-capture/src/macos.rs:755-758`; extern em `:831`).
- Post Event: `CGRequestPostEventAccess` (`lan-mouse-gtk/src/macos_privacy.rs:302-307`); preflight na emulação (`input-emulation/src/macos.rs:180-182`; extern em `:184-187`); os eventos são postados em `CGEventTapLocation::HID` (`:224, :249, :427, :507, :541, :572`).
- A ordem é deliberada: sem AX, a GUI só dispara o prompt de AX e não tenta registrar IM/Post Event, para não empilhar diálogos (`lan-mouse-gtk/src/macos_privacy.rs:266-290`).

### 2.3 Automation, Screen Recording e Local Network

- Automation: nenhum `NSAppleScript`, `AEDeterminePermissionToAutomateTarget` ou envio de AE no repositório; o handler de reopen não exige Automation. A chave do plist é morta e a string é factualmente errada (“Apple Events to deliver synthesized keyboard and mouse events”).
- Screen Recording: ausência de `CGWindowListCreateImage`, `CGDisplayStream`, `SCShareableContent` ou `NSScreenCaptureUsageDescription`; nenhum prompt esperado.
- Local Network: descoberta por broadcast UDP em `src/discovery.rs:33,341-357,416-424`; listener em `src/listen.rs:565-571`; UDP de controle em `src/connect.rs:442,492`. Não há `NSLocalNetworkUsageDescription` no plist; o único plist, incluído em `info_plist_exts` (`Cargo.toml:115-116`), só tem `LSUIElement`, `NSAppSleepDisabled`, `NSInputMonitoringUsageDescription` e `NSAppleEventsUsageDescription` (`build-aux/macos-lsui-element.plist:1-8`).

### 2.4 GUI × daemon: onde cada coisa roda

- Sem argumentos, a GUI sobe e lança o daemon como filho do **mesmo executável** (`src/main.rs:93-118` e `:151-161`); com `daemon`, só o serviço roda (`src/main.rs:83-91`). Logo, no caminho normal, GUI e daemon têm o mesmo binário e o mesmo bundle — não são identidades TCC separadas por design. Ver `H4`.
- Só a GUI chama as APIs que mostram prompt (`AXIsProcessTrustedWithOptions`, `CGRequestListenEventAccess`, `CGRequestPostEventAccess`). O daemon nunca chama request/prompt: faz preflight e, se autorizado, executa a operação protegida; se a captura/emulação falha, a task registra o erro e fica esperando `Reenable` (`src/capture.rs:369-405`; `src/emulation.rs:545-569`; pedido em `src/service.rs:460-461`).
- Contextos independentes existem: um daemon via `launchd` (`nix/hm-module.nix:63-71`) roda fora do `.app`, sem GUI e sem prompt; `relaunch_bundle` só funciona em `<bundle>.app/Contents/MacOS` (`lan-mouse-gtk/src/macos_privacy.rs:136-178`). O próprio código documenta herança de grant de um processo pai com IM, ex. Terminal (`lan-mouse-gtk/src/macos_privacy.rs:292-297`).

## 3. Prompts inevitáveis × evitáveis

### 3.1 Inevitáveis por desenho da Apple

- **Primeira execução**: o primeiro acesso a cada serviço protegido (AX, IM, Post Event e, se usados, Automation/Screen Recording/Local Network) é prompt do sistema; o app não pré-concede nada.
- **Negação**: depois que o usuário nega, o sistema não repete o alerta; o app só pode abrir System Settings (é o que faz em `lan-mouse-gtk/src/window/imp.rs:260-261` e `lan-mouse-gtk/src/macos_privacy.rs:126-128`) ou depender de `tccutil reset`/MDM.
- **TCC reset**: apagar a entrada (`tccutil reset ...`, citado em `lan-mouse-gtk/src/macos_privacy.rs:202-208`) faz o prompt voltar; é ação de usuário/admin, não do app.
- **Mudança de identidade**: bundle id ou assinatura diferentes fazem o TCC tratar como outro app; grants antigos ficam órfãos. Assinatura Developer ID estável + bundle id fixo evitam isso; notarização por si só não muda o grant.
- **Revogação em sessão**: o grant pode sumir a qualquer momento; o tap é desabilitado (`input-capture/src/macos.rs:482-505`) e a GUI encerra (`lan-mouse-gtk/src/lib.rs:321-325`). O app não consegue restaurar o grant sozinho.
- **Relaunch após grant**: o código assume que o processo que já falhou precisa de um novo processo (`lan-mouse-gtk/src/lib.rs:303-306`; `lan-mouse-gtk/src/window.rs:1523-1529`). Se o TCC realmente cacheia por processo, é inevitável (`H9`).

### 3.2 Evitáveis pelo código (e o que o código já evita)

- **Prompts empilhados**: a sequência AX → (relaunch) → IM/Post Event evita dois alertas de AX simultâneos (`lan-mouse-gtk/src/macos_privacy.rs:266-290`) — correto.
- **Re-prompt por ativação**: `Once` (`:260-263`) e navegação por URL em retries (`:250-254`) — correto.
- **Prompt a partir do daemon**: preflights silenciosos (`input-capture/src/macos.rs:747-757`; `input-emulation/src/macos.rs:174-181`) — correto; o daemon nunca deve chamar as APIs de request.
- **Permissões desnecessárias**: declarar Apple Events e Screen Recording é evitável; a de Local Network está faltando (não é “evitar”, é declarar).
- **Re-prompt em updates**: build ad-hoc/unsigned muda a identidade a cada rebuild; assinatura estável resolve (`H2`). O repo não tem passo de assinatura (`README.md:244-254`; `Cargo.toml:108-116`).
- **Orientação errada quando só IM/Post Event cai**: a UI colapsa os três serviços em uma linha e decide pelo AX (`lan-mouse-gtk/src/window.rs:1486-1536`, especialmente `:1523-1534`); se IM ou Post Event forem revogados com AX ainda true, o texto “relaunch required” não corrige o problema (`H5`).
- **Prompt sem explicação**: hoje o alerta de AX dispara no startup, antes de qualquer onboarding; uma tela única explicando os três grants e o relaunch reduziria negações e retries.

### 3.3 GUI × daemon são contextos TCC distintos?

Não como identidade, no caminho normal: a GUI lança o mesmo binário do mesmo bundle (`src/main.rs:151-153`), então o TCC deve ver o mesmo app. Mas são **processos e “responsible processes” distintos**: só a GUI mostra prompt; o daemon só checa. Quando o daemon é lançado por `launchd`/Nix (`nix/hm-module.nix:63-71`) ou por um Terminal já confiável, a atribuição pode mudar (herança documentada em `lan-mouse-gtk/src/macos_privacy.rs:292-297`). O mecanismo exato de “responsibility” é interno da Apple; ver `H4`.

## 4. Riscos

- **Distribuição sem assinatura/notarização**: não há job macOS de release (`.github/workflows/release.yml:21-23,52-53,114-116`), nem `codesign`, `notarytool`, entitlements ou hardened runtime no repo; o CI macOS só faz build/check/clippy/test (`.github/workflows/rust.yml:30-41,79-101`). O README instrui `xattr -rd com.apple.quarantine` (`README.md:130`), sinal clássico de app não notarizado; usuários são instruídos a contornar o Gatekeeper.
- **Identidade TCC instável**: rebuilds ad-hoc/unsigned podem gerar prompts repetidos ou entradas duplicadas; grants não sobrevivem a mudança de assinatura/bundle id (`H2`). Isso afeta atualização e suporte.
- **Bundle id compartilhado com o upstream**: `de.feschber.LanMouse` (`Cargo.toml:111`; `lan-mouse-gtk/src/lib.rs:89-91`) é o id do projeto upstream (`git remote -v`); duas cópias instaladas podem confundir entradas/grants e um perfil PPPC do upstream pode não casar com o fork (`H8`).
- **Launch agents / daemon sem GUI**: `launchd` inicia `bin/lan-mouse daemon` fora do `.app` (`nix/hm-module.nix:63-71`); não há prompt, e o caminho do Nix store muda a cada rebuild. `relaunch_bundle` falha fora de `<bundle>.app/Contents/MacOS` (`lan-mouse-gtk/src/macos_privacy.rs:154-178`). O `LAN_MOUSE_HIDDEN` (`lan-mouse-gtk/src/lib.rs:489-499`) não é configurado pelo módulo.
- **Corporativo/MDM/PPPC**: PPPC casa por identifier + code requirement/Team ID; sem Developer ID, na prática não há requisito estável para pré-aprovar AX/IM/Post Event, e Macs gerenciados podem bloquear o toggle manual. Usuários sem admin ficam sem saída.
- **Local Network silenciosa**: sem usage description e sem detecção de negação; descoberta e listener podem falhar em Wi-Fi corporativo/AP isolation/VPN, e o app não distingue “sem rede” de “sem permissão” (`H3`).
- **Superfície de segurança do tap**: tap ativo em `HeadInsertEventTap` pode “engolir” input se o processo travar; o código mitiga timeout e revogação (`input-capture/src/macos.rs:459-505`), mas o risco operacional existe.
- **Orientação de UI enganosa**: `Accessibility granted — restart` é exibido mesmo se o bloqueio real for IM/Post Event (`lan-mouse-gtk/src/window.rs:1523-1529`; `H5`).

## 5. Recomendações priorizadas

**P0 — destravar a distribuição e a identidade TCC**

1. Criar job macOS no release: build assinado com Developer ID Application, `--options runtime` (hardened runtime), notarização (`notarytool submit --wait`), `stapler staple`, verificação com `spctl -a -vvv` e publicação de Intel+ARM. Fixar bundle id e Team ID. Remover `xattr -rd com.apple.quarantine` do README (`README.md:128-133`) e documentar o Gatekeeper normal.
2. Corrigir o Info.plist: adicionar `NSLocalNetworkUsageDescription` (texto específico: descoberta e controle na LAN); remover `NSAppleEventsUsageDescription` enquanto não houver envio de AE (ou corrigir a string, se for adicionar); manter `NSInputMonitoringUsageDescription`; **não** adicionar Screen Recording (`build-aux/macos-lsui-element.plist:1-8`).
3. Onboarding único na GUI: apresentar/ativar a janela antes do prompt (`lan-mouse-gtk/src/lib.rs:302` vs `:496-499`), explicar AX/IM/Post Event e o relaunch, e disparar o prompt só depois do “Continuar”. Manter o `Once` e nunca chamar request no daemon.
4. `doctor`: incluir `macos_permissions` no relatório (`AXIsProcessTrusted`, `CGPreflightListenEventAccess`, `CGPreflightPostEventAccess`), caminho do bundle, bundle id, se está dentro de `.app` e se o contexto é GUI/daemon/Terminal; corrigir o docstring “may prompt” (`src/doctor.rs:16-17`) — os preflights são silenciosos (`:297-331`). Emitir warning específico para “grant herdado do Terminal” e para “daemon sem GUI”.

**P1 — reduzir re-prompt e suporte**

5. Testar se, após o grant, `FrontendRequest::EnableCapture/EnableEmulation` (`src/service.rs:460-461`) reinicializa os backends sem relaunch completo; se o TCC exigir processo novo, manter o relaunch e documentar. Isso pode eliminar um restart do onboarding.
6. Separar o status de AX/IM/Post Event na UI e abrir o painel correto para cada um; a linha única atual (`lan-mouse-gtk/src/window.rs:1486-1536`) só deve prometer “relaunch” quando AX for o bloqueio.
7. Logar no startup, sem dados sensíveis: bundle path/id, `AXIsProcessTrusted`, os dois preflights e o resultado de `CGEventTapCreate`; isso encurta o diagnóstico de “funciona no Terminal, não no Finder”.
8. Documentar MDM/PPPC com um perfil de exemplo para Developer ID (Accessibility, ListenEvent, PostEvent; ScreenCapture não se aplica). Deixar claro que daemon-only não faz onboarding e que o LaunchAgent deve ser instalado depois do primeiro grant.

**P2 — higiene**

9. Testar a matriz: usuário limpo, negação, revogação em sessão, update de assinatura, `tccutil reset`, launchd, Terminal e macOS 13/14/15/26; registrar em `docs/` os resultados (`H1`–`H9`).
10. Adicionar tratamento explícito para falha de Local Network (bind/broadcast) com mensagem de firewall/VPN, em vez de só `warn!` (`src/discovery.rs:416-421`).

**O que é impossível**: evitar o primeiro prompt; customizar o alerta além do texto de usage description; conceder AX/IM/Post Event programaticamente; reabrir o prompt após negação sem ação do usuário/MDM; preservar grant após mudança de bundle id/assinatura; fazer um daemon sem GUI mostrar prompt; pré-checar Local Network por API pública (`H3`); notarizar sem credenciais Developer ID; e usar Screen Recording como atalho para input — não é necessário e não substitui AX/IM.

## 6. Tabela permissão → quem pede → persistência → reduzir re-prompt → riscos

| Permissão | Quem pede (código) | Persistência | Como reduzir re-prompt | Riscos |
|---|---|---|---|---|
| Accessibility | GUI: `AXIsProcessTrustedWithOptions` (`lan-mouse-gtk/src/macos_privacy.rs:276-289`); checks: `input-capture/src/macos.rs:747-752`, `input-emulation/src/macos.rs:174-177` | TCC por usuário, ligado à identidade bundle/assinatura; sobrevive a restart; reset por `tccutil`/MDM | Developer ID estável; 1 prompt + onboarding; `Once`; abrir Settings em vez de re-promptar; PPPC | Rebuild ad-hoc muda identidade; revogação em sessão; tap ativo |
| Input Monitoring | GUI: `CGRequestListenEventAccess` + tap (`lan-mouse-gtk/src/macos_privacy.rs:211-230`); preflight (`input-capture/src/macos.rs:755-758`) | Mesma base; em versões antigas é painel separado | Premissa AX⊃IM (`H1`); se falhar, grant guiado + relaunch; assinatura | UI só olha AX; daemon sem GUI; TCC reset |
| Post Event | GUI: `CGRequestPostEventAccess` (`lan-mouse-gtk/src/macos_privacy.rs:302-307`); preflight (`input-emulation/src/macos.rs:180-182`) | Mesma base; categoria Accessibility nas versões recentes | Coberto por AX (`H1`); assinatura; status separado | Emulação falha sem aviso claro; UI cega (`H5`) |
| Automation | Nenhum envio de AE; só handler de reopen (`lan-mouse-gtk/src/macos_status_item.rs:244-271`) e string no plist (`build-aux/macos-lsui-element.plist:7-8`) | Não se aplica (nenhum prompt) | Remover a chave/string enganosa | Auditoria/compliance; prompt futuro se AE for adicionado |
| Screen Recording | Nenhum (sem APIs de captura) | Não se aplica | Não pedir; não declarar | Usuário pode conceder por engano; ruído de suporte |
| Local Network | Implícito nos sockets (`src/discovery.rs:33,341-357,416-424`; `src/listen.rs:565-571`; `src/connect.rs:442,492`) | TCC do macOS 15+ por app/usuário (`H3`) | `NSLocalNetworkUsageDescription`; explicar; tratar negação; testar | Sem texto/erro; Wi-Fi corporativo/VPN; daemon |

## 7. Hipóteses não confirmadas

- `H1` — “AX concede IM e Post Event no macOS 13+” (`lan-mouse-gtk/src/macos_privacy.rs:3-8`). Não testado; se falso, a UI de linha única e o onboarding estão errados em versões específicas.
- `H2` — comportamento de identidade TCC para builds ad-hoc/unsigned: se cada rebuild/atualização re-promptar, se entradas antigas ficam órfãs e se `tccutil reset` é o único caminho. Requer teste A/B de assinatura.
- `H3` — Local Network no macOS 15+: se o prompt aparece para este app/daemon, se `NSLocalNetworkUsageDescription` é obrigatório, se broadcast/UDP disparam a permissão e se há API de preflight. Não há código de tratamento no repo.
- `H4` — atribuição de “responsible process” entre GUI (pai) e daemon (filho), e entre launchd/Terminal; o comentário de herança (`lan-mouse-gtk/src/macos_privacy.rs:292-297`) sugere o comportamento, mas não foi reproduzido.
- `H5` — se IM/Post Event podem ser revogados independentemente com AX ainda true; a UI hoje assume que não.
- `H6` — `NSAppleEventsUsageDescription`/handler de reopen não geram prompt de Automation; o handler só recebe `kAEReopenApplication`, mas não foi validado em usuário limpo.
- `H7` — prompt de AX antes de `window.present()` com `LSUIElement=true` pode ser perdido ou não trazer o app para frente; testar em GUI real.
- `H8` — colisão de TCC/PPPC entre fork e upstream pelo mesmo bundle id `de.feschber.LanMouse`; e se um perfil PPPC do upstream casa com o fork.
- `H9` — se `EnableCapture/EnableEmulation` reinicializa o backend após o grant sem relaunch, ou se o TCC exige processo novo (o código atual aposta no relaunch).

## 8. Evidência negativa (o que não existe no repo)

- Sem `codesign`, `notarytool`, `stapler`, `.entitlements` ou hardened runtime; `cargo bundle` é o único empacotador citado (`README.md:244-254`).
- Sem `NSLocalNetworkUsageDescription` e sem `NSScreenCaptureUsageDescription` no único plist (`build-aux/macos-lsui-element.plist:1-8`).
- Sem `CGWindowListCreateImage`, `CGDisplayStream`, `SCShareableContent`, `NSAppleScript` ou envio de Apple Event.
- Sem job macOS em `.github/workflows/release.yml`; macOS aparece só na matriz de CI (`.github/workflows/rust.yml:30-41`).
