# Plan de Refactor — `sync-rollup-composer`

> Destino final: `docs/refactor/PLAN.md` (vive en el repo, en branch `refactor/phase-0-mapping`).
> Sesión Codex de referencia: gpt-5.4 / xhigh / read-only. Revisado en dos pasadas (propuesta + adversarial).

---

## 1. Contexto

Este refactor existe porque la **complejidad estructural** del crate `based-rollup` ha superado lo que un equipo chico puede mantener con confianza, aunque en métricas superficiales el código está limpio.

**Métricas medidas hoy** (no especulación):

- `cargo build --release`: limpio. `cargo clippy --workspace --all-features`: 0 warnings. 534 tests.
- 48 `unwrap()` en producción (la mayoría en `composer_rpc/trace.rs`, parseo JSON). 1 sólo `TODO` en todo el código de producción.
- Total crate: ~40k LOC (~16k producción + ~24k tests).
- Funciones-mamut: `step_builder` ~760 LOC, `flush_to_l1` ~640 LOC, `verify_local_block_matches_l1` ~253 LOC, `build_builder_protocol_txs` ~280 LOC, `trace_and_detect_l2_internal_calls` ~1693 LOC, `trace_and_detect_internal_calls` ~1817 LOC, `simulate_l1_combined_delivery` ~547 LOC, `simulate_l1_delivery` ~535 LOC.
- `composer_rpc/l1_to_l2.rs` (4345 LOC) y `composer_rpc/l2_to_l1.rs` (5459 LOC) son **mirrors casi idénticos** por diseño no refactorizado.
- `CLAUDE.md` "Lessons Learned" tiene **~50 reglas críticas** que viven en convenciones humanas, no en tipos.

**Diagnóstico**: no es spaghetti — es **deuda concentrada**. Las invariantes de consenso están codificadas como "orden correcto de hacer cosas" en secuencias imperativas largas. Cualquier nuevo dev (o agente) que toque `step_builder`, `flush_to_l1` o las funciones de detección las romperá silenciosamente.

**Resultado deseado**:

1. Las invariantes críticas de `CLAUDE.md` se vuelven imposibles de romper porque están en el sistema de tipos o un test/gate de CI dedicado (§6 tiene 23 invariantes asignadas).
2. Las funciones >500 LOC en `driver/` y `composer_rpc/` se rompen en pipelines / state machines explícitas con sub-funciones <200 LOC.
3. La duplicación L1↔L2 en `composer_rpc/` se elimina detrás de un sealed trait `Direction` — pero **sólo después** de que el comportamiento ya esté encapsulado en stages testables.
4. El plan en sí ayuda a quien lo lea a **entender el proyecto entero**, no sólo a refactorizarlo.

## 2. Non-goals

- ❌ NO se cambia la spec (`docs/DERIVATION.md`).
- ❌ NO se modifica el submódulo Solidity (`contracts/sync-rollups-protocol/`).
- ❌ NO se cambia el comportamiento observable: derivación, state roots, action hashes, postBatch encoding, y orden de tx en cada bloque deben quedar **byte-idénticos** (verificado vía baseline replay en 0.8 / 5.7).
- ❌ NO se agregan features nuevos: sin endpoints RPC nuevos, sin opcodes nuevos, sin métricas nuevas.
- ❌ NO se refactoriza la UI ni los scripts E2E (`scripts/e2e/`).
- ❌ NO se elimina el ECDSA prover de desarrollo — ese es un trabajo aparte (prerequisite para producción pero **no parte de este refactor**).
- ❌ NO se parten `cross_chain.rs` / `table_builder.rs` por tamaño (hoy 2410 / 2524 LOC). Si querés atacarlos, eso es una **Fase 6 explícita** que hoy no está en el plan.

## 3. Meta-reglas (aplican a TODO commit)

| Regla | Comando / Política |
|---|---|
| Verificación por commit | `cargo build --release` debe pasar |
| Verificación al cierre de cada paso | `cargo nextest run -p based-rollup` debe pasar |
| Verificación al cierre de cada fase | `cargo nextest run --workspace && cargo clippy --workspace --all-features -- -D warnings` |
| Smoke test E2E al cierre de fase | Levantar devnet-eez (NUNCA testnet-eez) y correr el subset E2E definido al final de la fase |
| **Baseline replay al cierre de cada fase** | `bash scripts/refactor/replay_baseline.sh` (creado en 0.8) debe producir 0 diffs |
| Branch policy | `refactor/<fase>-<topic>` (ej. `refactor/phase-1-newtypes`). `refactor/` es prefijo aceptado para este workstream (alineado con `refactor:` conventional commit). Pasos "incremental" pueden compartir branch; pasos "dedicada" llevan branch y PR propios. |
| Commits | Conventional commits, atómicos: `refactor(driver): introduce EntryVerificationHold typestate (#0)` |
| **Merge policy** | **Merge commit (NO squash)** para preservar revertibilidad por step. Squash rompe la regla "cada paso revertible con `git revert`". |
| No-touch zones | `contracts/sync-rollups-protocol/`, `docs/DERIVATION.md` (sólo `spec-writer` con auditoría), `crates/based-rollup/src/evm_config.rs` (passthrough delicado), `deployments/shared/genesis.json` |
| Halt conditions | Si un paso requiere modificar el comportamiento observable o sortear una invariante de `CLAUDE.md`, **PARAR** y abrir un issue describiendo la fricción antes de seguir |
| Reversibilidad | Cada paso debe poder revertirse con `git revert <merge-commit>` sin tocar pasos posteriores |
| Devnet reset | Si un cambio en entries/postBatch deja L1 con estado incompatible (ver CLAUDE.md "Stale L1 state blocks builder recovery"), reset con `down -v` **sólo con aprobación explícita del usuario** por paso |

## 4. Glosario (idiomas Rust que el plan usa)

- **Newtype pattern**: `pub struct ActionHash(pub B256);` — wrapper zero-cost sobre `B256`, tipo distinto para el compilador. Hace imposible pasar un `RollupId` donde se espera un `BlockNumber`.
- **Typestate pattern**: `FlushPlan<Collected>` → `FlushPlan<HoldArmed>` — el mismo struct con un parámetro fantasma de tipo. Permite que `send_to_l1(plan: FlushPlan<HoldArmed>)` rechace en compilación cualquier llamada que no haya armado el hold antes.
- **Sealed trait**: trait que sólo puede implementarse dentro del crate (vía un super-trait privado). Lo usaremos para `Direction` así nadie fuera puede agregar direcciones nuevas.
- **Dependency inversion trait**: trait que abstrae un borde de IO (red, filesystem, clock). Sólo vale la pena cuando tenés ≥2 impls reales (no "producción + mock inventado"). **En este refactor sólo usamos `SimulationClient`** (ver §4b para los que evaluamos y descartamos).
- **Module-private constructor + boundary wrapper**: `CleanStateRoot(B256)` cuyo `new` es `pub(crate)` y sólo se llama desde `compute_clean_root(...)`. En los bordes (ABI decode, logs, serde) se usan funciones `from_bytes_at_boundary` nombradas explícitamente para que grep encuentre todo constructor sospechoso.
- **Phantom data**: `PhantomData<S>` — dato zero-cost que sólo existe para el sistema de tipos, no en runtime.
- **`#[must_use]`**: anotación que hace warning si el tipo devuelto no se consume. Lo usaremos para `NonceResetRequired` para que ignorarlo sea error de compilación (con `-D warnings`).
- **Builder pattern**: `XBuilder::new().with_a(a).with_b(b).build()` — alternativa a constructores con muchos `Option`/`bool`.
- **`debug_assert!` vs `assert!`**: `debug_assert!` se elimina en release builds. Lo usaremos para invariantes blandas que el sistema de tipos no captura.

## 4b. Estrategia de traits (dónde SÍ y dónde NO)

> **Regla de diseño** (Codex, pasada 3): *"traits en bordes de IO; enums y structs concretos en el dominio; multiple `impl` blocks para partir responsabilidad; no traits con una sola impl interna"*.

Este refactor usa traits en **exactamente tres** lugares. Todo lo demás se modela con structs concretos, enums cerrados, o module split + multiple `impl Driver` blocks.

| Trait | Propósito | Criterio | Pasos |
|---|---|---|---|
| `Direction` (sealed) | Unificar L1↔L2 en composer | ≥2 implementaciones reales simétricas (`L1ToL2`, `L2ToL1`) | 3.1, 3.4-3.7 |
| `Sendable` (sealed marker) | Compile-time gate de `FlushPlan<S>` | Typestate — reemplaza chequeos runtime | 1.7 |
| `SimulationClient` | Borde JSON-RPC del composer | ≥2 impls reales: `HttpSimClient` + `InMemorySimClient` con fixtures (habilita tests de composer_rpc sin upstream) | 3.0 |

**EXPLÍCITAMENTE rechazados** (considerados y descartados tras revisión Codex):
- ❌ `EntryQueue` como trait — la cola composer→driver es estado interno compartido, no un backend intercambiable. Se modela como **struct concreto `EntryQueue`** con métodos `push` / `wait_confirmation` / `drain_confirmed`.
- ❌ `L1Provider` como trait ancho — alloy ya tiene su `Provider` trait. Doble abstracción. Se modela como **struct wrapper `L1Client`** local a `proposer.rs` con métodos narrow. Si más adelante necesitamos mockearlo, extraemos un trait entonces (no preemptivamente).
- ❌ Capability traits (`BuilderPhase`, `FlushCoordinator`, `BlockVerifier`, `BlockRewinder`) — con una sola impl no fuerzan nada que no forcen los multiple `impl Driver` blocks. El doc comment no es enforcement. La "disciplina de alcance" real viene de **structs concretos con borrows narrow** (`BuilderTickContext`, `FlushPrecheck`, `FlushAssembly`, `VerificationDecision`) que los pasos 2.2-2.7 ya introducen.
- ❌ `SimulationStrategy` como trait + `OrElse` chain — con un set de 3 estrategias fijas cerrado en el crate, un **enum `SimulationPlan` + función** es más claro.

**Regla operativa**: si te sentís tentado a agregar un trait nuevo durante la ejecución del plan, respondé primero: (1) ¿Hay ≥2 impls reales, no sólo una "por si acaso"? (2) ¿La segunda impl es un backend real, no un mock ficticio? Si cualquiera es "no", usá un struct concreto.

**Detalle técnico — async en traits**: Rust 1.85 soporta `async fn` nativo en traits. El único trait de este refactor que usa `Arc<dyn Trait>` es `SimulationClient` — para él agregamos el crate `async-trait = "0.1"` como dep (alocación por call, trade-off aceptado por el beneficio de fixtures). `Direction` usa generics (static dispatch), así que `async fn` nativo es suficiente.

## 5. Mapa de arquitectura actual (estado pre-refactor)

