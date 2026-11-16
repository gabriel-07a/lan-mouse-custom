# Plano de latência e handoff — lan-mouse-custom

- **Data:** 2026-11-16
- **Cenário alvo:** Ubuntu 26.04 (GNOME/Wayland, libei) ↔ Windows 11, ambos em **Wi-Fi**.
- **Objetivo:** ping ≤ 10 ms, movimento sem "travadinhas" e handoff parecido com um monitor extra.

## Metas (SLO)

| Métrica | Meta |
|---|---|
| RTT p99 | ≤ 10 ms |
| Intervalo entre movimentos recebidos, p99 (mouse em movimento) | ≤ 10 ms |
| Handoff: Enter → primeiro movimento aplicado | ≤ 16 ms (1 quadro) |
| Perda de pacotes de movimento sem efeito visível | até 2 pacotes seguidos |

## Linha de base (trace de 2026-11-16, Wi-Fi 6E, −47 dBm)

| Medida | Valor |
|---|---|
| RTT medido pelo app | 8 ms (uma amostra a cada ~2 s, sem percentis) |
| Intervalo entre movimentos recebidos do Windows | 47% acima de 8 ms; p90 18 ms, p99 52 ms, máx 150 ms |
| Injeção no Ubuntu (libei) | 3 µs (p99 11 µs) |
| Handoff: Enter → primeiro movimento | p50 6 ms, p90 11 ms, máx 20 ms |
| Ubuntu: início da captura → 1º pacote | 0,1 ms |

O atraso nasce antes de chegar ao receptor: no envio do Windows ou no ar.

## Fase 0 — medição

- RTT contínuo com p50/p99, jitter e perda, exposto no IPC, `cli list` e interface.
- Número de sequência e horário de envio nos movimentos (extensão negociada por
  capability), para separar atraso do emissor de atraso da rede.

## Fase 1 — rede (Wi-Fi)

1. Timer de alta resolução no Windows: o lote de movimento é de 2 ms, mas o
   Windows acorda a cada ~15,6 ms por padrão.
2. Prioridade de rede real no Windows (qWAVE): `setsockopt(IP_TOS)` costuma ser
   ignorado pelo Windows.
3. Envio imediato quando o canal está ocioso: o movimento só espera o lote se
   outro pacote acabou de sair.

## Fase 2 — caminho de entrada isolado

- Thread própria para a entrada: clipboard e IPC não podem atrasar o mouse.
- Redundância contra perda: cada pacote leva também os últimos movimentos.

## Fase 3 — handoff como monitor extra

- Trocar as esperas por tempo (cooldown, permanência, guardas de retorno) por
  regras de posição/estado (`docs/design/handoff-state-machine.md`).
- Mesma velocidade nas duas telas (calibração automática).
- Continuidade de posição com resoluções e escalas diferentes.
- Cursor: some na origem e aparece no destino no primeiro pacote.
- Arrastar entre telas com botão e modificadores corretos.

## Fase 4 — outras melhorias

- P1 de `docs/audit-network.md` (sessões mortas, estado retido no listener).
- desk-sim como teste de regressão de latência no CI.
- IPC do Windows restrito ao usuário.

## Registro de execução

Cada fase registra aqui o que foi feito, o que foi medido e o que ficou pendente.

### Fase 0 — feita (2026-11-16)

- Protocolo: capability `LATENCY_PROBE` (bit 10) e eventos `LatencyProbe`/
  `LatencyReply` (ids 21/22). Peers antigos não recebem sondas.
- `src/latency.rs`: janela de amostras com percentis, sondas com perda e
  medidor de cadência (pausas da mão acima de 100 ms não contam).
- Quem conecta envia 4 sondas/s e publica RTT p50/p99 e perda 1 vez por segundo.
- Cadência de envio medida no envio do datagrama de movimento; cadência de
  chegada medida na saída da rede, antes da fila de emulação.
- `ClientState::latency` no IPC, linha `latency:` no `cli list` e `p99` no card
  da interface.
- Teste de ponta a ponta no desk-sim (`the_link_reports_latency_percentiles`).

