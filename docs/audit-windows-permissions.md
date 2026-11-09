# Auditoria Windows — UAC, firewall, ACL, instalador e diagnose do smoke test
> **TL;DR** — O app exige admin **uma vez** para instalar (Program Files + HKLM + firewall) e depois
> roda sem admin, mas: (1) a regra de firewall cobre só `private,domain` e é re-adicionada sem
> `delete` em upgrades (`installer/windows/lan-mouse-custom.nsi:71`); (2) instalador/exe não são
> assinados → SmartScreen/EDR (`release.yml` sem signing/checksums); (3) a ACL da chave é `icacls`
> *best-effort* sem verificação/rollback (`src/crypto.rs:97-117`); (4) o release é **GUI subsystem** e
> o attach de console é só `AttachConsole` (`src/main.rs:1-5,188-198`) → saída de `doctor/cli` no
> terminal é frágil; (5) janelas elevadas exigem rodar elevado e o *secure desktop* é inalcançável
> (UIPI) — não documentado. O passo **"Smoke test silent installer and doctor probe" falha
> intermitentemente e sem diagnóstico**; a evidência descarta "backend errado" e deixa hipóteses
> release-only (§3). Auditoria somente leitura; nada alterado/commitado.

Escopo: `main` @ `ef5f41f`. Lidos: `installer/windows/*`, `.github/workflows/{release,rust}.yml`,
`src/{main,crypto,config,doctor}.rs`, `input-capture/src/windows/**`, `input-emulation/src/windows.rs`,
`lan-mouse-ipc/src/{lib,listen}.rs`, `tests/{packaging,windows_input}.rs`, `README.md`, `DOC.md`, `build.rs`.
Outros agentes editaram `src/doctor.rs`/`input-*/src` durante a auditoria (unix-only). CI via API pública (§3.1).

## 1. Matriz prompt/atrito → causa → persistência possível → recomendação
| Atrito | Quando | Pode silenciar/persistir? | Recomendação | Evidência |
|---|---|---|---|---|
| UAC na instalação/upgrade/uninstall | 1× por operação (per-machine) | Não p/ Program Files/HKLM/netsh; alternativa = per-user | Manter; oferecer "só para mim" ou documentar o ZIP portátil | `nsi:21,23,62-67,71` |
| UAC no "Launch Lan Mouse" da página final | install interativa | Sim (desmarcar default) | Filho herda token elevado → GUI+daemon como admin sem intenção | `nsi:31-32` |
| Firewall: prompt/bloqueio em rede Public | 1º bind do listener UDP 4242/4245 | Sim (melhorar regra); Public exige decisão | Regra cobre `private,domain`; Public fica bloqueado → opt-in + doctor mostrar a regra | `nsi:71`; `README.md:354` |
| Regras de firewall duplicadas em upgrades | upgrade | Sim | `netsh delete rule` antes do `add` (ou `set rule`); remover por programa no uninstall | `nsi:43-46,71,98` |
| SmartScreen ("Windows protegeu seu PC") | download+execução | Só com Authenticode + reputação | Assinar instalador/exe com timestamp; publicar SHA-256; documentar "Mais informações → Executar" | `release.yml` (sem signing/checksums) |
| Defender/EDR (hooks + `SendInput`) | execução/install | Parcial (assinatura, exclusões corporativas) | Assinar; documentar falso-positivo | `event_thread.rs:320-326`; `input-emulation/src/windows.rs:295` |
| Janelas elevadas (UIPI)/secure desktop | app admin em foco / UAC / lock | Só elevando o processo no PC receptor; secure desktop nunca | Documentar limitação; opção "reiniciar como admin"; **não** elevar por padrão | hooks/`SendInput` (OS) |
| Console piscando / saída invisível no terminal | CLI/doctor e spawn do daemon | Sim | `AttachConsole` sozinho não restaura std handles do CRT → `SetStdHandle`/`CONOUT$` ou binário CLI console-subsystem; daemon já usa `CREATE_NO_WINDOW` | `src/main.rs:1-5,151-161,188-198`; `crypto.rs:108` |
| DPI inconsistente (multi-monitor) | telas com escalas diferentes | Sim | Manifest PMv2 + doctor reportando DPI/layout; hoje erro de API ignorado e fallback escala 1.0 | `src/main.rs:169-181`; `input-emulation/src/windows.rs:193-205` |
| ACL da chave privada | 1ª geração do cert | Sim (sem UAC; dono altera DACL) | Verificar/rollback; conceder por SID; documentar reset; manter per-user | `src/crypto.rs:97-117,119-125,152-153` |
| Autostart (login) e daemon iniciado pelo GUI | login/execução | Sim (atalho Startup) | Autostart roda **não elevado**; daemon usa o mesmo token do GUI e morre com ele (sem serviço Windows) | `nsi:75-77`; `src/main.rs:99-118` |
| IPC do frontend `127.0.0.1:5252` (TCP, sem ACL/auth) | sempre no Windows | Parcial | Qualquer processo local manda `FrontendRequest`; migrar p/ named pipe com DACL do usuário | `lan-mouse-ipc/src/listen.rs:18-20,50-56`; `lib.rs:858-862` |
| Usuários sem admin | instalação per-machine | Só per-user/portátil | ZIP portátil evita UAC, mas não cria firewall/autostart | `release.yml:106-112`; `nsi:21,23` |