```
                            ┌──────────────────────────────────────────┐
                            │            L1 (reth --dev)               │
                            │  ┌────────────────────────────────────┐  │
                            │  │  Rollups.sol / CCM / Bridge / etc │  │
                            │  └─────────▲──────────┬───────────────┘  │
                            │            │postBatch │BatchPosted        │
                            └────────────┼──────────┼──────────────────┘
                                         │          │
                          ┌──────────────┴──┐    ┌──┴──────────────────┐
                          │   proposer.rs   │    │   derivation.rs     │
                          │  (sender L1)    │    │  (L1 sync, §4e/§4f)│
                          └──────────────▲──┘    └──────────┬──────────┘
                                         │                  │
                                         │                  ▼
   ┌────────────────────┐   ┌────────────┴──────────────────────────┐
   │ payload_builder.rs │   │              driver.rs                │
   │   block building   │◄──┤ step_builder │ flush_to_l1 │ verify  │
   └────────────────────┘   └────────────┬──────────────────────────┘
                                         │
            ┌────────────────────────────┴────────────────────────────┐
            │                                                          │
            ▼                                                          ▼
   ┌────────────────────────┐                          ┌──────────────────────────┐
   │   composer_rpc/        │                          │   cross_chain.rs +      │
   │  ┌──────────────────┐  │                          │   table_builder.rs     │
   │  │ l1_to_l2.rs (4k) │  │                          │  - ABI types           │
   │  │ l2_to_l1.rs (5k) │  │ ◄──────── builds ─────── │  - entry building      │
   │  │  trace.rs        │  │                          │  - state delta logic   │
   │  │  common.rs       │  │                          └──────────────────────────┘
   │  └──────────────────┘  │                                       ▲
   │  HTTP RPC interceptors │                                       │
   │  (hold-then-forward)   │                                       │
   └─────────▲──────────────┘                                       │
             │                                          ┌───────────┴────────────┐
             │                                          │       rpc.rs            │
             │            cliente JSON-RPC              │  (jsonrpsee trait)      │
             └──────────────────────────────────────────┤  syncrollups_*          │
                                                         └─────────────────────────┘
```

## 6. Tabla de invariantes críticas → tipo destino (23 reglas)

Esta tabla es el contrato del refactor. Cada regla crítica de `CLAUDE.md > Lessons Learned` que matamos por tipos se documenta aquí. **Paso 0.2 produce esta tabla expandida en el repo como `docs/refactor/INVARIANT_MAP.md`.**

| # | Invariante (CLAUDE.md) | Owner actual (humano) | Owner futuro (tipo) | Fase |
|---|---|---|---|---|
| 1 | Hold MUST be set BEFORE send_to_l1 | Comentario en `flush_to_l1` | `FlushPlan<HoldArmed>` typestate con `entry_block` dentro del plan | 1.7 |
| 2 | NEVER use auto-nonce, always reset on failure | Comentario en `proposer.rs` | `L1NonceReservation` + `#[must_use] NonceResetRequired` | 1.8 |
| 3 | NEVER align state roots by overwriting pre_state_root | Comentario `flush_to_l1` | `CleanStateRoot` newtype con módulo-privado + `from_bytes_at_boundary` explícito | 1.2 |
| 4 | §4f filtering is per-call prefix counting, never all-or-nothing | Comentario `derivation.rs` | `ConsumedPrefix(usize)` + test de monotonicidad (property) | 0.3 |
| 5 | Continuation entries are NOT triggers (`hash(next_action) != action_hash`) | Comentario en `partition_entries` | `enum EntryClass { Trigger, Continuation, Result, RevertContinue }` | 1.4 |
| 6 | Result entry skipped when `extra_l2_entries` non-empty | Doble check en driver y rpc | `enum QueuedCallRequest::{Simple, WithContinuations}` — la variante Simple carga Result, la WithContinuations no lo admite | 1.4b |
| 7 | `parent_call_index` debe rebasarse después de combined_delivery | Comentario en `l2_to_l1.rs` | `ParentLink { Root, Child(AbsoluteCallIndex) }` + helper único `rebase_parent_links` | 1.3, 3.3 |
| 8 | First TRIGGER entry needs currentState=clean (post swap-and-pop reorder) | Lógica imperativa | `first_trigger_idx` calculado por `ImmediateEntryBuilder` con test dedicado | 1.9b |
| 9 | Deferral exhaustion → rewind, not accept | Lógica imperativa en `verify_local_block_matches_l1` | `VerificationDecision::MismatchRewind { target: entry_block - 1 }` | 2.5 |
| 10 | Rewind target is `entry_block - 1` | Comentario | Método único `Driver::rewind_to_re_derive(entry_block: u64)` que calcula target dentro | 2.5 |
| 11 | Deposits + withdrawals can coexist in same block | Removed mutual exclusion | `enum BlockEntryMix { Empty, OnlyD, OnlyW, Mixed }` exportado por `PendingL1SubmissionQueue` | 1.5 |
| 12 | Multi-call L2→L1 must use scope navigation on Entry 1 | Comentario en `table_builder.rs` | `L2ToL1ContinuationBuilder::with_scope_return(scope)` requerido | 1.9c |
| 13 | **Hold-then-forward: both composer RPCs MUST await queue confirmation** | Comentario en `composer_rpc/*` | `ForwardPermit` token devuelto sólo cuando `EntryQueue` transiciona el receipt a `Confirmed` (= bloque L2 con las entries fue construido localmente) | 1.6b+c |
| 14 | **Builder HALTS block production while hold is active** | Comentario en `step_builder` | `EntryVerificationHold` expone `is_blocking_build() -> bool`; `BuilderStage::Build` no puede ejecutarse si `is_blocking_build` | 1.6 |
| 15 | **Withdrawal trigger revert on L1 causes REWIND, not log** | Lógica en `flush_to_l1` post-submit | `TriggerExecutionResult::{Confirmed, RevertedNeedsRewind(entry_block)}` | 2.7b |
| 16 | **§4f filtering is generic (CrossChainCallExecuted events), NOT Bridge selectors** | `filter_block_entries` + extract_l2_to_l1_tx_indices | Función única con type signature `(receipts, ccm_address) -> Vec<usize>`; NO parámetro de Bridge | 0.3 |
| 17 | **NEVER per-call simulate_l1_delivery for multi-call L2→L1** | Comentario en `l2_to_l1.rs` | `enum SimulationPlan { Single, CombinedThenAnalytical }` + función única `simulate_delivery()` que decide plan vía `simulation_plan_for(calls, promotion_decision)` | 3.6 |
| 18 | **L1 and L2 entry structures must MIRROR** | Comentario CLAUDE.md | Mirror tests (0.5) + shared `model.rs` en composer (3.2). No es tipo, pero sí test obligatorio y código compartido | 0.5, 3.2 |
| 19 | **NEVER swap (dest, source) for L1→L2 return call children** | Comentario en `table_builder.rs` | `enum CallOrientation { Forward, Return }` + función única `address_pair_for(orientation)` | 1.9a |
| 20 | **Return data shape: Void = 0 bytes; delivery_return_data → hashes; l2_return_data → scope resolution** | Múltiples comentarios en CLAUDE.md #245, #246 | `enum ReturnData { Void, NonVoid(Bytes) }` propagado por todos los builders | 1.10 |
| 21 | **Single L2→L1 + terminal return still promotes to multi-call continuation** | Bool condition en `l2_to_l1.rs` | `enum PromotionDecision { KeepSingle, PromoteToContinuation }` con reglas explícitas | 3.6 |
| 22 | **`publicInputsHash` uses block.timestamp, not block.number** | Código en `proposer.rs` + proxy sim | `ProofContext { block_timestamp: U256, … }` tipado en proposer; sim usa `blockOverride.time` obligatoriamente | 1.8 |
| 23 | **NEVER hardcode function selectors — `sol!` only** | Convención | Clippy lint local `disallowed_methods` + grep gate en CI que busca `0x[a-f0-9]{8}` en strings | 4.4 |

## 7. La ruta MVP (priority order)

Si el refactor se interrumpe, hacé en este orden:

```
0.3 → 0.4 → 0.5 → 0.6 → 0.7 → 0.8           (red de seguridad + BASELINE)
1.4 → 1.4b → 1.5 → 1.6 → 1.6b+c → 1.7 → 1.8 → 1.10
                                              (typestate del driver + return data + queued enums)
2.4 → 2.5 → 2.6 → 2.7 → 2.7b → 2.8           (romper flush_to_l1 y verify con forward+triggers)
3.0 → 3.1 → ... → 3.7                         (sim_client ANTES de direction trait, recién ahora unificar L1↔L2)
```

> "El mayor retorno está en tipar el estado del driver y hacer explícita la máquina de estados de flush/verify. El `Direction` trait vale la pena, pero no antes de que el comportamiento ya esté encapsulado." — Codex (pasada 1)
>
> "Mover 5.6 a Fase 1, adelantar `sim_client` antes de 3.4, agregar baseline capture en Fase 0, y corregir el DoD para que no prometa partir archivos sin tener pasos." — Codex (pasada 2)

---

## 8. Plan detallado por fases

**Convención de cada paso**:
> **N.M** Descripción imperativa.
> *Archivos:* `path/file.rs[:Lstart-Lend]` ...
> *Verifica:* comando `cargo …` específico.
> *Branch:* `incremental` (puede compartir) o `dedicada` (PR propio).

---

### Fase 0 — Mapping, Guardrails & Baseline

**Objetivo**: poner red de seguridad, mapas, y una **baseline byte-level** del comportamiento actual. Esta fase no cambia código de producción excepto tests y scripts nuevos.

**0.1** Crear `docs/refactor/ARCH_MAP.md` con el diagrama de §5 + un walkthrough de 1 página de cada flujo (`postBatch` outbound; `derivation` inbound; `composer_rpc` hold-then-forward). Referencias `file:line` a las funciones tope.
*Archivos:* `docs/refactor/ARCH_MAP.md` (nuevo)
*Verifica:* `cargo build --release`
*Branch:* `refactor/phase-0-mapping` (incremental)

**0.2** Crear `docs/refactor/INVARIANT_MAP.md` con la tabla de §6 expandida — una fila por cada regla dura de `CLAUDE.md > Lessons Learned` con columnas: regla, owner actual (`file:line` o "convención"), test actual (si existe), tipo futuro, fase del plan.
*Archivos:* `docs/refactor/INVARIANT_MAP.md` (nuevo)
*Verifica:* `cargo build --release`
*Branch:* incremental

**0.3** Property tests para las invariantes que viven en filtering, en los archivos correctos:
- `cross_chain_tests.rs`: `partition_entries` (`:2022`) estable y disjoint; `identify_trigger_tx_indices` (`:2172`) dedup por orden.
- **`derivation_tests.rs`** (faltaba en v1): `compute_consumed_trigger_prefix` (`cross_chain.rs:2237`) es prefix-monótono, `filter_block_entries` preserva el conteo de deposits y L2→L1 txs. **Esto cierra invariantes #4 y #16.**
Usar `proptest` (ya en dev-deps).
*Archivos:* `cross_chain_tests.rs`, `derivation_tests.rs`
*Verifica:* `cargo nextest run -p based-rollup cross_chain:: derivation::`
*Branch:* incremental

**0.4** Property test para `table_builder.rs::reorder_for_swap_and_pop` (`:124`): conserva multiset, preserva orden relativo por grupo, idempotente para grupos ≤ 2.
*Archivos:* `table_builder_tests.rs`
*Verifica:* `cargo nextest run -p based-rollup table_builder::`
*Branch:* incremental

