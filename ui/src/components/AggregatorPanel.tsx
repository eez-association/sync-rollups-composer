import { useState } from "react";
import { config } from "../config";
import { TxLink } from "./TxLink";
import { ExplorerLink } from "./ExplorerLink";
import { CrossChainFlowViz } from "./CrossChainFlowViz";
import styles from "./AggregatorPanel.module.css";

/* ── Types (defined locally — useAggregator hook is built by another agent) ── */

type AggregatorPhase =
  | "idle"
  | "wrapping"
  | "approving"
  | "sending"
  | "processing"
  | "verifying"
  | "complete"
  | "failed";

interface AggregatorState {
  phase: AggregatorPhase;
  l1ReserveA: string | null;
  l1ReserveB: string | null;
  l2ReserveA: string | null;
  l2ReserveB: string | null;
  l1Quote: string | null;
  l2Quote: string | null;
  singlePoolQuote: string | null;
  ethBalance: string | null;
  wethBalance: string | null;
  usdcBalance: string | null;
  totalAmount: string;
  splitPercent: number;
  txHash: string | null;
  l1TxStatus: number | null;
  l1BlockNumber: number | null;
  l1GasUsed: string | null;
  l1Done: boolean;
  l2Done: boolean;
  localOutput: string | null;
  remoteOutput: string | null;
  totalOutput: string | null;
  improvement: string | null;
  usdcBalanceBefore: string | null;
  usdcBalanceAfter: string | null;
  vizPhase: number;
  startTime: number | null;
  endTime: number | null;
  error: string | null;
  contractsDeployed: boolean;
  loading: boolean;
}

interface AggregatorPanelProps {
  state: AggregatorState;
  onExecute: () => void;
  onWrapEth: (amount: string) => void;
  onUnwrapWeth: (amount: string) => void;
  onReset: () => void;
  onSetSplit: (percent: number) => void;
  onSetAmount: (amount: string) => void;
  walletConnected: boolean;
  walletAddress: string | null;
}

/* ── Icons ── */

function IconCheck({ size = 16 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <polyline points="20 6 9 17 4 12" />
    </svg>
  );
}

function IconX({ size = 16 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <line x1="18" y1="6" x2="6" y2="18" />
      <line x1="6" y1="6" x2="18" y2="18" />
    </svg>
  );
}

function IconSplit({ size = 18 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <path d="M16 3h5v5" />
      <path d="M8 3H3v5" />
      <path d="M12 22v-8.3a4 4 0 0 0-1.172-2.828L3 3" />
      <path d="m15 9 6-6" />
    </svg>
  );
}

function IconInfo({ size = 13 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <circle cx="12" cy="12" r="10" />
      <line x1="12" y1="16" x2="12" y2="12" />
      <line x1="12" y1="8" x2="12.01" y2="8" />
    </svg>
  );
}

/* ── Spinner ── */
function Spinner() {
  return <span className={styles.spinner} aria-label="Loading" />;
}

/* ── Step status types ── */
type StepStatus = "waiting" | "active" | "done" | "failed";

interface ExecutionStep {
  id: string;
  label: string;
  detail: React.ReactNode;
  status: StepStatus;
}

/* ── Helpers ── */
function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const s = (ms / 1000).toFixed(1);
  return `${s}s`;
}

/* ── Contract data with descriptions ── */
interface ContractEntry {
  name: string;
  desc: string;
  value: string;
  net: "L1" | "L2";
}

/* ── Info icon with tooltip ── */
function InfoTooltip({ text }: { text: string }) {
  const [show, setShow] = useState(false);
  return (
    <span
      className={styles.infoTip}
      onMouseEnter={() => setShow(true)}
      onMouseLeave={() => setShow(false)}
      onFocus={() => setShow(true)}
      onBlur={() => setShow(false)}
      tabIndex={0}
      role="button"
      aria-label="More info"
    >
      <IconInfo />
      {show && <span className={styles.infoTipPopup}>{text}</span>}
    </span>
  );
}