**Persistível sem elevação:** config/chave em `%LOCALAPPDATA%\lan-mouse` (`src/config.rs:48-63`), ACL
da chave pelo dono, atalho de autostart, atalhos do usuário. **Com uma elevação (legítimo):** regra
de firewall, chaves HKLM de uninstall, arquivos em Program Files. **Exige elevação por design:**
input em janelas elevadas (UIPI) e o *secure desktop* (UAC/lock/Ctrl+Alt+Del — impossível para app
user-mode, mesmo elevado; só UIAccess assinado pela MS). **Não silenciável legitimamente:**
SmartScreen sem assinatura/reputação e o UAC da instalação per-machine.

## 2. Achados
- **[P1] Firewall: blind spot + duplicação + falha silenciosa.** `profile=private,domain` deixa
  Public bloqueado → sintoma "conecta só de um lado"; `add rule` roda a cada install sem remover a
  anterior (`nsi:43-46,71`) e nomes de regra do `netsh` não são únicos → regras acumulam em upgrades;
  falha do `netsh` é ignorada (`nsExec::ExecToLog` sem checagem) e o install termina OK.
- **[P1] ACL da chave best-effort, sem rollback.** `icacls /inheritance:r /grant:r {USER}:R`
  (`src/crypto.rs:103-109`) concede só leitura ao `USERNAME`; se a resolução do nome falhar (conta
  renomeada/domínio/nome incomum) a falha só é logada (`:110-116`) e o arquivo pode ficar sem ACE de
  leitura → próximo start falha em `load_or_generate_key_and_cert` (`:119-125`). Amarra ao modelo
  per-user (serviço/SYSTEM/outra conta não lê a chave). Sem teste Windows (só Unix `:261-272`).
- **[P1] "Launch Lan Mouse" da página final roda elevado** (`nsi:31-32`; marcado por padrão):
  o filho herda o token elevado do instalador → GUI **e** daemon como admin sem UAC extra, em
  desacordo com o autostart não elevado (`nsi:75-77`). Usar `MUI_FINISHPAGE_RUN_NOTCHECKED`.
- **[P1] Saída de `doctor`/`cli` no release é frágil:** GUI subsystem (`src/main.rs:1-5`);
  `attach_parent_console` só chama `AttachConsole` (`:188-198`) e não restaura std handles do CRT →
  saída pode sumir no terminal; em pipeline do PowerShell depende de handles herdados/redirecionados
  (H1). Fix: `SetStdHandle`/`CONOUT$` ou binário CLI console-subsystem.
- **[P2] DPI sem manifest:** `build.rs:70-83` só embute ícone/versão; PMv2 depende de
  `SetProcessDpiAwarenessContext` cujo erro é descartado (`src/main.rs:169-181`); se o awareness já
  veio por manifest/compat, `GetDpiForMonitor` devolve 96 e a emulação cai para 1.0 sem erro visível
  (`input-emulation/src/windows.rs:193-205`) → ponteiro dessincronizado em mixed-DPI sem diagnóstico.
- **[P2] UIPI/secure desktop não documentados** (`README.md:354` só fala de porta); **IPC Windows sem
  controle de acesso** (`lan-mouse-ipc/src/listen.rs:18-20,50-56`, perigoso com daemon elevado);
  **testes fracos** (`tests/packaging.rs:23-63` só `contains()`; `tests/windows_input.rs` condiciona
  asserções a `cursor_showing` `:85-86,125,191` e pode passar vazio); `DOC.md:196` diz que o
  `release.yml` "verifica" doctor/uninstaller, mas o passo falha.