**0.5** **DSL neutral para mirror tests** — debe vivir en `src/` para ser importable desde unit tests `*_tests.rs` (corrección Codex p4: tests bajo `src/` no pueden importar desde `tests/fixtures/`). Crear `crates/based-rollup/src/test_support/mirror_case.rs` bajo el feature `test-utils` (que ya existe):
```rust
// crates/based-rollup/src/test_support/mod.rs
#[cfg(any(test, feature = "test-utils"))]
pub mod mirror_case;

// crates/based-rollup/src/test_support/mirror_case.rs
pub struct MirrorCase {
    pub name: &'static str,
    pub calls: Vec<LogicalCall>,        // dirección-agnóstica
    pub expected_l1_shape: EntryShape,
    pub expected_l2_shape: EntryShape,
    pub expected_action_hashes: Vec<B256>,
}

pub fn canonical_cases() -> Vec<MirrorCase> {
    vec![
        deposit_simple(),
        withdrawal_simple(),
        flash_loan_3_call(),
        ping_pong_depth_2(),
        ping_pong_depth_3(),
    ]
}
```
Con esta DSL, los mirror tests son un loop sobre `canonical_cases()`. Importable desde `table_builder_tests.rs` y `cross_chain_tests.rs` (ambos son sibling files de `table_builder.rs`/`cross_chain.rs`).
*Archivos:* `src/test_support/mod.rs`, `src/test_support/mirror_case.rs` (nuevos), `src/lib.rs` (declarar el módulo bajo cfg), `table_builder_tests.rs`
*Verifica:* `cargo nextest run -p based-rollup table_builder::mirror_`
*Branch:* incremental

**0.6** Fixtures de trazas reentrantes mínimas (JSON en `crates/based-rollup/tests/fixtures/traces/`): (a) call simple L1→L2 y L2→L1, (b) flash loan 3-call, (c) PingPong depth-2 y depth-3, (d) top-level revert, (e) child continuation, (f) multi-call `CallTwice`. Cargadas desde `composer_rpc/l1_to_l2_tests.rs` y `composer_rpc/l2_to_l1_tests.rs` y también desde la DSL de 0.5.
*Archivos:* `tests/fixtures/traces/*.json`, `composer_rpc/l1_to_l2_tests.rs`, `composer_rpc/l2_to_l1_tests.rs`
*Verifica:* `cargo nextest run -p based-rollup composer_rpc::`
*Branch:* incremental

