# Mutation testing — baseline

Ferramenta: **cargo-mutants 27.1.0**. Escopo inicial: `lan-mouse-proto/src/lib.rs`
(protocolo; os testes são rápidos e cobrem encode/decode/compatibilidade).

```sh
cargo mutants -p lan-mouse-proto --file lan-mouse-proto/src/lib.rs \
  --timeout 60 --in-place
```

## Resultado

| Rodada | Mutantes | Caught | Missed | Unviable |
|---|---|---|---|---|
| Baseline inicial | 33 | 18 | 11 | 4 |
| Após reforçar testes | 33 | **24** | **5** | 4 |
| Após excluir equivalentes | 21 | **17** | **0** | 4 |

Mutation score: **17/17 viáveis = 100%** (0 missed).

Os 5 mutantes do primeiro ajuste são **equivalentes** (não matáveis):

- `BATCHED_INPUT: 1 << 0` → `1 >> 0`: ambos valem 1;
- `CAPABILITIES = A | B | ...` → `A ^ B ^ ...`: os bits de capability são
  distintos, então OR e XOR produzem o mesmo conjunto.

`.cargo/mutants.toml` exclui esses dois padrões por regex, com a justificativa
acima.

## Regras

1. Mutante novo que sobrevive: adicionar teste (preferencialmente no nível mais
   baixo que reproduz) ou documentar como equivalente aqui.
2. Expandir o escopo para `src/client.rs`, `src/connect.rs`, `src/coalesce.rs` e
   `input-emulation/src/absolute.rs` à medida que a suíte estabiliza.
3. Mutation roda manualmente/semanalmente (`.github/workflows/test-quality.yml`,
   job `mutation`), não bloqueia PR.