/* ── Contract chip ── */
function ContractChip({ c }: { c: ContractEntry }) {
  const present = !!c.value && c.value !== "0x" + "0".repeat(40);
  return (
    <div className={styles.chip} data-net={c.net}>
      <span className={styles.chipDot} data-ok={present ? "true" : "false"} />
      <span className={styles.chipName}>
        {present ? (
          <ExplorerLink value={c.value} type="address" chain={c.net === "L1" ? "l1" : "l2"} label={c.name} />
        ) : (
          <span className={styles.chipNameDim}>{c.name}</span>
        )}
      </span>
      <InfoTooltip text={c.desc} />
    </div>
  );
}

/* ── Contract Lanes ── */
function ContractLanes({
  loading,
  deployed,
  l1Contracts,
  l2Contracts,
}: {
  loading: boolean;
  deployed: boolean;
  l1Contracts: ContractEntry[];
  l2Contracts: ContractEntry[];
}) {
  return (
    <div className={styles.contractsCard}>
      <div className={styles.contractsHeader}>
        <span className={styles.contractsHeaderLeft}>
          {loading ? (
            <Spinner />
          ) : deployed ? (
            <span className={styles.contractsDot} data-status="ok" />
          ) : (
            <span className={styles.contractsDot} data-status="error" />
          )}
          <span className={styles.contractsTitle}>Contracts</span>
        </span>
        {!loading && (
          <span className={styles.statusBadge} data-status={deployed ? "ok" : "error"}>
            {deployed ? <><IconCheck size={10} /> Ready</> : <><IconX size={10} /> Missing</>}
          </span>
        )}
      </div>

      <div className={styles.lanes}>
        <div className={styles.lane} data-net="L1">
          <div className={styles.laneHeader}>
            <span className={styles.laneDot} data-net="L1" />
            <span className={styles.laneLabel}>L1 Ethereum</span>
          </div>
          <div className={styles.laneChips}>
            {l1Contracts.map((c) => <ContractChip key={c.name + c.net} c={c} />)}
          </div>
        </div>

        <div className={styles.laneConnector}>
          <div className={styles.laneConnectorLine} />
          <span className={styles.laneConnectorLabel}>Bridge</span>
          <div className={styles.laneConnectorLine} />
        </div>

        <div className={styles.lane} data-net="L2">
          <div className={styles.laneHeader}>
            <span className={styles.laneDot} data-net="L2" />
            <span className={styles.laneLabel}>L2 Rollup</span>
          </div>
          <div className={styles.laneChips}>
            {l2Contracts.map((c) => <ContractChip key={c.name + c.net} c={c} />)}
          </div>
        </div>
      </div>
    </div>
  );
}

/* ── Step Tracker ── */
function StepTracker({ steps }: { steps: ExecutionStep[] }) {
  return (
    <div className={styles.stepTracker}>
      {steps.map((step, i) => (
        <div key={step.id} className={styles.stepRow} data-status={step.status}>
          <div className={styles.stepLeft}>
            <div className={styles.stepCircle} data-status={step.status}>
              {step.status === "done" ? (
                <IconCheck size={12} />
              ) : step.status === "failed" ? (
                <IconX size={12} />
              ) : step.status === "active" ? (
                <Spinner />
              ) : (
                <span className={styles.stepNumber}>{i + 1}</span>
              )}
            </div>
            {i < steps.length - 1 && (
              <div className={styles.stepLine} data-status={step.status} />
            )}
          </div>
          <div className={styles.stepBody}>
            <div className={styles.stepLabel}>{step.label}</div>
            {step.detail && (
              <div className={styles.stepDetail}>{step.detail}</div>
            )}
          </div>
        </div>
      ))}
    </div>
  );
}