**0.7** Expandir cobertura de hold/mismatch/rewind alrededor de `flush_to_l1` (`driver.rs:1796`) y `verify_local_block_matches_l1` (`:3204`). Tests requeridos: "hold set ANTES de submit", "hold cleared on verify match", "defer 3 veces y rewind", "hold no se setea si no hay entries", "rewind cycle clamps al anchor", "withdrawal trigger revert causa rewind" (**cierra invariante #15**).
*Archivos:* `driver_tests.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::tests::test_hold driver::tests::test_full_rewind driver::tests::test_withdrawal_trigger_revert_rewind`
*Branch:* incremental

**0.8** ⭐ **Capture baseline** (nuevo, insistencia de Codex pasada 2). Crear `scripts/refactor/capture_baseline.sh` que arranca devnet-eez limpia, corre los E2E canónicos, y guarda:
- `postBatch` calldata hex (de cada postBatch).
- `BatchPosted` y `ExecutionConsumed` logs (con action hashes).
- State roots por bloque L1 y L2.
- Orden de tx por bloque.
- Output en `tests/baseline/<scenario>.json` (committed al repo).
Escenarios: `bridge`, `crosschain`, `flashloan`, `multi-call-cross-chain`, `test-depth2-generic`. **Sin esto, 5.7 (replay gate) es imposible.**
*Archivos:* `scripts/refactor/capture_baseline.sh` (nuevo), `tests/baseline/*.json` (committed)
*Verifica:* `bash scripts/refactor/capture_baseline.sh && git status tests/baseline/`
*Branch:* incremental

**Cierre Fase 0**: `cargo nextest run --workspace && cargo clippy --workspace --all-features -- -D warnings && bash scripts/e2e/bridge-health-check.sh && bash scripts/e2e/crosschain-health-check.sh && bash scripts/refactor/capture_baseline.sh`

---

### Fase 1 — Tipos para invariantes (typestate, newtypes, sealed traits)

**Objetivo**: codificar las invariantes críticas de §6 en el sistema de tipos (las que admiten compile-time gating). Fase de mayor retorno por LOC tocado.

**1.1a** Newtype `RollupId(U256)` en `cross_chain.rs`. Implementa `From<U256>` marcado `pub(crate)` + `from_bytes_at_boundary(bytes: &[u8])` para ABI decode. **Sólo el newtype y su impl — no migrar callsites.**
*Archivos:* `cross_chain.rs`
*Verifica:* `cargo build --release && cargo nextest run -p based-rollup cross_chain::`
*Branch:* incremental

**1.1b** Migrar callsites de `U256` → `RollupId` en `cross_chain.rs` y `table_builder.rs`. **Branch dedicada** porque toca >20 callsites. Usar `cargo check` iterativo.
*Archivos:* `cross_chain.rs`, `table_builder.rs`, `cross_chain_tests.rs`, `table_builder_tests.rs`
*Verifica:* `cargo nextest run -p based-rollup cross_chain:: table_builder::`
*Branch:* `refactor/phase-1-rollup-id` (dedicada)

**1.1c** Newtype `ScopePath(Vec<U256>)` + migración en los callsites que hoy usan `Vec<U256>` para scope. Incluye helper `ScopePath::enter(&mut self, U256)` y `ScopePath::exit(&mut self)`.
*Archivos:* `cross_chain.rs`, `table_builder.rs`
*Verifica:* `cargo nextest run -p based-rollup cross_chain:: table_builder::`
*Branch:* incremental

**1.2** Newtypes de state roots con **module-private constructors + boundary wrappers** (corregido de v1). **Cierra invariante #3.**
```rust
// cross_chain.rs
pub struct CleanStateRoot(B256);
pub struct SpeculativeStateRoot(B256);
pub struct NewStateRoot(B256);
pub struct ActionHash(B256);

impl CleanStateRoot {
    pub(crate) fn new(b: B256) -> Self { Self(b) }       // sólo dentro del módulo
    pub fn from_abi_boundary(b: B256) -> Self { Self(b) } // nombrado explícito en bordes
    pub fn from_log_boundary(b: B256) -> Self { Self(b) } // nombrado explícito en bordes
    pub fn as_bytes(&self) -> B256 { self.0 }
}
```
El grep para `from_*_boundary` lista todos los puntos de entrada — auditable a ojo. Ningún `CleanStateRoot(b)` tuple-struct constructor afuera del módulo.
*Archivos:* `cross_chain.rs`, `driver/`, `derivation.rs`, `proposer.rs`, `rpc.rs`, tests
*Verifica:* `cargo nextest run -p based-rollup`
*Branch:* `refactor/phase-1-state-root-types` (dedicada — toca 6+ archivos)

**1.3** Reemplazar `Option<usize>` por `enum ParentLink { Root, Child(AbsoluteCallIndex) }` en `table_builder.rs`, `rpc.rs` (`BuildExecutionTableCall`, `BuildL2ToL1Call`) y modelos locales del composer. **Cierra invariante #7 (parcial — el helper único vive en 3.3).**
*Archivos:* `table_builder.rs`, `rpc.rs`, `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`
*Verifica:* `cargo nextest run -p based-rollup table_builder:: composer_rpc::`
*Branch:* `refactor/phase-1-parent-link` (dedicada)

**1.4** Tres enums semánticos distintos (corrección de inconsistencia v1):
- `enum TxOutcome { Success, Revert }` reemplaza `tx_reverts: bool`.
- `enum EntryGroupMode { Chained, Independent }` reemplaza `l1_independent_entries: bool`.
- `enum EntryClass { Trigger, Continuation, Result, RevertContinue }` interno para classification.
**Cierra invariantes #5 (vía `EntryClass`) y la semántica de chained vs independent (vía `EntryGroupMode`).** La invariante #6 (result entry skipped) se cierra en **1.4b** con `QueuedCallRequest`.
*Archivos:* `rpc.rs`, `driver.rs`, `cross_chain.rs`, `table_builder.rs`, `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`
*Verifica:* `cargo nextest run -p based-rollup`
*Branch:* `refactor/phase-1-semantic-enums` (dedicada)

**1.4b** ⭐ **MOVIDO desde 1.11** (Codex p4: 1.6c dependía de `QueuedCallRequest` que no existía hasta 1.11, dependencia oculta — se adelanta a 1.4b). Reemplazar `QueuedCrossChainCall` y `QueuedL2ToL1Call` (en `rpc.rs`) por enums que separan explícitamente el caso simple del continuation. **Cierra invariante #6.**
```rust
pub enum QueuedCallRequest {
    Simple {
        call_entry: CrossChainExecutionEntry,
        result_entry: CrossChainExecutionEntry,  // SÓLO existe en Simple
        raw_l1_tx: Bytes,
        gas_price: u128,
        tx_outcome: TxOutcome,
        group_mode: EntryGroupMode,
    },
    WithContinuations {
        l2_table_entries: Vec<CrossChainExecutionEntry>,  // NO result_entry
        l1_entries: Vec<CrossChainExecutionEntry>,
        raw_l1_tx: Bytes,
        gas_price: u128,
        tx_outcome: TxOutcome,
        group_mode: EntryGroupMode,
    },
}
```
Se hace IMPOSIBLE construir un request `WithContinuations` con un `result_entry`. Mismo para `QueuedL2ToL1CallRequest` — ambos son variantes del mismo enum unificado.
*Archivos:* `rpc.rs`, `driver.rs`, `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`
*Verifica:* `cargo nextest run -p based-rollup`
*Branch:* `refactor/phase-1-queued-enums` (dedicada)

**1.5** **Eliminar los 4 vectores paralelos** de `driver.rs:130-140` y reemplazarlos:
```rust
pub struct PendingL1Group {
    pub entries: Range<usize>,
    pub mode: EntryGroupMode,
    pub trigger: Option<TriggerMetadata>,
}

pub struct PendingL1SubmissionQueue {
    entries: Vec<CrossChainExecutionEntry>,
    groups: Vec<PendingL1Group>,
}

impl PendingL1SubmissionQueue {
    pub fn entry_mix(&self) -> BlockEntryMix { /* cierra invariante #11 */ }
    pub fn take_all(&mut self) -> (Vec<CrossChainExecutionEntry>, Vec<PendingL1Group>);
}
```
*Archivos:* `driver.rs`, `driver_tests.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::tests::test_flush_to_l1_ driver::tests::test_cross_chain_entries_`
*Branch:* incremental

**1.6** `EntryVerificationHold` con gate de builder y semántica idempotente (corrección Codex p2):
```rust
pub enum EntryVerificationHold {
    Clear,
    Armed { entry_block: u64, deferrals: u8 },
}

impl EntryVerificationHold {
    pub fn arm(&mut self, entry_block: u64); // idempotente: arm() del mismo block es no-op
    pub fn defer(&mut self) -> DeferralResult; // Continue(deferrals) | MustRewind { target: entry_block - 1 }
    pub fn clear(&mut self);
    pub fn is_armed(&self) -> bool;
    pub fn is_blocking_build(&self) -> bool; // cierra invariante #14
    pub fn armed_for(&self) -> Option<u64>;
}
```
Reemplaza `pending_entry_verification_block: Option<u64>` + `entry_verify_deferrals: u8`. Además `BuilderStage::Build` consulta `is_blocking_build()` antes de construir — **cierra invariante #14 (builder HALTS while hold active)**.
*Archivos:* `driver.rs`, `driver_tests.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::tests::test_hold driver::tests::test_consecutive_rewind_backoff driver::tests::test_builder_halts_while_hold_active`
*Branch:* incremental

**1.6b+c** ⭐ **FUSIONADO (Codex p4)**: `EntryQueue` struct concreto + `ForwardPermit` token + máquina de estados explícita de 3 estados. **Cierra invariante #13.**

**Semántica de "confirmed"** (precisado tras Codex p6/p8): el token `ForwardPermit` se emite EXACTAMENTE en el momento en que el driver completa la transición `Reserved → Confirmed` sobre un receipt. **Punto exacto en el código**: justo después de que `update_fork_choice(block_hash)` retorna `Ok(PayloadStatus::Valid)` o equivalente, indicando que reth canonizó y persistió el bloque que contiene las entries. NO al terminar `build_and_insert_block` (eso es sólo la construcción), NO al terminar `fork_choice_updated_with_retry` si retorna `SYNCING`, sino específicamente cuando reth marca el bloque como canónico en su chain state.

Esto cierra el caso `build OK + crash before FCU` (ventana de inconsistencia) y el caso `FCU SYNCING` (no canonizado todavía). Es el primer momento ordenado en el cual el user tx puede forwardearse safely: cualquier tx futuro que llegue al builder caerá en un bloque L2 posterior al que ya contiene las entries en reth → no hay race con `ExecutionNotFound`.

Reemplaza `queued_cross_chain_calls: Arc<Mutex<Vec<QueuedCrossChainCall>>>` y `queued_l2_to_l1_calls: Arc<Mutex<Vec<QueuedL2ToL1Call>>>` (driver.rs:123, 129):

```rust
// crates/based-rollup/src/entry_queue.rs (nuevo)
use crate::rpc::QueuedCallRequest;  // tipo unificado del paso 1.4b

/// Token estable emitido por push(); opaque, no índice positional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueueReceipt(u64);  // monotonic counter

/// Estado de un receipt en la cola. Transiciones:
///   Pending     → Reserved   (driver llama drain_pending al iniciar build de bloque)
///   Reserved    → Confirmed  (driver llama confirm tras FCU exitoso del bloque)
///   Reserved    → Pending    (driver llama rollback si build/FCU falla)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptState { Pending, Reserved, Confirmed }

/// Token zero-cost. Sólo se construye en wait_confirmation tras observar Confirmed.
#[must_use = "ForwardPermit must be passed to forward_user_tx; never drop"]
pub struct ForwardPermit { _seal: () }

#[derive(Clone, Default)]
pub struct EntryQueue {
    inner: Arc<Mutex<QueueState>>,
    notify: Arc<Notify>,
}

struct QueueState {
    next_id: u64,
    items: BTreeMap<QueueReceipt, (QueuedCallRequest, ReceiptState)>,
}

impl EntryQueue {
    /// Composer pushes; receipt empieza en Pending.
    pub async fn push(&self, req: QueuedCallRequest) -> QueueReceipt;

    /// Composer espera hasta que el receipt esté en Confirmed.
    /// - Wakea via `notify`; tras wake, re-chequea estado (idempotente, robusto a wakeups perdidos).
    /// - Retorna Err si el receipt fue evicted (timeout, shutdown, o invalid).
    /// - El ForwardPermit es la única forma de invocar forward_user_tx().
    pub async fn wait_confirmation(&self, receipt: QueueReceipt) -> Result<ForwardPermit>;

    /// Driver: drena hasta `max` items en estado Pending → Reserved.
    /// Devuelve los items para que el driver los incluya en el bloque que está construyendo.
    pub fn drain_pending(&self, max: usize) -> Vec<(QueueReceipt, QueuedCallRequest)>;

    /// Driver: confirma receipts (Reserved → Confirmed) tras FCU exitoso del bloque.
    /// Wakea waiters de wait_confirmation.
    pub fn confirm(&self, receipts: &[QueueReceipt]);

    /// Driver: rollbackea receipts (Reserved → Pending) si el build/FCU falló.
    /// No wakea waiters (siguen esperando Confirmed).
    pub fn rollback(&self, receipts: &[QueueReceipt]);
}

/// Función libre que fuerza consumo del permit antes de forwardear.
pub async fn forward_user_tx(permit: ForwardPermit, raw_tx: &str, upstream: &str) -> Result<Response> {
    let _consume = permit;  // moved → no se puede forwardear sin haber esperado Confirmed
    // ... forward HTTP call
}
```

**Por qué la transición precisa importa**:
- Si `confirm` se llamara en `drain_pending` (al empezar el build), el composer forwarderaría antes de tener garantía de que el bloque se persistió → race con un crash post-drain pre-FCU.
- Si se llamara después de submit a L1, el composer esperaría innecesariamente segundos (slot L1) para forwardear algo que ya es safe localmente.
- El punto correcto es **post-FCU**: el bloque está committed en reth, cualquier tx futuro caerá en un slot posterior, y aunque el driver caiga después, al recover el bloque sigue ahí.

**Riesgo concurrente (§11)**: el `Notify` puede tener wakeups perdidos en teoría, pero `wait_confirmation` re-chequea estado tras wake (no asume que wake = confirmed), así que es idempotente. Test obligatorio: 1000 push concurrentes + 1000 wait desde tasks separados, asertear que ningún waiter se cuelga.

*Archivos:* `entry_queue.rs` (nuevo), `driver.rs`, `composer_rpc/common.rs`, `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`, tests
*Verifica:* `cargo nextest run -p based-rollup driver:: composer_rpc:: entry_queue::`
*Branch:* `refactor/phase-1-entry-queue` (dedicada)

**1.7** ⭐ **Typestate `FlushPlan<S>`**. **Cierra invariante #1.** Correcciones incorporadas de Codex p2:

```rust
// Tres variantes: NoEntries para bloques-only, Collected para entries pendientes, HoldArmed para listo-a-enviar.
pub struct FlushPlan<S> {
    blocks: Vec<PendingBlock>,                    // OWNED (no borrow — async safety)
    entries: Vec<CrossChainExecutionEntry>,       // OWNED
    groups: Vec<PendingL1Group>,                  // OWNED
    entry_block: Option<u64>,                     // carga el entry_block ADENTRO del plan
    _marker: PhantomData<S>,
}

pub struct NoEntries;   // bloques-only, no necesita hold
pub struct Collected;   // tiene entries, hold sin armar
pub struct HoldArmed;   // tiene entries, hold armado para el entry_block correcto

impl FlushPlan<NoEntries> {
    pub fn new_blocks_only(blocks: Vec<PendingBlock>) -> Self { ... }
}

impl FlushPlan<Collected> {
    pub fn new(blocks: Vec<PendingBlock>, queue: PendingL1SubmissionQueue) -> Self {
        // entry_block se calcula aquí a partir del último block con entries
    }
    pub fn arm_hold(self, hold: &mut EntryVerificationHold) -> FlushPlan<HoldArmed> {
        if let Some(eb) = self.entry_block {
            hold.arm(eb); // idempotente
        }
        FlushPlan { _marker: PhantomData, ..self }
    }
}

// ApplicableForSend es un trait sellado para ambas variantes que pueden enviarse
trait Sendable: sealed::Sealed {}
impl Sendable for NoEntries {}
impl Sendable for HoldArmed {}

impl Proposer {
    pub async fn send_to_l1<S: Sendable>(&self, plan: FlushPlan<S>) -> SendResult {
        // NoEntries: sólo bloques, no toca hold.
        // HoldArmed: fue armado con el entry_block correcto antes.
        // Collected: NO COMPILA — el borrow checker lo rechaza.
    }
}

// Failure path explícito: si send_to_l1 falla después de arm, caller debe consumir el error.
#[must_use]
pub enum SendResult {
    Ok { consumed_prefix: usize },
    Failed { needs_rollback: RollbackAction },
}

pub enum RollbackAction {
    ClearHoldAndCooldown { entry_block: u64 },
    ResetNonceAndRewind { target: u64 },
}
```
Con esto:
- No se puede llamar `send_to_l1` con `Collected` (error de compilación).
- `NoEntries` pasa derecho sin armar hold (corrige el problema que Codex señaló: hoy `send_to_l1` sirve para bloques-only).
- `entry_block` viaja dentro del plan, así que `arm_hold` no puede armar para el bloque equivocado.
- `FlushPlan` OWNS todo — no hay borrows vivos cruzando `.await`.
- `SendResult` es `#[must_use]` con un enum explícito de rollback — ignorarlo es warning.
- `arm_hold` es idempotente (si hold ya estaba armado para ese bloque, no-op) porque `EntryVerificationHold::arm` lo es (1.6).
*Archivos:* `driver/flush.rs`, `proposer.rs`, `driver_tests.rs`, `proposer_tests.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::tests::test_hold_ driver::tests::test_flush_to_l1_ proposer::tests::`
*Branch:* `refactor/phase-1-flushplan-typestate` (dedicada — paso central de la fase)

**1.8** `L1NonceReservation` + `#[must_use] NonceResetRequired` + `ProofContext` + `L1Client` wrapper concreto (revisado tras Codex p3: originalmente `trait L1Provider`, descartado — alloy ya tiene su `Provider` trait y doble-abstraerlo es ceremonia). **Cierra invariantes #2 y #22.**

```rust
// proposer.rs
pub struct L1NonceReservation { nonce: u64, key: Address }

#[must_use = "NonceResetRequired must be consumed by calling proposer.reset_nonce()"]
pub struct NonceResetRequired { _seal: () }

pub struct ProofContext {
    pub block_timestamp: U256,
    pub blob_hashes: Vec<B256>,
    pub entry_hashes: Vec<B256>,
    pub call_data_hash: B256,
}

/// Wrapper concreto local a proposer. Métodos narrow — sólo lo que proposer realmente usa de alloy.
/// Si más adelante necesitamos mockearlo para tests, extraemos un trait entonces (no preemptivamente).
pub(crate) struct L1Client {
    inner: RootProvider,
}

impl L1Client {
    pub async fn get_nonce(&self, addr: Address) -> Result<u64>;
    pub async fn send_tx(&self, tx: TransactionRequest) -> Result<B256>;
    pub async fn get_balance(&self, addr: Address) -> Result<U256>;
    pub async fn last_submitted_state_root(&self) -> Result<B256>;
    // ... sólo lo que proposer necesita, no un wrapper ancho sobre RootProvider
}

impl Proposer {
    pub async fn reserve_nonce(&mut self) -> L1NonceReservation;
    pub async fn send_with(&mut self, res: L1NonceReservation, tx: ...) -> Result<(), NonceResetRequired>;
    pub async fn reset_nonce(&mut self, _token: NonceResetRequired) -> Result<()>;
    pub fn sign_proof(&self, ctx: ProofContext) -> Signature;  // sólo acepta ProofContext
}
```

El `L1Client` consolida los accesos al provider en un solo lugar de `proposer.rs` y deja el resto del código (`driver/`, `derivation.rs`) usando `RootProvider` directamente — esto no se toca por ahora. Si eventualmente hace falta mockeo end-to-end, el struct tiene ya todos los métodos listos para convertirse en trait con un solo commit.

*Archivos:* `proposer.rs`, `proposer_tests.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::tests::test_submission_failure_sets_cooldown driver::tests::test_submission_success_clears_cooldown proposer::tests::`
*Branch:* incremental

**1.9a** `ImmediateEntryBuilder` en `cross_chain.rs`. Encapsula `build_l2_to_l1_call_entries` (`:819`) y la lógica de `address_pair_for(CallOrientation::Forward | Return)` — **cierra invariante #19**. Migrar el primer callsite en el mismo commit; dejar la función antigua como `#[deprecated]` wrapper que llama al builder.
*Archivos:* `cross_chain.rs`, `cross_chain_tests.rs`
*Verifica:* `cargo nextest run -p based-rollup cross_chain::tests::test_build_l2_to_l1_call_entries_`
*Branch:* incremental

**1.9b** `DeferredEntryBuilder` para entries diferidos (`build_cross_chain_call_entries`, `:719`) + `RevertGroupBuilder` para `attach_chained_state_deltas` (`:1520`). `RevertGroupBuilder` encapsula "first trigger needs currentState=clean after swap-and-pop reorder" — **cierra invariante #8**.
*Archivos:* `cross_chain.rs`, `cross_chain_tests.rs`
*Verifica:* `cargo nextest run -p based-rollup cross_chain::tests::test_attach_generic_state_deltas_`
*Branch:* incremental

**1.9c** `L2ToL1ContinuationBuilder` para `build_l2_to_l1_continuation_entries` (`table_builder.rs:1612`). API:
```rust
L2ToL1ContinuationBuilder::new()
    .with_scope_return(ScopePath::from([0]))   // OBLIGATORIO — cierra invariante #12
    .add_entry(...)
    .build()?
```
El `build()` falla si no se llamó `with_scope_return` — hace imposible olvidar la scope navigation.
*Archivos:* `table_builder.rs`, `table_builder_tests.rs`
*Verifica:* `cargo nextest run -p based-rollup table_builder::tests::test_build_l2_to_l1_continuation_`
*Branch:* incremental

**1.10** ⭐ **NUEVO (Codex p2): `ReturnData` enum** — mata varias reglas de golpe (#20):
```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ReturnData {
    Void,               // 0 bytes — void function via assembly return
    NonVoid(Bytes),     // raw ABI-encoded return value
}

impl ReturnData {
    pub fn from_bytes(b: Bytes) -> Self {
        if b.is_empty() { Self::Void } else { Self::NonVoid(b) }
    }
    pub fn is_void(&self) -> bool { matches!(self, Self::Void) }
}
```
Propagar por `DetectedReturnCall`, `DetectedCall`, RPC JSON (`L2ReturnCall`), builders. Todos los sitios que hoy chequean `.is_empty()` pasan a `matches!(_, ReturnData::Void)`. Fix explícito de `delivery_return_data` y `l2_return_data` — las 4 sites de RESULT hash en CLAUDE.md #245 ahora son verificadas por el tipo.
*Archivos:* `cross_chain.rs`, `table_builder.rs`, `rpc.rs`, `composer_rpc/trace.rs`, `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`
*Verifica:* `cargo nextest run -p based-rollup`
*Branch:* `refactor/phase-1-return-data` (dedicada — cross-cuts muchos archivos)

**Cierre Fase 1**: `cargo nextest run --workspace && cargo clippy --workspace --all-features -- -D warnings && bash scripts/e2e/bridge-health-check.sh && bash scripts/e2e/flashloan-health-check.sh && bash scripts/e2e/test-multi-call-cross-chain.sh && bash scripts/refactor/replay_baseline.sh`

---

### Fase 2 — Romper funciones-mamut en pipelines / state machines

**Objetivo**: ninguna función >200 LOC en `driver.rs`. Comportamiento byte-idéntico (verificado vs baseline).

**2.1** **Split mecánico de `driver.rs` en submódulos** (revisado tras Codex p3: originalmente con capability traits, descartado porque con una sola impl un trait no fuerza disciplina de alcance — el doc comment no es enforcement. La disciplina real viene de los structs concretos de 2.2-2.7). Estructura:
```
driver/
  mod.rs           // struct Driver + new/run
  types.rs         // structs públicos (BuiltBlock, PendingBlock, etc.)
  step.rs          // impl Driver { step, step_sync, step_fullnode }
  step_builder.rs  // impl Driver { step_builder + helpers }
  flush.rs         // impl Driver { flush_to_l1 + helpers }
  verify.rs        // impl Driver { verify_local_block_matches_l1, apply_*_filtering }
  protocol_txs.rs  // impl Driver { build_builder_protocol_txs } + ProtocolTxPlan stages (de 2.4)
  rewind.rs        // impl Driver { rewind_l2_chain, set_rewind_target }
  journal.rs       // impl Driver { tx_journal save/load/prune }
```

Varios bloques `impl Driver` en archivos distintos — Rust lo permite sin ceremonia. Usar `pub(super)` para métodos cross-module. Sin traits internos.

**La disciplina de alcance real** se logra con los structs de 2.2-2.7: `BuilderTickContext`, `FlushPrecheck`, `FlushAssembly`, `FlushPlan<S>`, `VerificationDecision`, `ProtocolTxPlan<Stage>`, `ForwardAndTriggerPlan`. Cada uno recibe sólo los sub-campos que necesita por parámetro, no `&mut Driver` completo.

*Archivos:* `crates/based-rollup/src/driver/*.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::`
*Branch:* `refactor/phase-2-driver-split` (dedicada)

**2.2** Extraer de `step_builder` un `BuilderTickContext` con métodos `derive_target_block`, `compute_mode_transition`, `load_l1_context`. Reduce el tope de la función a ~200 LOC.
*Archivos:* `driver/step_builder.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::tests::test_driver_mode_ driver::tests::test_target_l2_block_from_future_timestamp`
*Branch:* incremental

**2.3** Extraer `QueueDrain` con `drain_rpc_queues`, `merge_pending_entries`, `inject_held_l2_txs`. La función `inject_held_l2_txs` (`driver.rs:2970`) ya existe.
*Archivos:* `driver/step_builder.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::tests::test_pending_cross_chain_entries_accumulate driver::tests::test_cross_chain_entries_accumulate_across_blocks_before_flush`
*Branch:* incremental

**2.4** Extraer de `build_builder_protocol_txs` (`:3947`) un `ProtocolTxPlan` **con stages tipados** (corrección v2: no `Vec<TransactionSigned>` por stage). Cada stage consume y produce un estado tipado:
```rust
struct Draft; struct WithContext; struct WithTables; struct WithTriggers;

impl ProtocolTxPlan<Draft> {
    fn bootstrap(self, block_num: u64) -> ProtocolTxPlan<WithContext>;
}
impl ProtocolTxPlan<WithContext> {
    fn set_context(self, l1_block: u64) -> Self;
    fn load_tables(self, entries: &[CrossChainExecutionEntry]) -> ProtocolTxPlan<WithTables>;
}
impl ProtocolTxPlan<WithTables> {
    fn append_triggers(self, triggers: &[TriggerMetadata]) -> ProtocolTxPlan<WithTriggers>;
}
impl ProtocolTxPlan<WithTriggers> {
    fn build(self, nonce_base: u64) -> Vec<TransactionSigned>;
}
```
El orden de stages se vuelve imposible de equivocar.
*Archivos:* `driver/protocol_txs.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::tests::test_gap_fill_block_at_block_1_uses_deployment_context driver::tests::test_builder_assigns_entries_only_to_last_block_in_batch`
*Branch:* incremental

**2.5** Extraer de `verify_local_block_matches_l1` un `enum VerificationDecision { Match, Defer { reason }, MismatchRewind { target }, MismatchImmutable }` + método único `Driver::rewind_to_re_derive(entry_block: u64)`. **Cierra invariantes #9 y #10.**
*Archivos:* `driver/verify.rs`, `driver/rewind.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::tests::test_hold_cleared_on_verification_match driver::tests::test_immutable_ceiling_skips_verification driver::tests::test_full_rewind_cycle_state_transitions`
*Branch:* incremental

**2.6** Extraer de `flush_to_l1` un `FlushPrecheck` con `check_cooldown`, `check_balance`, `drop_l1_confirmed`, `decide_rewind_on_root_mismatch`. Retorna `enum PrecheckResult { Proceed, Skip, Rewind { target } }`.
*Archivos:* `driver/flush.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::tests::test_flush_to_l1_respects_submission_cooldown driver::tests::test_l1_confirmed_anchor_rewind_uses_anchor`
*Branch:* incremental

**2.7** Extraer `FlushAssembly` con `collect_submission_entries`, `collect_forward_txs`, `collect_trigger_txs`, `compute_group_order`. Produce `FlushPlan<Collected>` (del step 1.7).
*Archivos:* `driver/flush.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::tests::test_flush_to_l1_unified_submission driver::tests::test_flush_ordering_includes_forward_queued_l1_txs`
*Branch:* incremental

**2.7b** ⭐ **NUEVO (Codex p2): `ForwardAndTriggerPlan` + `TriggerExecutionResult`**. Hoy `flush_to_l1` hace: submit postBatch → forward queued user txs → send triggers → await receipts → decidir rewind por revert. Eso son 4 responsabilidades no modeladas. **Cierra invariante #15.**
```rust
pub struct ForwardAndTriggerPlan {
    pub queued_user_txs: Vec<Bytes>,
    pub triggers: Vec<TriggerMetadata>,
}

#[must_use]
pub enum TriggerExecutionResult {
    AllConfirmed { consumed: usize },
    ForwardFailed { tx_hash: B256, reason: String },
    TriggerReverted { entry_block: u64, needs_rewind: bool }, // invariante #15
    BuilderNonceStale(NonceResetRequired),
}
```
*Archivos:* `driver/flush.rs`, `proposer.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::tests::test_withdrawal_trigger_revert_rewind driver::tests::test_forward_queued_l1_txs`
*Branch:* incremental

**2.8** **Reescribir** `step_builder` y `flush_to_l1` como orquestadores de ~80 LOC cada uno sobre dos enums **completos** (corrección v2: v1 tenía FlushStage submodelado):
```rust
enum BuilderStage {
    CatchUp,          // derive_next_batch y verify
    Drain,            // merge queues + inject held txs
    Build,            // construir bloque (no ejecutar si hold.is_blocking_build())
    MaybeFlush,       // decidir llamar a flush_to_l1
    Done,
}

enum FlushStage {
    Precheck,             // 2.6 FlushPrecheck
    Collect,              // 2.7 FlushAssembly -> FlushPlan<Collected>
    ArmHold,              // 1.7 FlushPlan<HoldArmed>
    Submit,               // proposer.send_to_l1 — retorna SendResult
    ForwardUserTxs,       // 2.7b forward queued_user_txs
    SendTriggers,         // 2.7b send triggers
    AwaitReceipts,        // 2.7b await + classify
    HandleTriggerResult,  // 2.7b: Confirmed | RevertedNeedsRewind | BuilderNonceStale
    ClearOrRewind,        // terminal
}
```
Comportamiento byte-idéntico vs baseline.
*Archivos:* `driver/step_builder.rs`, `driver/flush.rs`
*Verifica:* `cargo nextest run -p based-rollup driver::` + `bash scripts/refactor/replay_baseline.sh`
*Branch:* `refactor/phase-2-driver-pipelines` (dedicada — paso final de la fase)

**Cierre Fase 2**: `cargo nextest run --workspace && cargo clippy --workspace --all-features -- -D warnings && bash scripts/e2e/bridge-health-check.sh && bash scripts/e2e/crosschain-health-check.sh && bash scripts/e2e/flashloan-health-check.sh && bash scripts/e2e/double-deposit-withdrawal-trace.sh && bash scripts/refactor/replay_baseline.sh`

---

### Fase 3 — Unificar `composer_rpc` L1↔L2 detrás de `Direction`

**Objetivo**: convertir `l1_to_l2.rs` y `l2_to_l1.rs` en adapters finos sobre un único motor parametrizado por dirección. **Sólo se entra a esta fase cuando 2.x está estable y las queued enums (1.4b) están mergeadas.**

**3.0** ⭐ **MOVIDO DE 4.7 + dependency inversion**: crear `composer_rpc/sim_client.rs` con `trait SimulationClient` + impl real + mock para tests.

```rust
#[async_trait]
pub trait SimulationClient: Send + Sync + 'static {
    async fn eth_call_view(&self, req: CallRequest) -> Result<Bytes>;
    async fn debug_trace_call_many(&self, bundle: CallManyBundle) -> Result<CallManyResponse>;
    async fn get_block_context(&self, block: BlockId) -> Result<BlockContext>;
    async fn get_verification_key(&self, rollup_id: RollupId) -> Result<Bytes>;
    async fn get_rollup_state_root(&self, rollup_id: RollupId, block: BlockId) -> Result<B256>;
}

pub struct HttpSimClient { client: reqwest::Client, upstream: String }
impl SimulationClient for HttpSimClient { ... }  // impl real

#[cfg(any(test, feature = "test-utils"))]
pub struct InMemorySimClient { /* fixture-backed */ }
#[cfg(any(test, feature = "test-utils"))]
impl SimulationClient for InMemorySimClient { ... }
```

Ambas direcciones toman `Arc<dyn SimulationClient>`. Los tests de composer_rpc cargan `InMemorySimClient` con fixtures de `tests/fixtures/traces/` (de 0.6) — ya no requieren un upstream HTTP real.

**Racional (orden)**: si primero extraemos `discover_until_stable` (3.4) sobre el JSON armado ad-hoc, vamos a re-tocarlo al tipar el cliente. Tipar + dependency invert primero.

*Archivos:* `composer_rpc/sim_client.rs` (nuevo), `composer_rpc/common.rs`, `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`, tests
*Verifica:* `cargo nextest run -p based-rollup composer_rpc::`
*Branch:* `refactor/phase-3-sim-client-trait` (dedicada)

**3.1** Crear `composer_rpc/direction.rs` con sealed trait y tipos asociados. El trait contiene **sólo facts y hooks direction-specific** — nada de policy de simulación (eso vive en `simulate.rs` del 3.6):
```rust
mod sealed { pub trait Sealed {} }

/// Resultado de clasificar un call de la trace: o es forward (descubrimos un nuevo cross-chain call)
/// o es return (un edge que cierra un call previo). Una sola función produce ambos.
pub enum ClassifiedCall {
    Forward(DiscoveredCall),
    Return(ReturnEdge),
}

pub trait Direction: sealed::Sealed {
    type RootCall;
    type ChildCall;
    type SimulationArtifact;

    fn name() -> &'static str;
    fn ccm_address_on_target_chain(&self) -> Address;

    // Hooks direction-specific usados por discover_until_stable (3.4):
    /// Clasifica un nodo del trace como forward call, return edge, o ninguno.
    fn classify_call(trace_call: &TraceCall) -> Option<ClassifiedCall>;

    /// Dado un round de discovery, produce los siguientes requests para expansión.
    fn expand_round(round: &DiscoveryRound) -> Vec<ExpansionRequest>;

    /// Decide si el discovered set debe promoverse a multi-call continuation.
    /// Cierra invariante #21: single L2→L1 + terminal return → PromoteToContinuation aunque len()==1.
    fn promotion_rule(calls: &[DiscoveredCall], returns: &[ReturnEdge]) -> PromotionDecision;

    /// Construye el payload de cola a partir del discovered set + artifact de simulación.
    /// Usado por 3.5. Cierra invariante #6 (Simple vs WithContinuations).
    fn build_queue_payload(
        discovered: &DiscoveredSet,
        artifact: &Self::SimulationArtifact,
    ) -> QueuedCallRequest;
}

pub struct L1ToL2;  impl sealed::Sealed for L1ToL2 {}
pub struct L2ToL1;  impl sealed::Sealed for L2ToL1 {}
```
Scaffold + `impl Direction for L1ToL2` y `impl Direction for L2ToL1` con los métodos `panic!()` hasta que los pasos siguientes los llenen. Sin migrar lógica.
*Archivos:* `composer_rpc/direction.rs` (nuevo), `composer_rpc/mod.rs`
*Verifica:* `cargo nextest run -p based-rollup composer_rpc::`
*Branch:* incremental

**3.2** Extraer modelos compartidos hacia `composer_rpc/model.rs`: `DiscoveredCall`, `ReturnEdge`, `DiscoveryRound`, `QueuePlan`, `SimulationArtifact`. Dos archivos fuente, muchos imports, tests de ambos lados — requiere branch dedicada.
*Archivos:* `composer_rpc/model.rs` (nuevo), `l1_to_l2.rs`, `l2_to_l1.rs`
*Verifica:* `cargo nextest run -p based-rollup composer_rpc:: table_builder::`
*Branch:* `refactor/phase-3-composer-model` (dedicada — toca los dos archivos de 4k/5k LOC)

**3.3** Extraer **rebasing de índices padre/hijo** a un helper compartido `rebase_parent_links(&mut [DiscoveredCall], offset: usize)` y borrar la lógica duplicada de `l1_to_l2.rs:3337`, `l2_to_l1.rs:4724`, `l2_to_l1.rs:4934`. **Cierra invariante #7 del todo.**
*Archivos:* `composer_rpc/model.rs`, `l1_to_l2.rs`, `l2_to_l1.rs`
*Verifica:* `cargo nextest run -p based-rollup composer_rpc:: table_builder::`
*Branch:* incremental

**3.4** Factorizar el **fixed-point discovery loop** de las dos funciones de >1.6k LOC (`l1_to_l2.rs:2280` y `l2_to_l1.rs:3558`) en `discover_until_stable<D: Direction>`. **Spec completa**:
```rust
async fn discover_until_stable<D: Direction>(
    sim: &(dyn SimulationClient),     // trait del step 3.0
    initial: DiscoveryRound,
) -> Result<DiscoveredSet> {
    // 1. Run sim.debug_trace_call_many para el round actual
    // 2. Extraer cross-chain calls + return calls vía D::classify_call
    // 3. Dedupe por (sender, target, calldata, value)
    // 4. Rebase parent_call_index hacia índice absoluto (usa rebase_parent_links de 3.3)
    // 5. Aplicar D::promotion_rule(calls, returns) -> PromotionDecision
    // 6. Check convergence: ¿el round no agregó calls nuevos?
    // 7. Si no converge: D::expand_round(round) produce el siguiente bundle
    // 8. Max rounds = MAX_RECURSIVE_DEPTH
}

pub struct DiscoveredSet {
    pub calls: Vec<DiscoveredCall>,
    pub returns: Vec<ReturnEdge>,
    pub promotion: PromotionDecision,  // propagada a simulate_delivery (3.6)
}
```
Los hooks `classify_call`, `expand_round`, `promotion_rule` son los que 3.1 ya define en `Direction`.
*Archivos:* `composer_rpc/discover.rs` (nuevo), `direction.rs`, `l1_to_l2.rs`, `l2_to_l1.rs`
*Verifica:* `cargo nextest run -p based-rollup composer_rpc::` + `bash scripts/e2e/deploy-ping-pong.sh` + `bash scripts/refactor/replay_baseline.sh`
*Branch:* `refactor/phase-3-discover-direction` (dedicada — rompe las dos funciones más grandes del crate)

**3.5** Factorizar construcción de payloads de cola detrás de `Direction::build_queue_payload` (declarado en 3.1), **retornando las variantes de `QueuedCallRequest`** del paso 1.4b directamente. Reemplaza `l1_to_l2.rs:413`, `l2_to_l1.rs:407, :584, :649`.
*Archivos:* `composer_rpc/queue.rs` (nuevo), `direction.rs`, `l1_to_l2.rs`, `l2_to_l1.rs`
*Verifica:* `cargo nextest run -p based-rollup composer_rpc:: driver::tests::test_cross_chain_entries_`
*Branch:* dedicada

**3.6** Factorizar simulación con **`enum SimulationPlan` + función** (revisado tras Codex p3: originalmente `trait SimulationStrategy` con `OrElse` componible, descartado — con 3 estrategias fijas cerradas en el crate, enum + match es más claro). Unifica `l1_to_l2.rs:1144, 1887`, `l2_to_l1.rs:1633, 2477`. **Cierra invariantes #17 y #21.**

```rust
// composer_rpc/simulate.rs
pub enum SimulationPlan {
    Single,                      // single call, no fallback
    CombinedThenAnalytical,      // multi-call OR single con terminal return: combined + fallback
}

/// Decide la estrategia según shape del input Y el `PromotionDecision` del discover loop.
/// Cierra invariantes #17 Y #21: multi-call nunca single-call sim, y single+terminal-return
/// se promueve a continuation (NO se queda en Single).
pub fn simulation_plan_for(
    calls: &[DiscoveredCall],
    promotion: PromotionDecision,  // viene del step 3.4 vía DiscoveredSet
) -> SimulationPlan {
    match promotion {
        PromotionDecision::PromoteToContinuation => SimulationPlan::CombinedThenAnalytical,
        PromotionDecision::KeepSingle => {
            if calls.len() > 1 {
                SimulationPlan::CombinedThenAnalytical
            } else {
                SimulationPlan::Single
            }
        }
    }
}

/// Ejecuta el plan. Punto único de entrada.
pub async fn simulate_delivery<D: Direction>(
    sim: &(dyn SimulationClient),
    calls: &[DiscoveredCall],
    promotion: PromotionDecision,
    ctx: &SimContext,
) -> eyre::Result<SimulationArtifact> {
    match simulation_plan_for(calls, promotion) {
        SimulationPlan::Single => {
            single_call_sim::<D>(sim, &calls[0], ctx).await
        }
        SimulationPlan::CombinedThenAnalytical => {
            match combined_sim::<D>(sim, calls, ctx).await {
                Ok(artifact) => Ok(artifact),
                Err(e) => {
                    tracing::warn!(target: "composer_rpc::sim", %e, "combined sim failed, falling back to analytical");
                    analytical_fallback::<D>(sim, calls, ctx).await
                }
            }
        }
    }
}
```

**Reemplaza explícitamente** las reglas "NEVER per-call simulate_l1_delivery for multi-call" (#17) Y "single L2→L1 + terminal return promotes to multi-call" (#21). Ambas invariantes ahora viven en un **único punto**: `simulation_plan_for`. El `PromotionDecision` lo produce `Direction::promotion_rule` en 3.1 y viaja por el `DiscoveredSet` desde `discover_until_stable` (3.4).

Las tres funciones privadas (`single_call_sim`, `combined_sim`, `analytical_fallback`) viven en `simulate.rs` como funciones libres con firmas idénticas. Testeables en aislamiento sin ceremony de trait impl.

*Archivos:* `composer_rpc/simulate.rs` (nuevo), `direction.rs`, `l1_to_l2.rs`, `l2_to_l1.rs`
*Verifica:* `cargo nextest run -p based-rollup composer_rpc:: table_builder::` + `bash scripts/e2e/flashloan-health-check.sh`
*Branch:* dedicada

**3.7** Reducir `l1_to_l2.rs` y `l2_to_l1.rs` a **adapters finos**: HTTP ingress + `impl Direction` + response shaping. Target: cada archivo <800 LOC (desde 4345/5459).
*Archivos:* `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`
*Verifica:* `cargo nextest run -p based-rollup composer_rpc::` + smoke E2E completo + `bash scripts/refactor/replay_baseline.sh`
*Branch:* dedicada

**Cierre Fase 3**: smoke completo (`bridge`, `crosschain`, `flashloan`, `multi-call-cross-chain`, `conditional-cross-chain`, `test-depth2-generic`, `deploy-ping-pong-return`) + `replay_baseline.sh`.

---

### Fase 4 — Separar capas en `composer_rpc/`

**Objetivo**: ningún archivo en `composer_rpc/` mezcla más de una responsabilidad.

**4.1** Split mecánico de cada dirección en submódulos:
```
composer_rpc/
  l1_to_l2/
    mod.rs
    server.rs    // HTTP ingress (delega a server.rs común del step 4.2)
    direction_impl.rs  // impl Direction for L1ToL2
    response.rs  // response shaping
  l2_to_l1/
    mod.rs ...
```
Movimiento mecánico.
*Archivos:* `composer_rpc/l1_to_l2/*`, `composer_rpc/l2_to_l1/*`
*Verifica:* `cargo nextest run -p based-rollup composer_rpc::`
*Branch:* `refactor/phase-4-composer-split` (dedicada)

**4.2** Mover parsing/response JSON-RPC a `composer_rpc/server.rs` (handler genérico). Cada `l1_to_l2/server.rs` y `l2_to_l1/server.rs` queda con la clasificación mínima de qué método pertenece a esta dirección.
*Archivos:* `composer_rpc/server.rs` (nuevo), `common.rs`
*Verifica:* `cargo nextest run -p based-rollup composer_rpc::`
*Branch:* incremental

**4.3** Mover decode/RLP helpers (`l1_to_l2.rs:4198` y equivalentes) a `composer_rpc/tx_codec.rs`.
*Archivos:* `composer_rpc/tx_codec.rs` (nuevo)
*Verifica:* `cargo nextest run -p based-rollup composer_rpc::`
*Branch:* incremental

**4.4** **Owner ELEGIDO (Codex p2): `cross_chain.rs`** — consolidar TODOS los selectors y ABI parsers en el `sol!` block existente. Eliminar literales duplicados. Agregar CI gate: `grep -rn "0x[a-f0-9]\{8\}" crates/based-rollup/src/*.rs crates/based-rollup/src/composer_rpc/` NO debe encontrar selectors fuera del bloque `sol!`. **Cierra invariante #23.**
*Archivos:* `cross_chain.rs`, todos los archivos con selectors hardcoded, `.github/workflows/ci.yml` (grep gate)
*Verifica:* `cargo nextest run -p based-rollup composer_rpc::trace:: cross_chain::`
*Branch:* incremental

**4.5** Separar `composer_rpc/trace.rs` en `trace/{walker,proxy,types}.rs` **Y al mismo tiempo** reemplazar `serde_json::Value` por structs serde tipados `TraceNode`, `TraceCallFrame`, `CallManyResponse` (ex-5.2, movido acá por Codex p2). Tocar el mismo subsistema dos veces (split + typing) es churn inútil — se hace junto.
*Archivos:* `composer_rpc/trace/{walker,proxy,types}.rs`
*Verifica:* `cargo nextest run -p based-rollup composer_rpc::trace::`
*Branch:* `refactor/phase-4-trace-typed` (dedicada)

**4.6** Crear `composer_rpc/entry_builder.rs` como **frontera única** hacia los builders de `cross_chain.rs` / `table_builder.rs` del paso 1.9. **Es una façade sobre 1.9**, no una capa nueva. Ambas direcciones llaman a `EntryBuilder::immediate(...)`, `EntryBuilder::deferred(...)`, `EntryBuilder::continuation(...)`, que delegan a `ImmediateEntryBuilder`/`DeferredEntryBuilder`/`L2ToL1ContinuationBuilder`.
*Archivos:* `composer_rpc/entry_builder.rs` (nuevo)
*Verifica:* `cargo nextest run -p based-rollup table_builder:: cross_chain:: composer_rpc::`
*Branch:* dedicada

**Cierre Fase 4**: smoke completo + `bash scripts/refactor/replay_baseline.sh`. Medir LOC: `l1_to_l2/` total y `l2_to_l1/` total (target directional — ver §12 ajustado).

---

### Fase 5 — Hardening (reducida)

**Objetivo**: la mayoría del hardening ya ocurrió en Fases 1-4 (por instrucción de Codex p2). Esta fase sólo quita los `unwrap()` restantes, agrega proptest/fuzz, y corre el replay final.

**5.1** Eliminar `unwrap()/expect()` de producción. Después de 4.5, `trace.rs` ya no tendrá el bulk de unwraps (typed structs eliminan la mayoría). Contamos los restantes y los atacamos con `eyre::WrapErr`.
*Archivos:* los que sigan teniendo `unwrap()` después de 4.5
*Verifica:* `cargo nextest run -p based-rollup && cargo clippy --workspace --all-features -- -D warnings -W clippy::unwrap_used`
*Branch:* incremental

**5.4** Agregar `proptest`/fuzz para: parser de traza (post-typing de 4.5), rebasing de parent links (post 3.3), roundtrip de entries (encode → decode → equal), mirror invariant (a partir de `MirrorCase` DSL del 0.5).
*Archivos:* `cross_chain_tests.rs`, `table_builder_tests.rs`, `composer_rpc/trace/`, `tests/fixtures/mirror_case.rs`
*Verifica:* `cargo nextest run -p based-rollup`
*Branch:* incremental

**5.7** **Gate final antes de merge a main**: `scripts/refactor/replay_baseline.sh` (creado en 0.8) debe producir **0 diffs** contra la baseline pre-refactor. Los E2E ya cubren happy paths; este gate verifica **byte-equivalencia** con el sistema previo. Commit del output final en `docs/refactor/REPLAY_RESULTS.md`.
*Archivos:* `docs/refactor/REPLAY_RESULTS.md`
*Verifica:* `cargo build --release && cargo nextest run --workspace && cargo clippy --workspace --all-features -- -D warnings && bash scripts/refactor/replay_baseline.sh`
*Branch:* `refactor/release-prep` (dedicada)

**Nota (Codex p2/p4)**: los ex-pasos `5.2` (typed trace nodes) se mergearon en `4.5`. El ex-`5.3` (typed RPC structs) está cubierto por los pasos de sim_client (3.0) + queued enums (1.4b) — ya no necesita un paso aparte. El ex-`5.5` (checked constructors + debug_assert) ya vive dentro de 1.2/1.9 donde se crean los tipos. El ex-`5.6` (enums en rpc.rs) se movió a **1.4b** (Codex p4 lo adelantó para resolver dependencia con EntryQueue de 1.6b+c).

**Cierre Fase 5**: refactor completo. PR final mergea con **merge commit** (no squash — preserva revertibilidad por step, según §3).

---

## 9. Tracker de progreso

> Actualizar el checkbox al cerrar cada paso. ⭐ = paso crítico / cambio de v1 por Codex p2.

| Fase | # | Paso | Tipo branch | Estado | Invariantes |
|---|---|---|---|---|---|
| 0 | 0.1 | ARCH_MAP.md | incremental | ☐ | — |
| 0 | 0.2 | INVARIANT_MAP.md | incremental | ☐ | (documenta todas) |
| 0 | 0.3 | property tests filtering (cross_chain + derivation) | incremental | ☐ | #4, #16 |
| 0 | 0.4 | property test reorder_for_swap_and_pop | incremental | ☐ | — |
| 0 | 0.5 | MirrorCase DSL + mirror tests | incremental | ☐ | #18 |
| 0 | 0.6 | trace fixtures | incremental | ☐ | — |
| 0 | 0.7 | hold/rewind tests (incl. withdrawal revert) | incremental | ☐ | #15 |
| 0 | 0.8 | ⭐ capture baseline script | incremental | ☐ | — |
| 1 | 1.1a | RollupId newtype (scaffold) | incremental | ☐ | — |
| 1 | 1.1b | RollupId migration | dedicada | ☐ | — |
| 1 | 1.1c | ScopePath newtype | incremental | ☐ | — |
| 1 | 1.2 | state root newtypes (module-private + boundary) | dedicada | ☐ | #3 |
| 1 | 1.3 | ParentLink enum | dedicada | ☐ | #7 |
| 1 | 1.4 | TxOutcome + EntryGroupMode + EntryClass | dedicada | ☐ | #5 |
| 1 | 1.4b | ⭐ QueuedCallRequest enums (movido desde 1.11, Codex p4) | dedicada | ☐ | #6 |
| 1 | 1.5 | PendingL1SubmissionQueue + BlockEntryMix | incremental | ☐ | #11 |
| 1 | 1.6 | EntryVerificationHold + builder halt gate | incremental | ☐ | #14 |
| 1 | 1.6b+c | ⭐ EntryQueue struct + ForwardPermit token (fusionado, Codex p4) | dedicada | ☐ | #13 |
| 1 | 1.7 | ⭐ FlushPlan typestate (NoEntries/Collected/HoldArmed + SendResult) | dedicada | ☐ | #1 |
| 1 | 1.8 | L1NonceReservation + ProofContext + L1Client wrapper | incremental | ☐ | #2, #22 |
| 1 | 1.9a | ImmediateEntryBuilder + CallOrientation | incremental | ☐ | #19 |
| 1 | 1.9b | DeferredEntryBuilder + RevertGroupBuilder | incremental | ☐ | #8 |
| 1 | 1.9c | L2ToL1ContinuationBuilder (with_scope_return obligatorio) | incremental | ☐ | #12 |
| 1 | 1.10 | ⭐ ReturnData enum (Void / NonVoid) | dedicada | ☐ | #20 |
| 2 | 2.1 | Driver split mecánico (multiple `impl Driver` blocks, sin traits) | dedicada | ☐ | — |
| 2 | 2.2 | BuilderTickContext | incremental | ☐ | — |
| 2 | 2.3 | QueueDrain | incremental | ☐ | — |
| 2 | 2.4 | ProtocolTxPlan con stages tipados | incremental | ☐ | — |
| 2 | 2.5 | VerificationDecision + rewind_to_re_derive | incremental | ☐ | #9, #10 |
| 2 | 2.6 | FlushPrecheck | incremental | ☐ | — |
| 2 | 2.7 | FlushAssembly → FlushPlan<Collected> | incremental | ☐ | — |
| 2 | 2.7b | ⭐ ForwardAndTriggerPlan + TriggerExecutionResult | incremental | ☐ | #15 |
| 2 | 2.8 | step_builder/flush_to_l1 como orquestadores (FlushStage completo) | dedicada | ☐ | — |
| 3 | 3.0 | ⭐ trait SimulationClient + HttpSimClient + InMemorySimClient | dedicada | ☐ | — |
| 3 | 3.1 | Sealed trait Direction | incremental | ☐ | — |
| 3 | 3.2 | composer_rpc/model.rs compartido | dedicada | ☐ | — |
| 3 | 3.3 | rebase_parent_links helper único | incremental | ☐ | #7 |
| 3 | 3.4 | discover_until_stable (spec completa) | dedicada | ☐ | — |
| 3 | 3.5 | build_queue_payload (usa enums de 1.4b) | dedicada | ☐ | — |
| 3 | 3.6 | SimulationPlan enum + simulate_delivery() función | dedicada | ☐ | #17, #21 |
| 3 | 3.7 | direcciones como adapters finos | dedicada | ☐ | — |
| 4 | 4.1 | composer_rpc split | dedicada | ☐ | — |
| 4 | 4.2 | server.rs genérico | incremental | ☐ | — |
| 4 | 4.3 | tx_codec.rs | incremental | ☐ | — |
| 4 | 4.4 | selectors en cross_chain.rs (owner elegido) + CI grep gate | incremental | ☐ | #23 |
| 4 | 4.5 | ⭐ trace split + typed structs (ex-5.2 mergeado) | dedicada | ☐ | — |
| 4 | 4.6 | entry_builder.rs (façade sobre 1.9) | dedicada | ☐ | — |
| 5 | 5.1 | eliminar unwraps residuales | incremental | ☐ | — |
| 5 | 5.4 | proptest / fuzz | incremental | ☐ | — |
| 5 | 5.7 | replay baseline gate vs 0.8 | dedicada | ☐ | bloquea merge a main |

---

## 10. Pasos de ejecución (qué hace este plan inmediatamente después de aprobado)

1. Crear branch `refactor/phase-0-mapping`.
2. Crear `docs/refactor/PLAN.md` con el contenido de este archivo.
3. Crear `docs/refactor/ARCH_MAP.md` (paso 0.1).
4. Crear `docs/refactor/INVARIANT_MAP.md` (paso 0.2, con las 23 filas de §6 expandidas).
5. Commit atómico inicial: `docs(refactor): introduce refactor plan, architecture map, and invariant map`.
6. Reportar al usuario y esperar instrucción explícita para arrancar 0.3 (property tests).

> El plan se aprueba ahora; los pasos de implementación se ejecutan después bajo aprobación explícita del usuario por paso (no por fase — cada paso requiere "go").

## 11. Riesgos y mitigaciones (12 riesgos, expandido por Codex p2)

| Riesgo | Mitigación |
|---|---|
| **Rebase hell** entre ramas dedicadas que tocan `driver.rs`, `l1_to_l2.rs`, `l2_to_l1.rs` | Mergear ramas dedicadas a `main` en orden estricto del plan. No mantener >2 ramas dedicadas abiertas al mismo tiempo. Cada rama rebasea sobre `main` antes de merge — si hay conflicto >1h, PARAR y revisar orden |
| Romper byte-equivalencia de derivación | Baseline capture en 0.8 + replay gate al cierre de cada fase + gate final en 5.7 |
| Una fase queda a medias y deja el código en peor estado | Cada paso es committeable independientemente; halt conditions explícitas; definir en §3 que fase incompleta se rollbackea con `git revert <merge-commit>` en orden inverso |
| Typestate viral que obliga cambios en APIs públicas | Limitar typestate a `FlushPlan`, `EntryVerificationHold`, `ForwardPermit`, `ProtocolTxPlan`, `L1NonceReservation` — todo interno al driver/proposer/composer_rpc, no parte del trait `SyncRollupsApi` público |
| **Dynamic dispatch en `SimulationClient`** (único `dyn Trait` del refactor, paso 3.0) | Negligible: el composer es IO-bound (HTTP), las llamadas son pocas por bloque. Medición opcional con `criterion` (benchmark `throughput` ya existe) al cierre de Fase 3. Si hay regresión >5%, convertir a generic estático |
| **Tentación de agregar traits durante la ejecución**: ejecutando pasos 2.x o 3.x puede aparecer "esto sería más limpio con un trait" | Aplicar la regla de §4b: (1) ¿≥2 impls reales no-mock? (2) ¿La segunda es un backend real? Si cualquiera es "no", usar struct concreto |
| **`EntryQueue` con `Notify` + `BTreeMap`** tiene semántica concurrente delicada: wakeups perdidos, double confirmation, receipts stale | (a) `QueueReceipt` es counter monotonic, no índice positional, así que receipts viejos son detectables. (b) Waiters re-chequean estado tras wake (no asumen que wake = confirmado). (c) Test específico: 1000 push concurrentes + 1000 wait_confirmation desde tasks separados, asertear que ninguno se cuelga ni recibe doble permit. (d) Property test con `loom` (opcional, sólo si emerge un bug real) |
| **Drift entre `Direction::promotion_rule` y `simulation_plan_for`**: la regla de promoción vive en 3.1, la regla de selección de plan en 3.6, pueden divergir | Test cruzado: `promotion_rule` y `simulation_plan_for` se llaman juntos en `discover_until_stable` (3.4); test que verifica que para 6 escenarios canónicos (de mirror_case.rs) la combinación `(promotion, plan)` es correcta. Esto detecta drift en el primer commit que rompe la coherencia |
| **`L1Client` wrapper bypass**: alguien dentro de `proposer.rs` mete un acceso `RootProvider` directo, perdiendo la disciplina | Lint local: `clippy::disallowed_methods` con regla de que `RootProvider::*` sólo se puede llamar dentro de `impl L1Client { ... }`. Verificable con `cargo clippy --workspace --all-features -- -D warnings` (ya en cierre de fase) |
| **Plan text staleness**: durante el loop de revisiones, queda texto stale como `SimulationStrategy`, `SimClient`, `QueueClient` que nunca existen en código | Convención: cada iteración del loop hace `grep -n` por nombres descartados (`SimulationStrategy`, `BuilderPhase`, `EntryQueue trait`, `L1Provider trait`) en el plan y verifica 0 matches antes de cerrar la iteración |
| Sobre-abstracción prematura del trait Direction | Orden del plan respeta: guardrails → tipos → pipelines → recién Direction (MVP path en §7) |
| **Inconsistencia revert vs squash**: §3 pide `git revert` por step pero v1 cerraba con squash | Corregido en v2: **merge commit obligatorio** en el cierre de cada fase. Squash prohibido excepto para pasos claramente atómicos tipo 0.1/0.2 |
| **Flakiness por timing**: hold-then-forward, receipt polling, `block.timestamp` en `publicInputsHash`, sequencing `postBatch/createProxy/trigger` | (a) E2E con retries sobre convergencia, no sobre síntomas puntuales; (b) `capture_baseline.sh` corre cada escenario 3 veces y verifica determinismo antes de guardar; (c) no tocar proposer timing en Fase 1/2 |
| **Estado stale del devnet** bloquea recovery entre pasos | Reset devnet (`docker compose down -v`) **sólo con aprobación explícita del usuario**, idealmente al cierre de cada fase. Cambios en entries/postBatch formats requieren reset previo |
| **ECDSA prover de desarrollo**: cambios en timestamp prediction o proof context pueden romper E2E intermitente | Paso 1.8 (`ProofContext`) centraliza el cálculo; cualquier cambio de predicción de timestamp se audita contra `scripts/refactor/capture_baseline.sh`. No tocar `sign_proof` fuera del 1.8 |
| **Drift de fixtures JSON de traza** si cambia el formato del tracer/reth | Versión pinada de reth en Docker (`fix/pin-reth-v1.11.3` branch actual). Fixtures regenerados con un comando específico (`scripts/refactor/regen_trace_fixtures.sh`) cuando reth cambia |
| **Falsa confianza por `nextest` filtrado**: los filtros del plan no cubren paths integrados `driver ↔ rpc ↔ composer` | Al cierre de cada fase se corre suite completa `cargo nextest run --workspace` + E2E — no sólo los filtros por paso. Los filtros son para iteración rápida |
| Refactor consume contexto del usuario / agente | Cada paso <1 día de trabajo. PRs chicos. Mergear continuamente. Cada fase tiene ≤12 pasos numerados, máximo |

## 12. Definition of Done (realista, corregido por Codex p2)

**Alcance del refactor** (lo que este plan ataca):
- ✅ Ninguna función en `driver/` >200 LOC
- ✅ `composer_rpc/l1_to_l2.rs` + subdirectorio `l1_to_l2/` ≤ 2000 LOC total (desde 4345) — **reducción ≥50%**
- ✅ `composer_rpc/l2_to_l1.rs` + subdirectorio `l2_to_l1/` ≤ 2500 LOC total (desde 5459) — **reducción ≥50%**
- ✅ Las 23 invariantes de §6 codificadas con uno de estos dos criterios (corrección Codex p4: no todas son compile-time):
  - **Compile-time**: la violación produce error del compilador. Aplica a: #1 (FlushPlan typestate), #2 (NonceResetRequired), #3 (CleanStateRoot constructors), #5 (EntryClass enum), #6 (QueuedCallRequest variants), #7 (ParentLink), #11 (BlockEntryMix), #12 (with_scope_return obligatorio), #13 (ForwardPermit), #14 (is_blocking_build gate), #19 (CallOrientation), #20 (ReturnData), #21 (PromotionDecision), #22 (ProofContext).
  - **Test o gate de CI**: la violación produce falla de test/lint. Aplica a: #4 (property test prefix monotonic), #8 (test "first trigger needs clean root"), #9 + #10 (test "deferral exhaustion rewinds"), #15 (test "withdrawal trigger revert rewinds"), #16 (test "filter es generic, no Bridge selectors"), #17 (test "single-call sim no se elige para multi-call"), #18 (mirror tests con MirrorCase DSL), #23 (CI grep gate anti-selectors).
- ✅ Para cada invariante de §6, **al menos uno de los dos criterios pasa verde**.
- ✅ 0 `unwrap()` / `expect()` en producción (`clippy::unwrap_used` deny)
- ✅ `cargo nextest run --workspace` verde
- ✅ `cargo clippy --workspace --all-features -- -D warnings` verde
- ✅ Suite E2E completa (`scripts/e2e/`) verde sobre devnet-eez
- ✅ `scripts/refactor/replay_baseline.sh` 0 diffs contra baseline de 0.8
- ✅ `docs/refactor/INVARIANT_MAP.md` con todas las filas en columna "tipo futuro" = ✓
- ✅ CI grep gate anti-selectors-hardcoded verde (4.4)
- ✅ **Trait `SimulationClient`** con ≥2 impls (`HttpSimClient` + `InMemorySimClient` con fixtures). Tests de `composer_rpc` corren con `InMemorySimClient` sin necesidad de reth upstream.
- ✅ **Trait `Direction`** (sealed) con ≥2 impls (`L1ToL2`, `L2ToL1`) y los archivos `l1_to_l2.rs`/`l2_to_l1.rs` reducidos a adapters finos (ver criterios de LOC más arriba).
- ✅ **Traits descartados documentados**: §4b lista explícitamente los traits considerados y rechazados (`L1Provider` como trait ancho, `EntryQueue` trait, capability traits, `SimulationStrategy` trait + `OrElse`). La regla operativa de §4b está respetada en todo el código del refactor.

**Fuera de alcance** (lo que este plan NO promete — requiere Fase 6 explícita):
- ❌ Partir `cross_chain.rs` (2410 LOC actuales)
- ❌ Partir `table_builder.rs` (2524 LOC actuales)
- ❌ Funciones grandes en esos dos archivos (`build_cross_chain_call_entries`, `build_l2_to_l1_call_entries`, `build_continuation_entries`, `build_l2_to_l1_continuation_entries`, `reconstruct_continuation_l2_entries`, `attach_chained_state_deltas`)
- ❌ Eliminación del ECDSA prover de desarrollo
- ❌ Partir `driver.rs` si termina >2000 LOC por archivo después de 2.1 (debería quedar por debajo, pero no es criterio de DoD)
- ❌ Refactor de `rpc.rs` más allá de lo necesario para 1.4b

Si después del refactor querés atacar lo "fuera de alcance", abrimos una Fase 6 con su propio plan. Eso no es este trabajo.
