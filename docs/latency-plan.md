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

### Fases 3 e 4 — não iniciadas

Dependem de teste nos dois PCs reais a cada mudança (guardas de handoff,
calibração de velocidade) ou de revisar item a item a auditoria de rede, que é
de 2026-11-05 e tem pontos já corrigidos desde então.

