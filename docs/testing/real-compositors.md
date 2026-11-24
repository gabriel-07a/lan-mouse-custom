# Testes contra compositores reais

O simulador de mesa (`desk-simulator.md`) finge o sistema operacional; estes
testes rodam os backends de verdade contra um compositor de verdade, sem tocar
na sessão do usuário (D-Bus, runtime dir, dconf e config próprios; headless,
sem dispositivos de entrada reais). Sem o script, cada teste retorna na hora,
então `cargo test` continua hermético.

| Script | Compositor | Testa | CI |
|---|---|---|---|
| `scripts/gnome-headless.sh --test` | GNOME Shell headless, modo Ubuntu (dock com auto-hide) | backend libei com um socket EIS pedido direto ao Mutter (`org.gnome.Mutter.RemoteDesktop`, sem diálogo do portal): controle remoto empurrando o ponteiro contra a borda inferior tem que abrir o dock | semanal (`test-quality.yml`, não bloqueante até ter base verde no GNOME do runner) |
| `scripts/sway-headless.sh --test` | sway headless (wlroots) | emulação wlroots (ponteiro virtual) leva o cursor até a borda; a captura layer-shell tem que começar e repassar o movimento | todo push (`rust.yml`) |
| `tests/windows_input.rs` | desktop do runner Windows | `SendInput`, hooks de captura, visibilidade do cursor | todo push |

## GNOME

- Extensão de teste: `tests/gnome-headless/lan-mouse-test-probe@lan-mouse-custom`
  grava a cada 20 ms posição do ponteiro, geometria do monitor primário e o
  estado do dock em `$LAN_MOUSE_TEST_PROBE`. Ela só é instalada na sessão
  descartável do script.
- Layouts (`GNOME_HEADLESS_SIZE` + `GNOME_HEADLESS_MONITORS`):

  ```sh
  scripts/gnome-headless.sh --test
  GNOME_HEADLESS_SIZE=2560x1600 \
    GNOME_HEADLESS_MONITORS=tests/gnome-headless/laptop-fractional.xml \
    scripts/gnome-headless.sh --test
  GNOME_HEADLESS_SIZE="2560x1600 2560x1440" \
    GNOME_HEADLESS_MONITORS=tests/gnome-headless/laptop-and-monitor.xml \
    scripts/gnome-headless.sh --test
  ```

- Sensibilidade conferida: sem o reenvio de pressão (`inject_pressure`) o teste
  falha (220 empurrões no último pixel, dock fechado); o layout de dois
  monitores falhava antes da correção de borda por monitor.
- `scripts/gnome-headless.sh <comando>` roda qualquer comando dentro da sessão
  (ex.: `gdbus call ... DisplayConfig.GetCurrentState`).

## sway

Precisa do `sway` no PATH, ou de um pacote descompactado:

```sh
apt-get download sway libwlroots-0.19 libseat1 libliftoff0 ...
for d in *.deb; do dpkg-deb -x "$d" root; done
SWAY=$PWD/root/usr/bin/sway SWAY_LD_LIBRARY_PATH=$PWD/root/usr/lib/x86_64-linux-gnu \
  scripts/sway-headless.sh --test
```

O primeiro teste achou um defeito real: a captura layer-shell criada logo após
o construtor ainda não conhecia os monitores e falhava com "no capture window
available"; o construtor agora espera a geometria dos outputs.

## Screenshots da interface

`scripts/gui-screenshots.sh <pasta>` abre a interface GPUI em modo de
demonstração (`LAN_MOUSE_GPUI_DEMO=1`: dispositivos fixos cobrindo todos os
estados, nomes longos, código de pareamento, pedido pendente, scan) dentro do
GNOME headless e tira um PNG por página e tamanho de tela, sem tocar no
desktop. Use para revisar layout (elementos cortados, sobrepostos, sem
tradução) antes e depois de mexer na interface.

```sh
(cd lan-mouse-gpui && RUSTFLAGS="-L native=$PWD/../.tools/xkblib" cargo build)
scripts/gui-screenshots.sh /tmp/ui
GUI_LANGUAGE=pt-br GUI_SIZES="1024x2400" GUI_PAGES="devices settings" \
  scripts/gui-screenshots.sh /tmp/ui-pt     # tela alta: a página inteira
```

Variáveis: `GUI_SIZES` (padrão `1920x1080 1366x768 1024x700`), `GUI_PAGES`
(`devices network settings add`; `add` abre o diálogo de adicionar),
`GUI_LANGUAGE` (`en` / `pt-br`). A extensão de teste fotografa a tela quando
`$LAN_MOUSE_TEST_PROBE.shot` aparece com o caminho do PNG.