## 3. Falha do passo "Smoke test silent installer and doctor probe"
### 3.1 Evidência (dados públicos; logs de job exigem auth — API devolve 403)
- Passo 8 do job `windows-release-build` falha; anotação pública traz só
  `"Process completed with exit code 1."` (sem a mensagem do `throw`). Run atual:
  `actions/runs/42130635491` (HEAD `ef5f41f`; passo ~9 s).
- Conclusão do job (API): **passa** em `ea66969` (`runs/42094273684`), `07ff23b`
  (`runs/42094648530`, passo ~8 s) e `c2b9618` (`runs/42095719382`, ~7 s); **falha** em `853387c`
  (`runs/42073142339`), `68d18bf` (`runs/42096431697`, ~21 s), `909cc41` (`runs/42096942763`, ~21 s),
  `fac04c1` (`runs/42097609629`), `3ac6e9f` (`runs/42104456239`, ~14 s), `7e3e11c`
  (`runs/42125290478`, ~9 s) e `ef5f41f` (~9 s). ⇒ **não é regressão determinística de um commit**.
- No HEAD o CI Rust (`rust.yml:83-88`) roda `cargo build` **debug** + `./target/debug/lan-mouse.exe
  doctor --probe --json` e **passa** em windows-2022/2025 (`runs/42130635377`). Debug (console
  subsystem) funciona; release (GUI subsystem + `panic="abort"`, `Cargo.toml:25`) falha.
- Probes Windows são **infalíveis por construção**: captura `input-capture/src/lib.rs:348` +
  `windows.rs:46-56` retorna `Self`; emulação `input-emulation/src/lib.rs:93` + `windows.rs:71-86`
  retorna `Ok` ⇒ os `.backend` só podem **não existir** se o processo não produziu JSON. **A
  comparação `-ne "windows"` não é a causa provável**; a falha está antes (instalador/exe/abort/stdout)
  ou no uninstaller.

> **Atualização (QA final, passe cross-OS):** a premissa acima não vale mais para a
> captura. WindowsInputCapture::new / EventThread agora são falíveis e o probe do
> doctor reporta erros reais de setup, então o resultado do probe pode divergir
> legitimamente (inclusive o .backend pode faltar por erro de setup, não só por
> falta de JSON). A emulação continua Ok por construção. O restante desta seção é
> o registro histórico da investigação naquele commit.

### 3.2 Hipóteses ordenadas (hipótese, não fato) + como confirmar
1. **H1 — `doctor` release termina sem imprimir JSON (mais provável).** (a) **GUI subsystem +
   stdout**: `windows_subsystem="windows"` (`src/main.rs:1-5`) + `release.yml:101` captura via
   `& $exe ... | ConvertFrom-Json`; PowerShell trata app GUI diferente de console
   (não-wait/redirecionamento inconsistente). Confirmar com `Start-Process $exe -ArgumentList
   'doctor','--probe','--json' -Wait -PassThru -RedirectStandardOutput out.json
   -RedirectStandardError err.txt`, imprimindo `$p.ExitCode` e os arquivos; repetir com
   `cmd /c "... > out.json"`; rodar `doctor --json` **sem** `--probe`.
   (b) **Abort/race no teardown do capture thread**: `EventThread::new` publica o `thread_id` antes
   de a thread criar a message queue (`event_thread.rs:269-300`); `Drop` posta WM_USER com
   `PostThreadMessageW(...).unwrap()` (`:105,109-114`) — postar para thread sem message queue falha →
   panic; `panic="abort"` derruba o processo **antes** do `println!` do JSON (`src/doctor.rs`,
   `capture_probe`/`emulation_probe`); `GetMessageW` com erro também dá panic (`event_thread.rs:257-266`).
   É timing → explica intermitência e o debug (unwind, mais lento) passar. Confirmar: loop 50-100×
   `doctor --probe --json` do binário **release** no runner/VM capturando exit code (abort =
   `0xC0000409`/`-1073740791`) e stderr; ver "panicked at" em `PostThreadMessageW`; comparar
   `doctor` com/sem `--probe`.
