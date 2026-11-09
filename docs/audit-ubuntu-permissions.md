# Auditoria: permissões de portais no Ubuntu/Linux (por que o prompt repete)

Escopo: descobrir por que GNOME/KDE pedem permissão a cada execução do lan-mouse
e implementar persistência segura de consentimentos. Host de referência:
**Ubuntu 26.04.1 LTS, GNOME 50.1, sessão Wayland**, `xdg-desktop-portal`
1.21.1+ds-1ubuntu3.1, `xdg-desktop-portal-gnome` 50.0, `ashpd` 0.13.9.

## 1. Diálogos de permissão que o lan-mouse dispara

| Diálogo | Portal / API | Disparado por | Persistência suportada? |
|---|---|---|---|
| "Capture Input" — *"The application … wants to capture input events"* | `org.freedesktop.portal.InputCapture` (`CreateSession` → `CreateSession2`+`Start`) | `input-capture/src/libei.rs::create_session` | **Só na v2** (`Start.restore_token`/`persist_mode`); GNOME 50 é v1 → **não** |
| "Remote Desktop" — *"Allow Remote Interaction"* + *"Remember This Selection"* | `org.freedesktop.portal.RemoteDesktop` (`SelectDevices`+`Start`) | `input-emulation/src/libei.rs::get_ei_fd` | **Sim** (restore token da v2) |
| "Allow Clipboard Access" (mesmo diálogo do RemoteDesktop) | `org.freedesktop.portal.Clipboard` sobre a sessão RemoteDesktop | `input-clipboard/src/portal.rs::PortalClipboard::new` | **Sim**, se o token do RemoteDesktop for reaproveitado |
| ScreenCast/ScreenRecording | `org.freedesktop.portal.ScreenCast` | **não usado** — nenhuma referência no código | N/A |
| polkit, keyring/Secret Service | — | **não usado** pelo lan-mouse | N/A |

Só o daemon cria sessões de portal: a GUI não depende de `ashpd`, então não há
prompt duplicado vindo da interface.

## 2. Evidência do portal instalado (consultado via D-Bus em 2026-11-05)

```sh
busctl --user get-property org.freedesktop.portal.Desktop \
  /org/freedesktop/portal/desktop org.freedesktop.portal.InputCapture version
# u 1                                  <- InputCapture v1: NÃO tem restore token
busctl --user get-property org.freedesktop.portal.Desktop \
  /org/freedesktop/portal/desktop org.freedesktop.portal.RemoteDesktop version
# u 2                                  <- RemoteDesktop v2: tem persistência
busctl --user introspect org.freedesktop.impl.portal.desktop.gnome \
  /org/freedesktop/portal/desktop org.freedesktop.impl.portal.InputCapture
# só .CreateSession; .version = 0      <- backend GNOME não implementa v2
busctl --user call org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop \
  org.freedesktop.portal.InputCapture CreateSession2 'a{sv}' 1 handle_token s probe
# Call failed: Method CreateSession2 is not implemented on interface
#              org.freedesktop.portal.InputCapture
```