/* ── Build execution steps from state ── */
function buildSteps(state: AggregatorState): ExecutionStep[] {
  const phase = state.phase;
  const ORDER: AggregatorPhase[] = ["idle", "wrapping", "approving", "sending", "processing", "verifying", "complete", "failed"];
  const phaseIdx = (p: AggregatorPhase) => ORDER.indexOf(p);
  const currentIdx = phaseIdx(phase);

  const sendStatus: StepStatus = (() => {
    if (phase === "wrapping" || phase === "approving" || phase === "sending") return "active";
    if (phase === "failed" && !state.txHash) return "failed";
    if (state.txHash) return "done";
    return "waiting";
  })();

  const processStatus: StepStatus = (() => {
    if (state.l1Done && state.l2Done) return "done";
    if (phase === "processing") return "active";
    if (phase === "failed" && state.txHash && !(state.l1Done && state.l2Done)) return "failed";
    if (phaseIdx(phase) > phaseIdx("processing")) return "done";
    return "waiting";
  })();

  const verifyStatus: StepStatus = (() => {
    if (phase === "complete") return "done";
    if (phase === "verifying") return "active";
    if (phase === "failed" && currentIdx >= phaseIdx("verifying")) return "failed";
    if (currentIdx > phaseIdx("verifying")) return "done";
    return "waiting";
  })();

  const completeStatus: StepStatus = phase === "complete" ? "done" : "waiting";

  return [
    {
      id: "send",
      label: "Send via L1 Proxy",
      detail: state.txHash ? (
        <span>Tx: <TxLink hash={state.txHash} chain="l1" short /></span>
      ) : phase === "wrapping" ? (
        <span>Wrapping ETH to WETH...</span>
      ) : phase === "approving" ? (
        <span>Approving WETH spend...</span>
      ) : phase === "sending" ? (
        <span>Waiting for wallet signature...</span>
      ) : null,
      status: sendStatus,
    },
    {
      id: "process",
      label: "Cross-chain processing",
      detail: (() => {
        if (state.l1Done && state.l2Done) {
          return <span>L1 local swap + L2 remote swap complete</span>;
        }
        if (phase === "processing") {
          return <span>L1 AMM executing local portion, L2 AMM executing remote portion...</span>;
        }
        return null;
      })(),
      status: processStatus,
    },
    {
      id: "verify",
      label: "Verify results",
      detail: phase === "verifying" ? <span>Checking output balances and state roots...</span> : null,
      status: verifyStatus,
    },
    {
      id: "complete",
      label: "Complete",
      detail: state.startTime && state.endTime ? (
        <span>Execution time: {formatDuration(state.endTime - state.startTime)}</span>
      ) : null,
      status: completeStatus,
    },
  ];
}

/* ── Results Card ── */
function ResultsCard({ state }: { state: AggregatorState }) {
  if (state.phase !== "complete") return null;

  const duration = state.startTime && state.endTime ? formatDuration(state.endTime - state.startTime) : null;

  return (
    <div className={styles.resultsCard}>
      <div className={styles.resultsTitle}>Aggregation Results</div>
      <div className={styles.resultsGrid}>
        {state.localOutput !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>L1 AMM Output</span>
            <span className={styles.resultValue}>{state.localOutput} USDC</span>
          </div>
        )}
        {state.remoteOutput !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>L2 AMM Output</span>
            <span className={styles.resultValue}>{state.remoteOutput} USDC</span>
          </div>
        )}
        {state.totalOutput !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>Total USDC</span>
            <span className={`${styles.resultValue} ${styles.resultHighlight}`}>{state.totalOutput} USDC</span>
          </div>
        )}
        {state.improvement !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>Improvement</span>
            <span className={styles.resultValue}>
              <span className={styles.quoteImprovement}>+{state.improvement}%</span>
              <span style={{ marginLeft: 4, fontSize: 10, color: "var(--text-dim)" }}>vs single pool</span>
            </span>
          </div>
        )}
        {state.l1GasUsed && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>Gas Used</span>
            <span className={styles.resultValue}>{state.l1GasUsed} gas</span>
          </div>
        )}
        {state.l1BlockNumber !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>L1 Block</span>
            <span className={styles.resultValue}>
              <ExplorerLink value={state.l1BlockNumber.toString()} type="block" chain="l1" label={`#${state.l1BlockNumber}`} />
            </span>
          </div>
        )}
        {state.txHash && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>L1 Transaction</span>
            <span className={styles.resultValue}>
              <TxLink hash={state.txHash} chain="l1" short={false} />
            </span>
          </div>
        )}
        {duration && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>Total Time</span>
            <span className={styles.resultValue}>{duration}</span>
          </div>
        )}
        {state.l1BlockNumber !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>Inspect</span>
            <span className={styles.resultValue}>
              <a href={`#/visualizer?block=${state.l1BlockNumber}`} className={styles.vizLink}>
                Open in Visualizer
                <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/><polyline points="15 3 21 3 21 9"/><line x1="10" y1="14" x2="21" y2="3"/></svg>
              </a>
            </span>
          </div>
        )}
      </div>
    </div>
  );
}