2. **H2 — instalador silencioso/UAC retorna código ≠ 0.** `RequestExecutionLevel admin` (`nsi:23`)
   e `Start-Process` sem `-Verb RunAs` (`release.yml:97`); UAC do runner não verificado. Confirmar:
   logar `$p.ExitCode`/`Test-Path $exe`, `reg query HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System /v EnableLUA`,
   `whoami /groups`; rodar `/S` em VM elevado vs não.
3. **H3 — exe instalado não inicia (DLL ausente / AV).** Pacote copia só DLLs de `/ucrt64/bin` do
   `ldd` (`release.yml:77-80`); deps carregadas dinamicamente ou bloqueio de Defender/EDR podem falhar
   antes de `main`. Confirmar: `Start-Process -Wait -RedirectStandardError` (exit `0xC0000135` = DLL
   não encontrada), `Get-MpThreatDetection`, Event Viewer, listar `C:\Program Files\Lan Mouse`.
4. **H4 — JSON vazio/malformado + sem checagem de exit code.** `release.yml:101` não checa
   `$LASTEXITCODE`/`$null`; com `$json` nulo a mensagem vira "Unexpected capture probe: " (vazia).
   Confirmar: capturar stdout/stderr bruto (como H1) + checar exit code.
5. **H5 — uninstaller retorna código ≠ 0.** NSIS relança o uninstaller de `%TEMP%`;
   `Start-Process -Wait -PassThru` pode capturar código inesperado e arquivos em uso atrapalham
   (`release.yml:104-105`). Confirmar: imprimir `$u.ExitCode`/`Test-Path` após e testar em VM parado.
6. **H6 — `doctor` falha antes dos probes (cert/config/ACL).** `load_or_generate_key_and_cert`
   (`src/crypto.rs:119-125`) + `Config::new` criando `%LOCALAPPDATA%\lan-mouse` (`src/config.rs:626-631`);
   `icacls` sem verificação (`crypto.rs:110-116`). Confirmar: `doctor` humano (sem `--json`), stderr,
   e permissões de `%LOCALAPPDATA%\lan-mouse\lan-mouse.pem`.

### 3.3 Diagnóstico recomendado (sem mascarar falha)
Separar em passos "install"/"doctor"/"uninstall"; checar `$LASTEXITCODE`/`$p.ExitCode`, redirecionar
stdout/stderr p/ arquivos, imprimir ambos em falha, `throw` com o conteúdo e `upload-artifact` com
`if: always()` (hoje o upload fica `skipped`). Ler o JSON do arquivo (não do pipe), imprimir
`makensis /VERSION`/`choco list --local-only nsis`; no instalador, `IfSilent`+`FileWrite` de log
(netsh/taskkill). Só depois corrigir a causa (evitar `retry` para esconder flake).
## 4. Riscos principais
1. **Elevado vs não:** GUI/daemon elevado amplia superfície (DTLS + IPC + `SendInput` em integridade
   alta) — só opt-in explícito; não elevar quebra controle de janelas elevadas (UIPI).
2. **ACL:** falha silenciosa pode deixar a chave ilegível (peer vê "CertificateChanged") ou exposta
   a grupos herdados; sem verificação pós-condição.
3. **Firewall/AV:** Public bloqueado sem aviso, duplicação em upgrades e `netsh` silencioso →
   suporte "não conecta"; binário não assinado com hooks + input injection é alvo clássico de EDR.
4. **Sem admin / limites locais:** per-machine install inacessível (ZIP portátil sem regra inbound
   fica bloqueado no 1º contato); IPC TCP sem DACL permite que processo local de outro usuário leia
   fingerprints/estado e altere config, agravado com daemon elevado.

## 5. Recomendações priorizadas
- **P0 (CI):** implementar §3.3 e corrigir a causa raiz. Independentemente do CI: eliminar os panics
  de teardown do capture (`event_thread.rs:105,257-266`) e garantir que `doctor --json` produza
  saída/exit code mesmo em erro (e que o modo GUI subsystem funcione com stdout redirecionado).
- **P1:** assinar instalador/exe (Authenticode+timestamp) e publicar SHA-256; decidir Public no
  firewall (opt-in) + `delete`-antes-de-`add` + doctor mostrando a regra; ACL da chave por SID +
  verificação; desmarcar "Launch" da página final; documentar UIPI/secure desktop; IPC Windows por
  named pipe com DACL.
- **P2:** manifest PMv2 + DPI no `doctor`; README Windows (install/SmartScreen/sem admin); atualizar
  `DOC.md:196`; testes de ACL/firewall; `tests/windows_input.rs` menos condicional.