**Como ler:** `send gap` irregular no emissor = problema do emissor (timer,
agendamento). `send gap` regular e `recv gap` irregular no outro PC = rede.

**Primeira medição real (Ubuntu com este build, Windows ainda no build antigo):**

| Medida | Valor | Leitura |
|---|---|---|
| `recv gap` p50 (Windows → Ubuntu) | 8,0 ms exatos | o mouse do Windows reporta a 125 Hz; não é atraso de rede |
| `recv gap` p99 | 46–60 ms | cauda; falta o `send gap` do Windows para dizer se é emissor ou rede |
| RTT (ping antigo) | 2 ms | abaixo da meta |
| Travadas do laço de eventos no Ubuntu | nenhuma | a thread única não está atrasando o mouse aqui |

Isso corrige a linha de base: "47% dos intervalos acima de 8 ms" era a taxa de
125 Hz do mouse, não travadinha. O que falta explicar é a cauda (p99).

### Fase 1 — feita no código, falta validar no Windows (2026-11-16)

- `src/net_tuning.rs` (só Windows; no Linux são funções vazias):
  - timer de 1 ms (`timeBeginPeriod` + opt-out do throttling de timer do
    Windows 11 para processos sem janela);
  - Wi-Fi em modo de streaming e sem varredura em segundo plano enquanto há
    link ativo (`WlanSetInterface`); o sistema desfaz ao fechar o handle ou se
    o processo morrer;
  - fluxo de voz do qWAVE no socket de saída (`QOSAddSocketToFlow`), porque o
    Windows ignora `setsockopt(IP_TOS)`.
- Item "envio imediato com canal ocioso": **já existia**. `MotionCoalescer::push`
  devolve "enviar agora" quando o último envio foi há mais de um lote; o timer
  só cobre o último movimento pendente. Por isso o timer de 15,6 ms do Windows
  atrasa só esse último movimento, não a cadência inteira. A hipótese inicial
  ("o timer explica o p90 de 18 ms") estava errada.
- Verificado: compila e passa no clippy para `x86_64-pc-windows-gnu`. **Não
  executado em Windows.**
- Dependência nova em tempo de carga: `wlanapi.dll`, `qwave.dll`, `winmm.dll`
  (presentes no Windows 10/11 de desktop; edições Server sem o serviço de WLAN
  podem não ter `wlanapi.dll`).

**Para validar:** instalar o build nos dois PCs e comparar, no mesmo movimento,
`send gap` no Windows com `recv gap` no Ubuntu (`lan-mouse cli list`).

### Fase 2 — só a medição (2026-11-16)

- `latency::watch_loop_lag`: avisa no log quando o laço de eventos acorda mais
  de 10 ms atrasado (no máximo um aviso a cada 10 s).
- **Thread separada para a entrada: não feita.** O serviço inteiro usa
  `Rc`/`RefCell` em uma thread; separar é uma refatoração grande. No Ubuntu o
  medidor não registrou nenhuma travada. Só vale fazer se o aviso aparecer no
  uso real (por exemplo ao copiar algo grande).
- **Redundância contra perda: não feita.** Só ajuda com perda real de pacotes;
  o Wi-Fi já retransmite na camada de rádio. Decidir com `loss` das sondas:
  acima de ~0,5% vale implementar.

### Picos de RTT de ~40 ms — diagnóstico (2026-11-16)

Medido com `scripts/net-probe.py` (handshake TCP até a porta 7680 do Windows,
respondido pelo kernel dele, em paralelo com ping ao roteador):

| Trecho | p50 | p90 | p99 | máx |
|---|---|---|---|---|
| Ubuntu → Windows | 3,3 ms | 7,2 ms | 24,7 ms | 46,9 ms |
| Ubuntu → roteador | 1,6 ms | 2,6 ms | 17,0 ms | 33,4 ms |

- Os picos de 15–47 ms existem **fora do Lan Mouse**: aparecem no handshake
  TCP, que não passa pelo aplicativo.
- Nos instantes dos picos o roteador responde em ~1,5 ms: o atraso está no
  trecho roteador ↔ Windows (rádio do Windows).
