# Lan Mouse Custom

> **Fork pessoal e experimental.** Este repositório é uma versão modificada do
> [Lan Mouse](https://github.com/feschber/lan-mouse), de seus contribuidores
> originais, licenciado sob GPL-3.0-or-later. Modificações © 2026 Gabriel
> Arcanjo. Veja [NOTICE](NOTICE) e [LICENSE](LICENSE). Este fork não é
> endossado pelo projeto original.
>
> A documentação completa do upstream (instalação por distro, dependências,
> Nix, firewall, backends, etc.) foi preservada em
> [README-upstream.md](README-upstream.md). Este README explica **o caso deste
> fork**, o que mudou, o que foi testado e como contribuir.

Lan Mouse é um Software KVM: um conjunto de mouse e teclado controla vários PCs
pela rede local, com o cursor atravessando a borda da tela — parecido com o
Universal Control da Apple ou com dois monitores ligados no mesmo computador.

Este fork existe para resolver um problema bem específico meu: **tornar a
passagem do cursor de um PC para o outro fluida o suficiente para parecer que
os dois são monitores da mesma máquina**. Não é um produto, não é uma release
oficial do upstream e não tem a intenção de substituir o projeto original.

---

## O problema que este fork resolve

Meu cenário é um PC com **Ubuntu 26.04 (Wayland/GNOME)** como máquina
principal e um PC com **Windows 10/11** como secundário, na mesma rede local.
O uso normal é mouse/teclado compartilhados, com o cursor cruzando a borda entre
as telas.

Os sintomas que eu via:

- o cursor **travava ou voltava sozinho** logo depois de entrar no outro PC;
- ao cruzar a borda, o cursor **aparecia no centro da borda** (ou na posição
  antiga do Windows) em vez de continuar de onde eu cruzei;
- o **primeiro movimento** depois da travessia às vezes era perdido ou aplicado
  antes do cursor ser posicionado, causando um salto;
- cliques/teclas/scroll logo no início do hand-off podiam ser engolidos;
- a conexão oscilava e, em alguns momentos, os dois PCs mostravam "conectado"
  sem realmente estarem se controlando;
- fechar a janela no Ubuntu matava a conexão/serviço em vez de continuar em
  segundo plano;
- o ping-pong de `Enter`/`Leave` na borda aparecia como travamento no cursor.

Este fork ataca esses pontos um por um, sempre com mudanças **aditivas e
negociadas por capability**, para não quebrar peers antigos.

## O que mudou neste fork

### 1. Handoff de borda (o foco principal)

- **Coordenada de cruzamento no protocolo.** Peers novos anunciam a capability
  `EDGE_COORDINATE` e enviam `EnterAt { pos, serial, along }`, em que `along` é
  a posição normalizada (0.0 = topo/esquerda, 1.0 = base/direita) do ponto onde
  o cursor cruzou. O receptor posiciona o cursor **exatamente nesse ponto**, em
  vez de no centro da borda. Peers antigos continuam recebendo `Enter` e usando
  o fallback antigo.
- **Âncora exata na borda.** Removemos o recuo fixo de 24 px que fazia o cursor
  "pular" para dentro da tela; o retorno continua protegido por uma margem
  pequena (3 px no libei, 2 px no Windows) para não devolver controle por
  trepidação.
- **Multi-monitor.** No Windows a âncora escolhe o monitor cujo segmento de
  borda contém a coordenada (layouts em L/buracos não caem no vazio); no libei
  a região do monitor é escolhida pela mesma fração.
- **Ordem de arming.** O receptor arma o backend **antes do `Ack` e de
  qualquer input**, então o primeiro delta já parte da posição de entrada. O
  serviço não re-arma depois (isso warpeava o cursor em cima do primeiro
  movimento).
- **Warm-up.** O backend de emulação é criado quando o cliente é ativado, para
  o primeiro cruzamento não pagar o custo de criação (portal/libei/Windows).
- **Leading edge.** O primeiro movimento do hand-off sai imediatamente, sem
  esperar a janela de coalescing de 2 ms herdada do movimento na tela anterior.
- **Fallbacks por backend.** X11 recebe o warp via `XWarpPointer`; X11 capture
  continua sendo stub (só recebe); macOS é dormente; wlroots usa o caminho
  relativo quando não há posicionamento absoluto.

### 2. Handoff confiável (`Enter`/`Ack`/`Leave`)

- `Enter` com serial + `Ack`, retransmissão e deduplicação.
- Input não é mais transformado em `Enter` durante o handshake: o primeiro
  clique/tecla/scroll não é perdido.
- `Leave(serial)` com serial não-zero para pedidos de retorno, e `Leave(0)`
  como confirmação; peers novos anunciam `SERIALIZED_LEAVE` para o receptor não
  cancelar um capture recém-adquirido com uma confirmação atrasada.
- Retry de `Enter`/`Leave` com orçamento limitado, sem loop infinito.
- `EdgeReturnLatch` no receptor limita os pedidos de retorno por contato com a
  borda.

### 3. Rede e lag

- Coalescing de movimento (agrupa deltas de poucos ms em um datagrama) para
  reduzir pacotes no Wi-Fi.
- Reconexão com backoff, timeouts e métricas de RTT/jitter/loss no frontend.
- Correções de pacing do clipboard e de eventos de alta frequência.

### 4. Fechar a janela mantendo o serviço

- No Ubuntu, fechar a janela (X) agora encerra apenas o frontend; o daemon e a
  conexão continuam rodando (contrato de exit code 3). O app pode ser reaberto
  pelo atalho sem derrubar o peer.
- O serviço pode rodar como serviço de usuário do systemd
  (`lan-mouse-custom.service`), separado da janela.

### 5. Compatibilidade

- Todas as mudanças de protocolo são **capability-gated**; peers antigos
  ignoram os bits novos e mantêm o comportamento anterior.
- `PROTO_VERSION` continua 1; nenhum layout existente foi quebrado.

## Status de teste (importante)

Este fork foi testado **somente** nestas combinações:

| Máquina principal (host) | Máquina secundária | Status |
|---|---|---|
| Ubuntu 26.04 (Wayland/GNOME) | Windows 10 | testado |
| Ubuntu 26.04 (Wayland/GNOME) | Windows 11 | testado |

Outras combinações **não foram validadas** por mim:

- Windows ↔ Windows;
- Linux ↔ Linux (Wayland/libei/layer-shell);
- X11 como host (capture X11 é stub neste fork — só recebe);
- macOS (código dormente);
- wlroots/Sway/Hyprland como receptor (fallback relativo, sem posicionamento
  absoluto de entrada);
- múltiplos monitores/DPI misto em todas as combinações (há testes unitários de
  mapeamento, mas falta validação real).

Se você usa outra combinação e quiser testar, ótimo — é exatamente para isso
que o fork está público. Só não trate como suporte oficial.

## Como testar

1. Baixe a release mais recente em
   [Releases](https://github.com/gabriel-07a/lan-mouse-custom/releases).
   A release de teste mais recente deste fork é
   [`v0.2.14-handoff-coord2-ee9cee0`](https://github.com/gabriel-07a/lan-mouse-custom/releases/tag/v0.2.14-handoff-coord2-ee9cee0).
   Há artefatos para Linux (`.tar.gz`, `.deb`) e Windows (`.zip` e instalador
   `lan-mouse-custom-setup-x86_64.exe`), além de `SHA256SUMS.txt`.
2. Instale/extraia os binários e abra o app nas duas máquinas.
3. Adicione o outro PC como cliente (hostname ou IP), escolha a posição da tela
   e autorize o certificado.
4. Cruze a borda nos dois sentidos, em alturas diferentes (10%, 50%, 90%).
5. Se algo estranhar, rode com log de debug:
   ```sh
   LAN_MOUSE_LOG_LEVEL=debug lan-mouse
   ```
   e procure as linhas `handoff:` — elas mostram a coordenada de entrada, o
   tempo até o primeiro movimento e o caminho de retorno.

### Build a partir do código

```sh
cargo build --release
cargo build --release --manifest-path lan-mouse-gpui/Cargo.toml
```

Os dois binários (`lan-mouse` e `lan-mouse-gpui`) precisam ficar lado a lado.
Dependências por distro, Nix, firewall e instalação manual estão em
[README-upstream.md](README-upstream.md).

## Como contribuir

Contribuições são bem-vindas, principalmente:

- **testes em outras combinações de SO** (Windows ↔ Windows, Linux ↔ Linux,
  X11, wlroots);
- **relatos de handoff** com versão do SO, versão/commit do fork, tipo de
  conexão (cabo/Wi-Fi) e o trecho do log com as linhas `handoff:`;
- correções de posicionamento em multi-monitor/DPI misto;
- resiliência a perda de pacotes no movimento (redundância/sequência);
- documentação e melhorias de acessibilidade.

Antes de abrir PR:

```sh
cargo fmt --check
cargo test --workspace
cargo clippy --workspace --all-targets --all-features
```

Regras do fork:

- mudanças de protocolo devem ser aditivas ou gated por capability;
- não quebrar peers antigos (upstream e builds anteriores do fork);
- manter o escopo: o foco é handoff de borda fluido e confiável;
- um PR por assunto, com teste proporcional e descrição do cenário testado.

Abra uma issue em
[github.com/gabriel-07a/lan-mouse-custom/issues](https://github.com/gabriel-07a/lan-mouse-custom/issues)
com o template informal: **o que aconteceu, em quais SOs, o que você esperava e
os logs**. Se for uma combinação que eu não tenho, eu provavelmente vou pedir
ajuda para validar.

## Limitações conhecidas

- **Windows**: o cursor pode ficar invisível quando não há mouse real conectado
  (limitação do backend); UIPI/secure desktop não é contornado.
- **X11**: capture não implementado neste fork; a máquina X11 só recebe input.
- **macOS**: código dormente, não testado.
- **wlroots**: entrada por coordenada usa fallback relativo quando o compositor
  não oferece posicionamento absoluto.
- **Perda de pacotes**: movimento ainda é delta puro; perda de um datagrama pode
  causar um pequeno engasgo. Redundância/sequência ficou como trabalho futuro.
- **Portal/GNOME**: a primeira captura pode pedir permissão; o token é
  persistido quando o portal permite.
- Este fork é **experimental**; releases de teste podem ser regeradas a
  qualquer momento.

## Clean install, reset and uninstall

O fork mantém o comportamento do upstream: atualizar um pacote **preserva** os
dados do usuário (devices, permissões, certificados, audit log e tokens de
consentimento); uma instalação limpa não deve ressuscitar dados antigos. Os
caminhos exatos por SO estão em
[docs/audit-fresh-install.md](docs/audit-fresh-install.md).

- **Windows installer**: detecta restos em `%LOCALAPPDATA%\lan-mouse` e
  `%APPDATA%\lan-mouse` e renomeia para `lan-mouse.bak-<time>` em vez de apagar.
  Opções: `/KEEPUSERDATA` (não mexe), `/CLEANUSERDATA` (limpa com backup) e
  `/PURGEUSERDATA` (remove de vez). O instalador é o
  `lan-mouse-custom-setup-x86_64.exe` das releases.
- **Linux**: `scripts/install-clean.sh --dry-run` / `--yes` / `--purge` instala
  os binários e limpa os dados do app com backup (quando aplicável).
- **CLI**: `lan-mouse --reset-config` move os diretórios do app para um backup
  `lan-mouse.bak-*`; use `lan-mouse --reset-config --purge` para apagar de vez
  (com cuidado) e `--dry-run` para só listar o que seria movido/removido.

Para o passo a passo completo (incluindo o que cada opção apaga), veja a seção
correspondente em [README-upstream.md](README-upstream.md#clean-install-reset-and-uninstall).

## Licença e créditos

- Projeto original: [Lan Mouse](https://github.com/feschber/lan-mouse), de seus
  contribuidores, sob GPL-3.0-or-later.
- Este fork: modificações © 2026 Gabriel Arcanjo, também sob
  GPL-3.0-or-later. Veja [NOTICE](NOTICE) e [LICENSE](LICENSE).
- O upstream não endossa este fork; bugs específicos das minhas mudanças devem
  ser reportados **aqui**, não no repositório original.

---

## English summary

This is a personal, experimental fork of
[Lan Mouse](https://github.com/feschber/lan-mouse) focused on making the
edge-crossing hand-off between two PCs feel like moving between two monitors.
It is only tested on **Ubuntu 26.04 (Wayland/GNOME) + Windows 10/11**; other OS
combinations are untested. The upstream documentation is preserved in
[README-upstream.md](README-upstream.md). Testers and contributors are welcome:
please open an issue with your OS versions, connection type and the `handoff:`
debug logs. Licensed GPL-3.0-or-later; see [NOTICE](NOTICE).
