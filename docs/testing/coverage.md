# Cobertura — baseline e ratchet

Baseline medido em 2026-11-10 (commit `202182c`) com:

```sh
cargo llvm-cov --workspace --all-features --summary-only
```

## Resultado global

| Métrica | Cobertura |
|---|---|
| Lines | **58,03%** |
| Functions | **59,35%** |
| Regions | **59,43%** |

## Módulos críticos (lines)

| Módulo | Cobertura | Observação |
|---|---|---|
| `lan-mouse-proto` | 98,44% | protocolo e clipboard |
| `src/connect.rs` | 91,60% | dial, retry, auth, caps |
| `src/client.rs` | 90,84% | estado/identidade/capabilities |
| `src/listen.rs` | 88,81% | policy gate, sessões |
| `src/coalesce.rs` | 100,00% | movimento |
| `src/service_core.rs` | 100,00% | decisões puras extraídas |
| `input-emulation/src/absolute.rs` | 97,82% | tracker/âncora/overshoot |
| `src/capture.rs` | 69,22% | task principal; falta o run-loop |
| `src/emulation.rs` | 73,82% | task principal; falta o run-loop |
| `src/service.rs` | 6,41% | orquestração ainda sem harness |
| `lan-mouse-cli/src/lib.rs` | 18,52% | comandos CLI |
| `input-capture/src/libei.rs` | 38,71% | backend Wayland |
| `input-capture/src/layer_shell.rs` | 8,68% | backend Wayland |

## Regras de ratchet

1. O CI usa `scripts/coverage.sh --check <threshold>`.
2. Baseline inicial do workflow é **55%** (headroom de ~3 pontos sobre o medido).
3. A cada rodada verde, subir **+2 pontos** até 75% global.
4. Metas por área:
   - `service_core` + `coalesce` + `proto`: ≥ 95%;
   - `connect`/`client`/`listen`/`capture`/`emulation`: ≥ 85%;
   - `service` (quando o `ServiceHarness` existir): ≥ 70%;
   - backends de SO/UI: medidas apenas onde o CI nativo executa; código FFI
     exclusivo é documentado como exclusão.
5. Não aceitar queda sem justificativa no PR; exceções entram como exclusão
   documentada (ex.: código `#[cfg(windows)]` não executado no Linux runner).

## Exclusões previstas

- Código que só compila em outro SO (`windows`, `macos`) quando o runner não é
  nativo;
- wrappers FFI triviais sem lógica;
- UI puramente visual (layout/render) que não pode ser testada sem GPU.

## Comandos

```sh
# relatório completo (tabela)
cargo llvm-cov --workspace --all-features --summary-only

# relatório LCOV + threshold
scripts/coverage.sh --check 55
```