- Eles se repetem a cada ~4,9 s (intervalos de 4,9, 9,4 e 14,7 s nas amostras).
- Nada no Lan Mouse roda nesse período (anúncio de descoberta a cada 10 s;
  varredura de rede só manual).

**Causa mais provável:** o adaptador Wi-Fi do Windows sai do canal
periodicamente (varredura de redes em segundo plano). Não está provado.

**Correção já no código (commit `71346c5`):** modo de streaming e varredura em
segundo plano desligada enquanto há link (`net_tuning::set_link_active`).

**Como confirmar:** instalar o build no Windows e repetir
`scripts/net-probe.py 192.168.166.166`. Teste rápido sem instalar nada, em um
PowerShell de administrador no Windows:
`netsh wlan set autoconfig enabled=no interface="Wi-Fi"` (desfazer com
`enabled=yes`). Se os picos sumirem, a causa é a varredura.

**Resultado do teste com `netsh` (varredura desligada no Windows):**

| Trecho | p50 | p90 | p99 | máx | picos ≥ 15 ms |
|---|---|---|---|---|---|
| Ubuntu → Windows | 3,3 ms | 8,2 ms | 23,1 ms | 31,8 ms | 27 de 586 |
| Ubuntu → roteador | 1,6 ms | 2,9 ms | 11,3 ms | 29,2 ms | — |

A hipótese **não se confirmou**: os picos continuam na mesma proporção (antes
20 de 586). O ritmo de ~4,9 s sumiu e o máximo caiu de 47 para 32 ms, então a
varredura contribuía para os piores casos, mas não é a causa principal. O
trecho Ubuntu → roteador também tem cauda própria (p99 11–17 ms).

Próximas hipóteses, todas do lado do rádio (nenhuma é do aplicativo):
economia de energia do adaptador Wi-Fi do Windows, banda/sinal do Windows
(o Ubuntu está em 6 GHz com −47 dBm; o do Windows é desconhecido) e
coexistência com Bluetooth. Precisa da saída de `netsh wlan show interfaces`
no Windows.

### Fase 3 — revisada (2026-11-16)

- **Esperas por tempo:** as guardas de `src/capture.rs` já são por posição desde
  `ac728e5`. Elas só descartam movimento *de volta* a menos de 16 px da borda de
  entrada, por no máximo 250 ms; o movimento para frente nunca espera. Restam
  duas esperas puramente por tempo (120 ms de cooldown na captura do Windows e
  300 ms de `REMOTE_ENTRY_DWELL`), ajustadas com trace real. Não mexidas: mudar
  às cegas arrisca o vai-e-volta na borda.
- **Mesma velocidade nas duas telas:** feito na captura do Windows. Ao iniciar
  uma captura ela lê a velocidade do ponteiro do Windows (`SPI_GETMOUSESPEED`)
  e aplica o multiplicador correspondente às contagens brutas do mouse. Com a
  velocidade padrão (10) o multiplicador é 1,0 e nada muda. A curva de
  "Aprimorar precisão do ponteiro" não é reproduzida. Quem já compensava uma
  velocidade diferente com a sensibilidade por cliente vai precisar voltá-la
  para 100%. Compila para Windows; **não executado em Windows**.
- **Cursor:** Windows resolvido (`6f7d7b3`). GNOME 50: `inhibit_cursor_visibility`
  não tem efeito; falta o teste com `MUTTER_DEBUG_DISABLE_HW_CURSORS=1`.
- **Arrastar entre telas:** não avaliado.

### Fase 4 — auditoria revisada (2026-11-16)

`docs/audit-network.md` é de 2026-11-05. Estado dos itens graves hoje:

| Item | Estado |
|---|---|
| P1-02 sessões mortas no listener | corrigido (timeout ocioso fecha o transporte em `src/emulation.rs`) |
| P1-03 corrida na janela de pareamento | corrigido (`pairing_lost_race` em `src/listen.rs`) |
| P1-01 estado e buffers retidos por endpoint UDP | **pendente**: o `accept_filter` do `webrtc-util` recebe só os bytes do datagrama, sem o endereço de origem, então não dá para limitar por IP antes da alocação; exige alterar ou substituir o listener |

Os itens P2/P3 não foram reconferidos.