O front-end 1.21.1 tem código v2, mas o habilita com
`impl_version = MIN(backend_version, 2)` e retorna `UNHANDLED` quando o backend
é v1 (<https://github.com/flatpak/xdg-desktop-portal/blob/1.21.1/src/input-capture.c>).
Ou seja: no Ubuntu 26.04 + GNOME o consentimento de **captura** não é
persistível por API — o diálogo não tem nem checkbox de "lembrar"
(`gresource extract .../inputcapturedialog.ui`: só o switch "Allow Remote Input
Capturing"). Já o `remotedesktopdialog.ui` tem `persist_check`
("_Remember This Selection") e o switch "Allow Clipboard Access".

## 3. Por que o prompt reaparecia (causa → evidência → correção)

| # | Causa | Evidência (antes) | Correção |
|---|---|---|---|
| 1 | Consentimento de captura nunca era salvo: o código usava `CreateSession` v1 (inicia implícito) e **nunca chamava `Start`**, único método com `restore_token`/`persist_mode`. | `input-capture/src/libei.rs:181-191` (HEAD), sem `input_capture.start` | fluxo v2 `CreateSession2`+`Start` com `PersistMode::ExplicitlyRevoked` + token; fallback v1 via `RequiresVersion` (`input-capture/src/libei.rs:296-360`) |
| 2 | Tokens em `$XDG_CACHE_HOME/lan-mouse` (limpo por ferramentas de limpeza/reboot). | `input-emulation/src/libei.rs:54-65`; `input-clipboard/src/portal.rs:410-415` | `$XDG_STATE_HOME/lan-mouse` (fallback `~/.local/state/lan-mouse`), leitura+migração do cache |
| 3 | `fs::write` não atômico: crash no meio grava token truncado, que é enviado de volta ao portal e rejeitado. | `input-emulation/src/libei.rs:83`; `input-clipboard/src/portal.rs:431` | tmp file + `rename` atômico |
| 4 | Token ilegível por outros usuários: arquivos reais estavam `0644/664` (`ls -l ~/.cache/lan-mouse/*.token`). Token é capability (restaura sessão sem prompt). | inspeção local do host | `0600` no arquivo e `0700` no diretório; cópia legada removida após migrar |
| 5 | Token vazio/corrompido lido como válido (emulação não filtrava string vazia) → `Start` falha em vez de pedir consentimento. | `input-emulation/src/libei.rs:68-74` | vazio = ausente (`read_token_file`) |
| 6 | Sessão de captura criada no startup do daemon, mesmo sem nenhum client ativo → prompt de captura em todo lançamento para quem só recebe input. | `input-capture/src/libei.rs:237` (HEAD) | sessão criada **lazy** só quando há client a capturar (`input-capture/src/libei.rs:397,478-483`) |
| 7 | Token é *single-use* e rotativo; se o novo token não for salvo após cada `Start`, o próximo restaura falha e o portal pergunta de novo. | `input-emulation/src/libei.rs:108-113`; `input-clipboard/src/portal.rs:116-122` | salvo a cada `Start` (captura incluída, `input-capture/src/libei.rs:320-328`) |
| 8 | Duas sessões RemoteDesktop separadas (emulação e clipboard), cada uma com seu token → até dois prompts, uma vez cada. | `input-emulation/src/libei.rs:155`; `input-clipboard/src/portal.rs:84` | mantido (mudança exigiria refatorar o core); ambos persistem agora |
| 9 | `app-id` do portal para apps nativos vem do scope systemd + `.desktop`; rodar do terminal (app-id vazio) e pelo launcher (app-id `de.feschber.LanMouse`) criam consentimentos diferentes no PermissionStore. | <https://github.com/flatpak/xdg-desktop-portal/blob/1.21.1/src/xdp-app-info-host.c> | não corrigível no app; documentado (valide sempre pelo mesmo caminho) |
| 10 | GNOME recria a sessão de captura a cada mudança de device (`RESTART_SESSION_ON_DEVICE_CHANGE`); na v1 cada recriação é um novo prompt. | `input-capture/src/libei.rs:60-70` | inerente ao backend v1; workaround na seção 6 |
| 11 | Backend `rdp` alternativo (`input-emulation/src/xdg_desktop_portal.rs:36-50`) seta `persist_mode` mas descarta `restore_token` → prompt a cada start. | grep `restore_token` no arquivo: ausente | **fora do escopo** desta task; follow-up |

## 4. O que foi implementado

- **`input-capture/src/libei.rs`**: helpers de token (state dir, migração,
  escrita atômica 0600/0700 — linhas 52-151); `create_session` com v2
  (`CreateSession2`+`Start`, `PersistMode::ExplicitlyRevoked`, restore token) e
  fallback para v1 (linhas 296-360); sessão criada apenas quando há captura a
  fazer (linhas 397 e 478-483).
- **`input-emulation/src/libei.rs`**: token do RemoteDesktop movido para o state
  dir, atômico/0600, migração do cache e remoção da cópia legada (linhas
  55-151); log de diagnóstico "restore token present/missing" (linhas 158-166).
- **`input-clipboard/src/portal.rs`**: mesmo tratamento para `clipboard.token`
  (linhas 431-527); `restore_token_present()` virou somente-leitura para o
  `doctor` não migrar nada (linhas 54-72).
- **`src/doctor.rs`**: relatório `portal_tokens` com finalidade, caminho,
  presença e cópia legada, sem revelar conteúdo (linhas 53-148, 189-238, 425-455);
  avisos acionáveis e `--json`.

Exemplo de saída (`lan-mouse doctor`) em um host com tokens antigos no cache:

```
  portal tokens:   /home/user/.local/state/lan-mouse
    input capture (InputCapture portal): missing (.../input-capture.token)
    input emulation (RemoteDesktop portal): present in legacy cache (~/.cache/lan-mouse/remote-desktop.token)
    clipboard (RemoteDesktop/Clipboard portal): present in legacy cache (~/.cache/lan-mouse/clipboard.token)
```

## 5. Segurança

- Token de restore = capability: restaura a sessão do portal **sem** diálogo.
  Por isso 0600/0700 (arquivos legados estavam legíveis por outros usuários e
  são removidos após a migração). Conteúdo nunca é logado; o `doctor` mostra só
  caminho/existência.
- Escrita atômica evita token truncado (que seria recusado pelo portal e
  causaria novo prompt) e não deixa arquivo temporário para trás.
- Token rotativo: salvar sempre o token devolvido por `Start`; a revogação real
  continua sendo feita no portal (negar/desmarcar "Remember"), após o que o
  token em disco simplesmente deixa de restaurar.

## 6. Validação manual (GNOME)

1. `lan-mouse doctor` → confira a seção `portal tokens` (antes do primeiro start
   após o upgrade os tokens aparecem como "legacy cache").
2. Inicie o lan-mouse, aceite "Remote Desktop" **marcando "Remember This
   Selection"** e "Allow Clipboard Access". Um client ativo é necessário para o
   diálogo de captura; sem client ativo ele não aparece mais.
3. `ls -l ~/.local/state/lan-mouse/` → `*.token` com `-rw-------` e diretório
   `drwx------`; cópias em `~/.cache/lan-mouse/` somem após a migração.
4. `lan-mouse doctor` → emulação/clipboard "present"; o aviso de captura
   permanece no GNOME (esperado, ver seção 2).
5. Reinicie o app → **não** deve haver diálogo de RemoteDesktop/clipboard; o de
   "Capture Input" aparece de novo no GNOME v1 (e a cada hotplug de device).

KDE/Plasma (não instalado neste host): rode
`busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.InputCapture version`.
Se retornar `>= 2`, o fluxo implementado ativa o restore token de captura e o
diálogo deve ser único; valide movendo o mouse até a borda duas vezes e
reiniciando o app. Se retornar `1`, vale a limitação do GNOME.

## 7. Limitações / hipóteses não verificadas (follow-ups)

> **Atualização (pós-v0.2.7, R-02):** a sessão de captura agora é única por execução e reutilizada
> entre hand-offs; com InputCapture v1 (GNOME) o compositor ainda pede consentimento uma vez por
> **execução** (v1 não tem restore token). O override LM_RESTART_SESSION_ON_DEVICE_CHANGE deixou de
> existir: mudanças de device não recriam mais a sessão.


- **Captura no GNOME (v1) não tem correção via portal.** Workarounds: aceitar um
  prompt por sessão de captura; extensão GNOME Shell que fale libei/EIS sem
  portal; aguardar `xdg-desktop-portal-gnome` implementar InputCapture v2
  (o front-end 1.21.1 já suporta, o backend reporta versão 0).
- **KDE não testado** neste host; o caminho v2 é usado automaticamente se o
  portal reportar versão >= 2.
- **Mudança de comportamento (hipótese de risco):** como a sessão de captura
  passou a ser criada lazy, um usuário que negue o diálogo não cai mais
  automaticamente para outro backend de captura; a GUI mostra captura
  desabilitada com "Reenable". Precisa de teste manual em GNOME/KDE.
- **Backend `rdp`** de emulação continua sem persistir token (fora do escopo).
- `trusted_ips`/pairing e revogação de sessão viva são do core, não desta task.
