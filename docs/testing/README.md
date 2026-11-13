# Guia de testes do lan-mouse-custom

Este diretório é a fonte de verdade sobre **como testar, onde colocar cada tipo de
teste e o que não pode regredir**. O objetivo é simples: toda mudança de
comportamento deve ter teste; todo bug corrigido deve virar teste de regressão
permanente.

## Camadas

| Camada | O que cobre | Onde | Regras |
|---|---|---|---|
| L0 — unitário | funções puras, parsers, mapeamentos, state machines sem I/O | `#[cfg(test)] mod tests` no próprio arquivo | determinístico; sem sleep/rede/porta |
| L1 — property/state machine | invariantes, sequências aleatórias, entradas hostis | junto do módulo (proptest) | seed reprodutível; sem I/O |
| L2 — contrato/componente | traits de capture/emulation/clipboard, `CaptureTask`, `EmulationTask`, `ListenTask` | `tests/component_*.rs` ou `src/test_support` | fakes; sem portal/rede real |
| L3 — integração determinística | pipeline capture → conn → listener → emulation em memória | `tests/integration_*.rs` | relógio pausado e transporte in-memory |
| L4 — smoke real | DTLS/UDP loopback, IPC Unix/TCP, permissões de socket, um listener real | `tests/network_smoke.rs` | **serial**, sem paralelismo |
| L5 — hardware/manual | portal GNOME/KDE, hooks do Windows, DPI, clipboard real | `docs/qa/` | matriz manual; não finge automação |
| L6 — fuzz/mutation/coverage | robustez e qualidade da suíte | `fuzz/`, CI | nightly/weekly |

Os testes existentes de `tests/qa_*.rs` são contratos de regressão históricos:
continuam valendo e não devem ser removidos.

## Ferramentas de suporte

O crate `lan-mouse-test-support` (dev-only, fora do binário) concentra:

- `TestClock` — relógio monotônico controlado;
- `TestEnv` — árvore temporária isolada (home/config/state/cache/runtime);
- `wire` — encode/decode de produção, bytes legados (`Enter` sem serial,
  `Pong` sem caps, `Hello` sem versão);
- `assertions` — `assert_close`, `assert_sequence`, `assert_one_of`,
  `assert_contains_all`.

Regra: **não duplicar** `unique_cert`, `free_udp_port`, listeners ou clientes
fakes em testes novos; primeiro estenda o crate de suporte.

## Onde colocar testes

- Função pura nova → teste no módulo, mesmo arquivo.
- Estado/máquina nova → testes de tabela + property test se houver invariante.
- Mudança em trait/backend → teste de contrato reutilizável.
- Bug corrigido → teste de regressão em `docs/testing/regressions.md` e teste
  automatizado na camada mais baixa que reproduz o bug.
- Fluxo de UI → teste de estado puro (sem GPU) quando possível.

## Determinismo (obrigatório)

- **Proibido `sleep`** para sincronizar teste; use canal, `yield_now` ou
  `tokio::time::pause`/`TestClock`.
- Portas: `bind(("127.0.0.1", 0))` e repasse a porta/socket; não use
  `free_udp_port` em testes novos.
- Certificados: fixture única por processo com contador atômico; nunca o mesmo
  path em dois testes paralelos.
- Ambiente: use `TestEnv`; variáveis de ambiente são globais — teste que mexe em
  `HOME`/`XDG_*` entra no grupo serial.
- E2E de rede real: apenas um smoke serial; o fluxo funcional principal roda em
  memória.
- Nada de retry automático para esconder flake: flake é bug e deve ser
  investigado.

## Comandos

```sh
# rápido, durante o desenvolvimento
cargo test -p <crate> --lib

# tudo (workspace)
cargo test --workspace --all-features

# lint
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings

# smoke de rede real (serial, quando existir)
cargo test --workspace --all-features network_smoke -- --test-threads=1

# repetir a suíte para expor flakes (CI semanal roda 3x)
scripts/test-repeat.sh --runs 10 --all-features

# cobertura (cargo-llvm-cov; baseline 58,03%, threshold inicial 55%, meta 75%)
scripts/coverage.sh --check 55

# fuzz de protocolo (nightly; ver fuzz/README.md)
cargo +nightly fuzz run proto_decode -- -max_total_time=60
```

CI atual: Linux + Windows, jobs de build/check/clippy/test, fmt e feature
matrix. Windows roda serial para backends que tocam estado global; Linux roda em
paralelo. O workflow `.github/workflows/test-quality.yml` (manual/semanal) roda
cobertura (`cargo-llvm-cov`), detecção de flake (3 execuções) e mutation
(`cargo-mutants`) no crate de protocolo; `cargo-nextest` está configurado em
`.config/nextest.toml` sem retries.

## Política de PR

Checklist mínimo:

- [ ] teste unitário para toda lógica pura nova;
- [ ] teste de componente/integração se mudou I/O, estado ou protocolo;
- [ ] teste de regressão se corrigiu bug (nome do teste referencia o sintoma);
- [ ] nenhum `sleep`/porta fixa/credencial compartilhada em teste novo;
- [ ] `cargo fmt --check` e `cargo clippy ... -D warnings`;
- [ ] se mexeu na wire, teste de compatibilidade com peer antigo;
- [ ] atualizou `docs/testing/regressions.md` quando aplicável.

## Handoff no Windows

Veja [windows-handoff.md](windows-handoff.md) para a matriz Windows 10/11 e a cobertura automatizada do retorno por coordenada.
O handoff Ubuntu → Windows contra receptores antigos (0.2.18) está coberto por
[handoff-old-receiver-harness.md](handoff-old-receiver-harness.md) e
[handoff-old-receiver-verification.md](handoff-old-receiver-verification.md).

## Diagnóstico de handoff

Consulte [handoff-trace.md](handoff-trace.md) para capturar a latência
`Begin → Enter → arm → Ack → primeiro input` com `LAN_MOUSE_HANDOFF_TRACE=1`.

## Próximos passos do plano de testes

1. Infra de fakes (transporte em memória, capture/emulation/listener/frontend)
   e migração dos helpers duplicados.
2. Seams `Clock`, `CaptureConnection`, `ListenerPort` e `ServiceCore` para
   testar orquestração sem portal/rede.
3. Testes de componente do Service e catálogo completo de regressões.
4. Property tests, fuzz, mutation e coverage no CI.
5. Job de detecção de flake + execução repetida da suíte.