/* ── Under the Hood — visual explainer ── */
function UnderTheHood() {
  const hops: Array<{
    num: number;
    label: string;
    chain: "L1" | "L2" | "L1→L2" | "L2→L1";
    title: string;
    body: React.ReactNode;
  }> = [
    {
      num: 1,
      label: "Local Swap",
      chain: "L1",
      title: "Aggregator splits your WETH",
      body: (
        <>
          Your wallet sends <strong>WETH</strong> to the <strong>Aggregator</strong> contract on L1.
          It forwards the <em>local</em> portion to the L1 AMM for an immediate WETH→USDC swap,
          and approves the <em>remote</em> portion for the Bridge.
        </>
      ),
    },
    {
      num: 2,
      label: "Bridge L1→L2",
      chain: "L1→L2",
      title: "Cross-chain call to L2",
      body: (
        <>
          The Aggregator calls <code>Bridge.bridgeTokens()</code> and then invokes the
          <code> L2Executor</code> proxy with <code>swapAndBridgeBack()</code>. The L1 composer
          RPC traces the transaction, queues <strong>cross-chain entries</strong> on the L2 execution
          table, and includes them in the next L2 block — all in the same atomic tx.
        </>
      ),
    },
    {
      num: 3,
      label: "Remote Swap",
      chain: "L2",
      title: "L2 Executor runs the swap",
      body: (
        <>
          On L2, the <strong>L2Executor</strong> receives <strong>wWETH</strong>, approves
          the <strong>L2 AMM</strong>, and executes a wWETH→wUSDC swap. The L2 AMM is a
          separate liquidity pool with its own price, giving you access to <em>two markets at once</em>.
        </>
      ),
    },
    {
      num: 4,
      label: "Bridge L2→L1",
      chain: "L2→L1",
      title: "Bounce-back to L1",
      body: (
        <>
          The L2Executor immediately calls <code>Bridge.bridgeTokens()</code> to send the
          <strong> wUSDC</strong> back to L1. This is a <strong>scope-navigation return call</strong> —
          part of the same atomic cross-chain scope, depth 7 in the call trace. The Aggregator
          on L1 receives the wrapped USDC as <strong>real USDC</strong> and forwards the combined
          output to your wallet.
        </>
      ),
    },
  ];

  const facts: Array<{ value: string; label: string }> = [
    { value: "1", label: "atomic tx" },
    { value: "3", label: "cross-chain hops" },
    { value: "7", label: "call depth" },
    { value: "2", label: "AMM pools" },
    { value: "0", label: "trust assumptions" },
  ];

  return (
    <div className={styles.underHood}>
      <div className={styles.underHoodHeader}>
        <span className={styles.underHoodIcon}>
          <IconSplit size={16} />
        </span>
        <span className={styles.underHoodTitle}>Under the Hood</span>
        <span className={styles.underHoodSubtitle}>
          How a single transaction orchestrates a cross-chain swap
        </span>
      </div>

      <div className={styles.underHoodBody}>
        <p className={styles.underHoodIntro}>
          Cross-chain DeFi composability usually means <strong>multiple transactions</strong>,
          bridging delays, and partial execution risk. The <strong>sync rollup protocol</strong>
          collapses the whole flow into a <strong>single atomic L1 transaction</strong> —
          either everything succeeds, or nothing happens.
        </p>

        <div className={styles.hopGrid}>
          {hops.map((hop) => (
            <div key={hop.num} className={styles.hopCard} data-chain={hop.chain}>
              <div className={styles.hopHeader}>
                <span className={styles.hopNum}>{hop.num}</span>
                <span className={styles.hopChainBadge} data-chain={hop.chain}>
                  {hop.chain}
                </span>
                <span className={styles.hopLabel}>{hop.label}</span>
              </div>
              <div className={styles.hopTitle}>{hop.title}</div>
              <div className={styles.hopBody}>{hop.body}</div>
            </div>
          ))}
        </div>

        <div className={styles.factsRow}>
          {facts.map((f) => (
            <div key={f.label} className={styles.factItem}>
              <div className={styles.factValue}>{f.value}</div>
              <div className={styles.factLabel}>{f.label}</div>
            </div>
          ))}
        </div>

        <p className={styles.underHoodFooter}>
          If any hop reverts, the whole scope unwinds — no leftover state on L1 or L2,
          no tokens stranded in a bridge. That is what makes this pattern impossible on
          traditional cross-chain messaging stacks.
        </p>
      </div>
    </div>
  );
}

/* ── Main AggregatorPanel component ── */
export function AggregatorPanel({
  state,
  onExecute,
  onWrapEth,
  onUnwrapWeth,
  onReset,
  onSetSplit,
  onSetAmount,
  walletConnected,
  walletAddress: _walletAddress,
}: AggregatorPanelProps) {
  const [wrapAmount, setWrapAmount] = useState("0.1");
  const [unwrapAmount, setUnwrapAmount] = useState("0.1");
  const [wrapOpen, setWrapOpen] = useState(false);

  const busy =
    state.phase !== "idle" &&
    state.phase !== "complete" &&
    state.phase !== "failed";
  const complete = state.phase === "complete";
  const failed = state.phase === "failed";
  const showSteps = state.phase !== "idle";

  const l1Contracts: ContractEntry[] = [
    { name: "WETH", desc: "Wrapped Ether on L1. The input token for the aggregated swap.", value: config.aggWeth, net: "L1" },
    { name: "USDC", desc: "USD Coin on L1. The output token received after aggregation.", value: config.aggUsdc, net: "L1" },
    { name: "Aggregator", desc: "Splits the input across L1 and L2 AMMs, manages cross-chain bridging, and recombines the output.", value: config.aggAggregator, net: "L1" },
    { name: "L1 AMM", desc: "Automated market maker on L1. Handles the local portion of the WETH-to-USDC swap.", value: config.aggL1Amm, net: "L1" },
  ];

  const l2Contracts: ContractEntry[] = [
    { name: "L2Executor", desc: "Receives bridged WETH on L2, swaps through L2 AMM, and bridges USDC back to L1.", value: config.aggL2Executor, net: "L2" },
    { name: "L2 AMM", desc: "Automated market maker on L2. Handles the remote portion of the WETH-to-USDC swap.", value: config.aggL2Amm, net: "L2" },
    { name: "wWETH", desc: "Wrapped WETH on L2. Created by the bridge when WETH is bridged from L1.", value: config.aggWrappedWethL2, net: "L2" },
    { name: "wUSDC", desc: "Wrapped USDC on L2. Created by the L2 AMM swap, bridged back to L1 as real USDC.", value: config.aggWrappedUsdcL2, net: "L2" },
  ];

  const steps = buildSteps(state);

  return (
    <div className={styles.panel}>
      {/* Combined card: header strip + visualization + swap form */}
      <div className={styles.mainCard}>
        {/* Compact header strip: badge + title + stats + balances */}
        <div className={styles.mainHeader}>
          <div className={styles.mainHeaderLeft}>
            <span className={styles.heroBadge}>
              <IconSplit size={11} />
              Cross-Chain Aggregator
            </span>
            <span className={styles.mainHeaderTitle}>Split. Swap. Atomic.</span>
            <span className={styles.mainHeaderStats}>
              <span>3 hops</span><span className={styles.dot}>·</span>
              <span>2 AMMs</span><span className={styles.dot}>·</span>
              <span>depth 7</span><span className={styles.dot}>·</span>
              <span>1 tx</span>
            </span>
          </div>
          <div className={styles.mainHeaderRight}>
            <span className={styles.headerBalance}>
              <span className={styles.headerBalanceLabel}>ETH</span>
              <span className={styles.headerBalanceValue}>
                {state.ethBalance !== null ? parseFloat(state.ethBalance).toFixed(2) : "—"}
              </span>
            </span>
            <span className={styles.headerBalance}>
              <span className={styles.headerBalanceLabel}>WETH</span>
              <span className={styles.headerBalanceValue}>
                {state.wethBalance !== null ? parseFloat(state.wethBalance).toFixed(2) : "—"}
              </span>
            </span>
            <span className={styles.headerBalance}>
              <span className={styles.headerBalanceLabel}>USDC</span>
              <span className={styles.headerBalanceValue}>
                {state.usdcBalance !== null ? parseFloat(state.usdcBalance).toFixed(2) : "—"}
              </span>
            </span>
            <button
              className={styles.wrapToggleBtn}
              onClick={() => setWrapOpen(!wrapOpen)}
              title="Wrap ETH ↔ WETH"
              aria-expanded={wrapOpen}
            >
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                <path d="M7 16V4M7 4L3 8M7 4l4 4M17 8v12M17 20l4-4M17 20l-4-4" />
              </svg>
              Wrap ETH ↔ WETH
            </button>
          </div>
        </div>

        {/* SVG visualization */}
        <div className={styles.vizInner}>
          <CrossChainFlowViz
            vizPhase={state.vizPhase}
            splitPercent={state.splitPercent}
            l1ReserveA={state.l1ReserveA}
            l1ReserveB={state.l1ReserveB}
            l2ReserveA={state.l2ReserveA}
            l2ReserveB={state.l2ReserveB}
            improvement={state.improvement}
          />
        </div>

        {/* Swap form — same card, separated by a thin divider */}
        <div className={styles.swapInline}>

        {/* Wrap/Unwrap drawer (collapsed by default) */}
        {wrapOpen && (
          <div className={styles.wrapDrawer}>
            <div className={styles.wrapField}>
              <input
                type="text"
                className={styles.wrapInput}
                value={wrapAmount}
                onChange={(e) => setWrapAmount(e.target.value)}
                placeholder="0.1"
              />
              <span className={styles.wrapFieldLabel}>ETH</span>
              <button className={styles.wrapBtn} onClick={() => onWrapEth(wrapAmount)}>
                Wrap → WETH
              </button>
            </div>
            <div className={styles.wrapField}>
              <input
                type="text"
                className={styles.wrapInput}
                value={unwrapAmount}
                onChange={(e) => setUnwrapAmount(e.target.value)}
                placeholder="0.1"
              />
              <span className={styles.wrapFieldLabel}>WETH</span>
              <button className={styles.wrapBtn} onClick={() => onUnwrapWeth(unwrapAmount)}>
                Unwrap → ETH
              </button>
            </div>
          </div>
        )}

        {/* FROM box (Uniswap-style) */}
        <div className={styles.swapBox}>
          <div className={styles.swapBoxHeader}>
            <span className={styles.swapBoxLabel}>You pay</span>
            <span className={styles.swapBoxBalance}>
              Balance: {state.wethBalance !== null ? parseFloat(state.wethBalance).toFixed(4) : "—"}
              {state.wethBalance !== null && parseFloat(state.wethBalance) > 0 && (
                <button
                  className={styles.maxBtn}
                  onClick={() => onSetAmount(state.wethBalance!)}
                >
                  MAX
                </button>
              )}
            </span>
          </div>
          <div className={styles.swapBoxRow}>
            <input
              type="text"
              className={styles.swapAmountInput}
              value={state.totalAmount}
              onChange={(e) => onSetAmount(e.target.value)}
              placeholder="0.0"
            />
            <span className={styles.swapTokenChip}>
              <span className={styles.swapTokenDot} data-token="weth" />
              WETH
            </span>
          </div>
        </div>

        {/* Split slider — compact */}
        <div className={styles.splitGroup}>
          <div className={styles.splitLabels}>
            <span className={styles.splitLabel} data-net="L1">
              L1 {state.splitPercent}%
            </span>
            <span className={styles.splitLabelMid}>Route split</span>
            <span className={styles.splitLabel} data-net="L2">
              L2 {100 - state.splitPercent}%
            </span>
          </div>
          <input
            type="range"
            min={0}
            max={100}
            value={state.splitPercent}
            onChange={(e) => onSetSplit(parseInt(e.target.value, 10))}
            className={styles.splitSlider}
          />
        </div>

        {/* TO box — receive */}
        <div className={styles.swapBox} data-receive="true">
          <div className={styles.swapBoxHeader}>
            <span className={styles.swapBoxLabel}>You receive</span>
            <span className={styles.swapBoxRoute}>
              {state.l1Quote !== null && (
                <span className={styles.routeChip} data-net="L1">
                  L1 {parseFloat(state.l1Quote).toFixed(2)}
                </span>
              )}
              {state.l2Quote !== null && (
                <span className={styles.routeChip} data-net="L2">
                  L2 {parseFloat(state.l2Quote).toFixed(2)}
                </span>
              )}
            </span>
          </div>
          <div className={styles.swapBoxRow}>
            <span className={styles.swapAmountOutput}>
              {state.l1Quote !== null && state.l2Quote !== null
                ? (parseFloat(state.l1Quote) + parseFloat(state.l2Quote)).toFixed(4)
                : "—"}
            </span>
            <span className={styles.swapTokenChip}>
              <span className={styles.swapTokenDot} data-token="usdc" />
              USDC
            </span>
          </div>
          {state.singlePoolQuote !== null && state.improvement !== null && (
            <div className={styles.improvementRow}>
              vs single pool ({parseFloat(state.singlePoolQuote).toFixed(2)} USDC):
              <span className={styles.quoteImprovement}>+{state.improvement}%</span>
            </div>
          )}
        </div>
        {/* Execute button */}
        <button
          className={styles.executeBtn}
          onClick={onExecute}
          disabled={busy || !walletConnected || !state.contractsDeployed}
        >
          {busy ? (
            <><Spinner /> Executing Aggregated Swap...</>
          ) : (
            <><IconSplit size={15} /> Aggregate Swap</>
          )}
        </button>
        </div>
      </div>

      {/* Contract Lanes (deployed contracts reference — less important) */}
      <ContractLanes
        loading={state.loading}
        deployed={state.contractsDeployed}
        l1Contracts={l1Contracts}
        l2Contracts={l2Contracts}
      />

      {/* Step tracker */}
      {showSteps && (
        <div className={styles.trackerCard}>
          <div className={styles.trackerTitle}>Execution Progress</div>
          <StepTracker steps={steps} />
          {(complete || failed) && (
            <button className={styles.resetBtn} onClick={onReset}>
              Reset
            </button>
          )}
        </div>
      )}

      {/* Error banner */}
      {state.error && state.phase === "failed" && (
        <div className={styles.errorBanner}>{state.error}</div>
      )}

      {/* Results card */}
      <ResultsCard state={state} />

      {/* Under the Hood */}
      <UnderTheHood />
    </div>
  );
}
