import { useState } from "react";
import { FlashLoanState, FlashLoanPhase } from "../hooks/useFlashLoan";
import { ReverseFlashLoanState, ReverseFlashLoanPhase } from "../hooks/useFlashLoanReverse";
import { DeployState } from "../hooks/useFlashLoanDeploy";
import { TxLink } from "./TxLink";
import { ExplorerLink } from "./ExplorerLink";
import styles from "./FlashLoanPanel.module.css";

/* ── Icons ──────────────────────────────────────────────────────────────── */

function IconLightning({ size = 18 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2" />
    </svg>
  );
}

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

function IconArrowRight({ size = 16 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <line x1="5" y1="12" x2="19" y2="12" />
      <polyline points="12 5 19 12 12 19" />
    </svg>
  );
}

function IconSwap({ size = 16 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <path d="M7 16l-4-4 4-4" /><path d="M17 8l4 4-4 4" />
      <path d="M3 12h18" />
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

function shortHash(hash: string): string {
  return `${hash.slice(0, 10)}...${hash.slice(-6)}`;
}

function shortAddr(addr: string): string {
  return `${addr.slice(0, 8)}...${addr.slice(-6)}`;
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
      <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
        <circle cx="12" cy="12" r="10" />
        <line x1="12" y1="16" x2="12" y2="12" />
        <line x1="12" y1="8" x2="12.01" y2="8" />
      </svg>
      {show && <span className={styles.infoTipPopup}>{text}</span>}
    </span>
  );
}

/* ── Contract chip — visualizer style ── */
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

/* ── Direction Toggle ── */
type FlashLoanDirection = "l1-to-l2" | "l2-to-l1";

function DirectionToggle({
  direction,
  onSwap,
}: {
  direction: FlashLoanDirection;
  onSwap: () => void;
}) {
  const isL1toL2 = direction === "l1-to-l2";
  return (
    <div className={styles.directionToggle}>
      <div className={styles.directionCard} data-active={isL1toL2 ? "true" : "false"} data-net={isL1toL2 ? "L1" : "L2"}>
        <div className={styles.directionDot} data-net={isL1toL2 ? "L1" : "L2"} />
        <div className={styles.directionInfo}>
          <div className={styles.directionChain}>{isL1toL2 ? "L1 Ethereum" : "L2 Rollup"}</div>
          <div className={styles.directionRole}>Source</div>
        </div>
      </div>

      <button
        className={styles.swapBtn}
        onClick={onSwap}
        title="Swap direction"
        aria-label="Swap flash loan direction"
      >
        <IconSwap size={15} />
      </button>

      <div className={styles.directionCard} data-active={!isL1toL2 ? "true" : "false"} data-net={isL1toL2 ? "L2" : "L1"} style={{ textAlign: "right" }}>
        <div className={styles.directionInfo}>
          <div className={styles.directionChain}>{isL1toL2 ? "L2 Rollup" : "L1 Ethereum"}</div>
          <div className={styles.directionRole}>Destination</div>
        </div>
        <div className={styles.directionDot} data-net={isL1toL2 ? "L2" : "L1"} />
      </div>
    </div>
  );
}

/* ── Flow Diagram — animated cross-chain path ── */
function FlowDiagram({ direction, phase }: { direction: FlashLoanDirection; phase: string }) {
  const isL1toL2 = direction === "l1-to-l2";
  const active = phase !== "idle" && phase !== "failed";
  const done = phase === "complete";

  const steps = isL1toL2 ? [
    { label: "execute()", chain: "L1", desc: "FlashLoanBridgeExecutor" },
    { label: "L1 Proxy", chain: "Proxy", desc: "Detect cross-chain calls" },
    { label: "Composer", chain: "Builder", desc: "Build L2 block with entries" },
    { label: "NFT Claimed", chain: "L2", desc: "receiveTokens + claimAndBridgeBack" },
    { label: "Loan Repaid", chain: "L1", desc: "FlashPool.onFlashLoan callback" },
  ] : [
    { label: "execute()", chain: "L2", desc: "ReverseExecutorL2" },
    { label: "L2 Composer", chain: "Proxy", desc: "Detect L2→L1 cross-chain call" },
    { label: "Composer", chain: "Builder", desc: "PostBatch with L2→L1 entries" },
    { label: "NFT Claimed", chain: "L1", desc: "executeIncomingCrossChainCall" },
    { label: "Bridge Back", chain: "L2", desc: "Tokens return to L2 pool" },
  ];

  return (
    <div className={`${styles.flowDiagram} ${active ? styles.flowActive : ""} ${done ? styles.flowDone : ""}`}>
      {steps.map((step, i) => (
        <div key={i} className={styles.flowStep} style={{ animationDelay: `${i * 0.1}s` }}>
          <div className={styles.flowNode} data-chain={step.chain.toLowerCase()}>
            <div className={styles.flowNodeBadge}>{step.chain}</div>
            <div className={styles.flowNodeLabel}>{step.label}</div>
            <div className={styles.flowNodeDesc}>{step.desc}</div>
          </div>
          {i < steps.length - 1 && (
            <div className={styles.flowArrow} data-active={active ? "true" : "false"}>
              <div className={styles.flowArrowLine} />
              <IconArrowRight size={11} />
            </div>
          )}
        </div>
      ))}
    </div>
  );
}

/* ── Prerequisites — two-column L1/L2 lanes ── */

function L1toL2Prerequisites({ state }: { state: FlashLoanState }) {
  const l1Contracts: ContractEntry[] = [
    { name: "FlashLoanBridgeExecutor", desc: "Step 1: Initiates the flash loan. Borrows from the pool, bridges tokens to L2 via Bridge.bridgeTokens(), and repays the loan when tokens return.", value: state.executorL1, net: "L1" },
    { name: "FlashPool", desc: "Step 2: Lends 10,000 tokens to the executor. Verifies repayment atomically — if tokens don't return, the entire L1 transaction reverts.", value: state.poolAddress, net: "L1" },
    { name: "FlashToken", desc: "The ERC-20 token being borrowed. Transferred from the pool to the executor, then bridged cross-chain to L2 and back.", value: state.tokenAddress, net: "L1" },
  ];

  const l2Contracts: ContractEntry[] = [
    { name: "WrappedToken", desc: "Step 3: Minted on L2 when tokens arrive from L1 via the Bridge. CREATE2 deterministic address based on the L1 token.", value: state.wrappedTokenL2, net: "L2" },
    { name: "FlashLoanBridgeExecutor", desc: "Step 4: Receives the wrapped tokens on L2, calls claimAndBridgeBack() — claims the NFT and initiates the return bridge to L1.", value: state.executorL2, net: "L2" },
    { name: "FlashLoanersNFT", desc: "Step 5: Token-gated NFT. Requires holding 10,000 wrapped tokens to claim — proof that the cross-chain bridge successfully delivered the funds.", value: state.nftAddress, net: "L2" },
  ];

  return <ContractLanes loading={state.loading} deployed={state.contractsDeployed} l1Contracts={l1Contracts} l2Contracts={l2Contracts} />;
}

function L2toL1Prerequisites({ state }: { state: ReverseFlashLoanState }) {
  const l2Contracts: ContractEntry[] = [
    { name: "ReverseExecutorL2", desc: "Step 1: Initiates the reverse flash loan on L2. Sends bridgeEther(0) cross-chain trigger to L1.", value: state.reverseExecutorL2, net: "L2" },
    { name: "WrappedToken L2", desc: "The wrapped token held by the executor on L2 before the bridge back.", value: state.wrappedTokenL2, net: "L2" },
  ];

  const l1Contracts: ContractEntry[] = [
    { name: "ReverseNFT L1", desc: "The NFT minted on L1 when the cross-chain execution completes. Proof that L2→L1 bridging worked atomically.", value: state.reverseNftL1, net: "L1" },
    { name: "FlashToken", desc: "Underlying ERC-20 token on L1 being returned after the bridge cycle.", value: state.tokenAddress, net: "L1" },
  ];

  return <ContractLanes loading={state.loading} deployed={state.contractsDeployed} l1Contracts={l1Contracts} l2Contracts={l2Contracts} />;
}

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
    <div className={styles.prereqCard}>
      <div className={styles.prereqHeader}>
        <span className={styles.prereqHeaderLeft}>
          {loading ? (
            <Spinner />
          ) : deployed ? (
            <span className={styles.prereqDot} data-status="ok" />
          ) : (
            <span className={styles.prereqDot} data-status="error" />
          )}
          <span className={styles.prereqTitle}>Contracts</span>
        </span>
        {!loading && (
          <span className={styles.statusBadge} data-status={deployed ? "ok" : "error"}>
            {deployed ? <><IconCheck size={10} /> Ready</> : <><IconX size={10} /> Missing</>}
          </span>
        )}
      </div>

      {!deployed && !loading && (
        <div className={styles.prereqWarning}>
          Flash loan contracts not found. Restart with <code>DEPLOY_FLASH_LOAN=true</code>.
        </div>
      )}

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

/* ── Execution step tracker ── */

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

/* ── Build L1→L2 execution steps from state ── */
function buildL1toL2Steps(state: FlashLoanState): ExecutionStep[] {
  const phase = state.phase;
  const ORDER: FlashLoanPhase[] = ["idle", "sending", "processing", "verifying", "complete", "failed"];
  const phaseIdx = (p: FlashLoanPhase) => ORDER.indexOf(p);
  const currentIdx = phaseIdx(phase);

  const PROCESSING_ACTIVE = phaseIdx("processing");
  const VERIFYING_ACTIVE = phaseIdx("verifying");
  const COMPLETE_DONE = phaseIdx("complete");

  function stepStatus(
    activeIdx: number,
    doneIdx: number,
    isDoneOverride?: boolean,
    isActiveOverride?: boolean,
  ): StepStatus {
    if (isDoneOverride === true) return "done";
    if (phase === "failed") {
      if (currentIdx > activeIdx || (currentIdx === activeIdx && isDoneOverride)) return "done";
      if (currentIdx === activeIdx && isActiveOverride !== false) return "failed";
      if (doneIdx <= currentIdx) return "done";
      return "waiting";
    }
    if (currentIdx >= doneIdx) return "done";
    if (currentIdx === activeIdx || isActiveOverride) return "active";
    if (currentIdx > activeIdx) return "done";
    return "waiting";
  }

  const sendStatus: StepStatus = (() => {
    if (phase === "sending") return "active";
    if (phase === "failed" && !state.txHash) return "failed";
    if (state.txHash) return "done";
    return "waiting";
  })();

  const l2Status: StepStatus = (() => {
    if (state.l2Done) return "done";
    if (phase === "processing") return "active";
    if (phase === "failed" && state.txHash && !state.l2Done) return "failed";
    if (phaseIdx(phase) > PROCESSING_ACTIVE) return "done";
    return "waiting";
  })();

  const l1Status: StepStatus = (() => {
    if (state.l1Done) return "done";
    if (phase === "processing") return "active";
    if (phase === "failed" && state.txHash && !state.l1Done) return "failed";
    if (phaseIdx(phase) > PROCESSING_ACTIVE) return "done";
    return "waiting";
  })();

  const verifyStatus: StepStatus = stepStatus(VERIFYING_ACTIVE, COMPLETE_DONE);
  const completeStatus: StepStatus = phase === "complete" ? "done" : "waiting";

  return [
    {
      id: "send",
      label: "Send via L1 Proxy",
      detail: state.txHash ? (
        <span>Tx: <TxLink hash={state.txHash} chain="l1" short /></span>
      ) : phase === "sending" ? (
        <span>Waiting for wallet signature...</span>
      ) : null,
      status: sendStatus,
    },
    {
      id: "l2",
      label: "L2 processes cross-chain entries",
      detail: (() => {
        if (state.l2Done && state.l2BlockBefore !== null && state.l2BlockAfter !== null) {
          return <span>Blocks {state.l2BlockBefore} → {state.l2BlockAfter} (+{state.l2BlockAfter - state.l2BlockBefore})</span>;
        }
        if (phase === "processing" && state.l2BlockBefore !== null) {
          return <span>From block {state.l2BlockBefore}{state.l2BlockAfter !== null ? ` → ${state.l2BlockAfter}` : ""}...</span>;
        }
        if (phase === "processing") return <span>L1 proxy sent entries — waiting for L2 blocks...</span>;
        return null;
      })(),
      status: l2Status,
    },
    {
      id: "l1",
      label: "L1 confirms",
      detail: state.l1BlockNumber !== null ? (
        <span>Block {state.l1BlockNumber}{state.l1GasUsed ? ` · ${state.l1GasUsed} gas` : ""}{state.l1TxStatus === 0 ? " · REVERTED" : ""}</span>
      ) : phase === "processing" ? <span>Waiting for L1 receipt...</span> : null,
      status: l1Status,
    },
    {
      id: "verify",
      label: "Verify results",
      detail: phase === "verifying" ? <span>Checking balances and state roots...</span> : null,
      status: verifyStatus,
    },
    {
      id: "complete",
      label: state.nftMinted ? "NFT Claimed on L2!" : "Complete",
      detail: state.startTime && state.endTime ? <span>Execution time: {formatDuration(state.endTime - state.startTime)}</span> : null,
      status: completeStatus,
    },
  ];
}

/* ── Build L2→L1 execution steps from state ── */
function buildL2toL1Steps(state: ReverseFlashLoanState): ExecutionStep[] {
  const phase = state.phase;
  const ORDER: ReverseFlashLoanPhase[] = ["idle", "sending", "processing", "verifying", "complete", "failed"];
  const phaseIdx = (p: ReverseFlashLoanPhase) => ORDER.indexOf(p);
  const currentIdx = phaseIdx(phase);

  const VERIFYING_ACTIVE = phaseIdx("verifying");
  const COMPLETE_DONE = phaseIdx("complete");

  function stepStatus(activeIdx: number, doneIdx: number): StepStatus {
    if (phase === "failed") {
      if (doneIdx <= currentIdx) return "done";
      if (currentIdx === activeIdx) return "failed";
      return "waiting";
    }
    if (currentIdx >= doneIdx) return "done";
    if (currentIdx === activeIdx) return "active";
    if (currentIdx > activeIdx) return "done";
    return "waiting";
  }

  const sendStatus: StepStatus = (() => {
    if (phase === "sending") return "active";
    if (phase === "failed" && !state.txHash) return "failed";
    if (state.txHash) return "done";
    return "waiting";
  })();

  const l2Status: StepStatus = (() => {
    if (state.l2Done) return "done";
    if (phase === "processing") return "active";
    if (phase === "failed" && state.txHash && !state.l2Done) return "failed";
    return "waiting";
  })();

  const l1Status: StepStatus = (() => {
    if (state.l1Done) return "done";
    if (phase === "processing") return "active";
    if (phase === "failed" && state.txHash && !state.l1Done) return "failed";
    return "waiting";
  })();

  const verifyStatus = stepStatus(VERIFYING_ACTIVE, COMPLETE_DONE);
  const completeStatus: StepStatus = phase === "complete" ? "done" : "waiting";

  return [
    {
      id: "send",
      label: "Send via L2 Proxy",
      detail: state.txHash ? (
        <span>Tx: <TxLink hash={state.txHash} chain="l2" short /></span>
      ) : phase === "sending" ? (
        <span>Waiting for wallet signature...</span>
      ) : null,
      status: sendStatus,
    },
    {
      id: "l2",
      label: "L2 confirms + L2→L1 call queued",
      detail: state.l2BlockNumber !== null ? (
        <span>Block {state.l2BlockNumber}{state.l2GasUsed ? ` · ${state.l2GasUsed} gas` : ""}{state.l2TxStatus === 0 ? " · REVERTED" : ""}</span>
      ) : phase === "processing" ? <span>Waiting for L2 receipt...</span> : null,
      status: l2Status,
    },
    {
      id: "l1",
      label: "L1 trigger fires (postBatch)",
      detail: (() => {
        if (state.l1Done) return <span>L1 cross-chain delivery complete</span>;
        if (phase === "processing") return <span>Waiting for builder to post batch + trigger...</span>;
        return null;
      })(),
      status: l1Status,
    },
    {
      id: "verify",
      label: "Verify results",
      detail: phase === "verifying" ? <span>Checking balances and state roots...</span> : null,
      status: verifyStatus,
    },
    {
      id: "complete",
      label: state.nftMinted ? "NFT Claimed on L1!" : "Complete",
      detail: state.startTime && state.endTime ? <span>Execution time: {formatDuration(state.endTime - state.startTime)}</span> : null,
      status: completeStatus,
    },
  ];
}

/* ── NFT Celebration Card — for L1→L2 (NFT on L2) ── */
function NftCardL2({ nftAddress, executorL2 }: { nftAddress: string; executorL2: string }) {
  return (
    <div className={styles.nftWrapper} role="region" aria-label="NFT claimed on L2">
      <div className={styles.confettiField} aria-hidden>
        {Array.from({ length: 12 }, (_, i) => (
          <span key={i} className={styles.confettiDot} data-i={i} />
        ))}
      </div>
      <div className={styles.nftCard} tabIndex={0} aria-label="FlashLoaners NFT on L2">
        <div className={styles.nftShimmer} aria-hidden />
        <div className={styles.nftTopBadge}>
          <span className={styles.nftVerifiedDot} />
          Minted on L2
        </div>
        <div className={styles.nftTitle}>FlashLoaners</div>
        <div className={styles.nftIconWrap} aria-hidden>
          <img src="/logo.png" alt="" className={styles.nftLogoImg} />
        </div>
        <div className={styles.nftSubtitle}>L1 → L2 Flash Loan</div>
        <div className={styles.nftDetail}>10,000 tokens bridged L1 to L2 and back atomically</div>
        <div className={styles.nftDivider} aria-hidden />
        {nftAddress && nftAddress !== "0x" + "0".repeat(40) && (
          <div className={styles.nftContractRow}>
            <span className={styles.nftContractLabel}>NFT Contract</span>
            <span className={styles.nftContractAddr}>
              <ExplorerLink value={nftAddress} type="address" chain="l2" label={shortAddr(nftAddress)} />
            </span>
          </div>
        )}
        {executorL2 && executorL2 !== "0x" + "0".repeat(40) && (
          <div className={styles.nftContractRow}>
            <span className={styles.nftContractLabel}>Owner</span>
            <span className={styles.nftContractAddr}>
              <ExplorerLink value={executorL2} type="address" chain="l2" label={shortAddr(executorL2)} />
            </span>
          </div>
        )}
        <div className={styles.nftCheckBadge}>
          <IconCheck size={13} />
          Verified on L2
        </div>
      </div>
    </div>
  );
}

/* ── NFT Celebration Card — for L2→L1 (NFT on L1) ── */
function NftCardL1({ nftAddress, executorL2 }: { nftAddress: string; executorL2: string }) {
  return (
    <div className={styles.nftWrapper} role="region" aria-label="NFT claimed on L1">
      <div className={styles.confettiField} aria-hidden>
        {Array.from({ length: 12 }, (_, i) => (
          <span key={i} className={styles.confettiDot} data-i={i} />
        ))}
      </div>
      <div className={`${styles.nftCard} ${styles.nftCardReverse}`} tabIndex={0} aria-label="Reverse FlashLoaners NFT on L1">
        <div className={styles.nftShimmer} aria-hidden />
        <div className={`${styles.nftTopBadge} ${styles.nftTopBadgeL1}`}>
          <span className={`${styles.nftVerifiedDot} ${styles.nftVerifiedDotL1}`} />
          Minted on L1
        </div>
        <div className={styles.nftTitle}>ReverseFlashLoaners</div>
        <div className={styles.nftIconWrap} aria-hidden>
          <img src="/logo.png" alt="" className={`${styles.nftLogoImg} ${styles.nftLogoImgReverse}`} />
        </div>
        <div className={styles.nftSubtitle}>L2 → L1 Flash Loan</div>
        <div className={styles.nftDetail}>Cross-chain execution: L2 trigger, L1 delivery</div>
        <div className={styles.nftDivider} aria-hidden />
        {nftAddress && nftAddress !== "0x" + "0".repeat(40) && (
          <div className={styles.nftContractRow}>
            <span className={styles.nftContractLabel}>NFT Contract</span>
            <span className={styles.nftContractAddr}>
              <ExplorerLink value={nftAddress} type="address" chain="l1" label={shortAddr(nftAddress)} />
            </span>
          </div>
        )}
        {executorL2 && executorL2 !== "0x" + "0".repeat(40) && (
          <div className={styles.nftContractRow}>
            <span className={styles.nftContractLabel}>Executor</span>
            <span className={styles.nftContractAddr}>
              <ExplorerLink value={executorL2} type="address" chain="l2" label={shortAddr(executorL2)} />
            </span>
          </div>
        )}
        <div className={`${styles.nftCheckBadge} ${styles.nftCheckBadgeL1}`}>
          <IconCheck size={13} />
          Verified on L1
        </div>
      </div>
    </div>
  );
}

/* ── Results Panel for L1→L2 ── */
function L1toL2Results({ state }: { state: FlashLoanState }) {
  const isAlreadyClaimed = state.alreadyClaimed && state.phase === "idle";
  if (!isAlreadyClaimed && state.phase !== "complete" && state.phase !== "failed") return null;
  if (!isAlreadyClaimed && !state.txHash) return null;

  const duration = state.startTime && state.endTime ? formatDuration(state.endTime - state.startTime) : null;

  return (
    <div className={styles.resultsCard}>
      <div className={styles.resultsTitle}>
        {isAlreadyClaimed ? "Previous Claim Details" : state.phase === "complete" ? "Execution Results" : "Execution Failed"}
      </div>
      <div className={styles.resultsGrid}>
        {(state.txHash || state.claimL1TxHash) && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>L1 Transaction</span>
            <span className={styles.resultValue}>
              <TxLink hash={(state.txHash || state.claimL1TxHash)!} chain="l1" short={false} />
            </span>
          </div>
        )}
        {(state.l1BlockNumber !== null || state.claimL1Block !== null) && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>L1 Block</span>
            <span className={styles.resultValue}>
              {(() => {
                const block = state.l1BlockNumber ?? state.claimL1Block;
                return block !== null
                  ? <ExplorerLink value={block.toString()} type="block" chain="l1" label={`#${block}`} />
                  : "—";
              })()}
              {state.l1GasUsed ? ` · ${state.l1GasUsed} gas` : ""}
            </span>
          </div>
        )}
        {state.claimL2Block !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>L2 Claim Block</span>
            <span className={styles.resultValue}>
              <ExplorerLink value={state.claimL2Block.toString()} type="block" chain="l2" label={`#${state.claimL2Block}`} />
            </span>
          </div>
        )}
        {state.claimL2TxHash && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>L2 Claim Transaction</span>
            <span className={styles.resultValue}>
              <TxLink hash={state.claimL2TxHash} chain="l2" short={false} />
            </span>
          </div>
        )}
        {state.nftTokenId !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>NFT Token ID</span>
            <span className={styles.resultValue}>
              {state.nftAddress ? (
                <ExplorerLink value={state.nftAddress} type="address" chain="l2" label={`#${state.nftTokenId}`} />
              ) : (
                <span className={styles.monoPill}>#{state.nftTokenId}</span>
              )}
            </span>
          </div>
        )}
        {(state.poolBalanceBefore !== null || state.poolBalanceAfter !== null) && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>Pool Token Balance</span>
            <span className={styles.resultValue}>
              {state.poolBalanceBefore !== null ? (
                <>
                  <span className={styles.balanceBefore}>{state.poolBalanceBefore}</span>
                  <span className={styles.balanceArrow}>{" → "}</span>
                  <span className={styles.balanceAfter}>{state.poolBalanceAfter ?? "—"}</span>
                  {state.poolBalanceAfter !== null && (
                    <span className={styles.balanceNote}>
                      {state.poolBalanceBefore === state.poolBalanceAfter ? " (unchanged — loan repaid)" : " (changed)"}
                    </span>
                  )}
                </>
              ) : (
                <span className={styles.balanceAfter}>
                  {state.poolBalanceAfter} tokens
                  <span className={styles.balanceNote}> (current balance — loan was repaid)</span>
                </span>
              )}
            </span>
          </div>
        )}
        <StateRootRow builderRoot={state.builderStateRoot} fullnodeRoot={state.fullnodeStateRoot} match={state.stateRootsMatch} />
        {state.l2BlockBefore !== null && state.l2BlockAfter !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>L2 Blocks Processed</span>
            <span className={styles.resultValue}>
              {state.l2BlockAfter - state.l2BlockBefore} blocks ({state.l2BlockBefore} → {state.l2BlockAfter})
            </span>
          </div>
        )}
        {state.nftMinted !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>NFT Claimed (L2)</span>
            <span className={styles.resultValue}>
              {state.nftMinted ? (
                <span className={styles.stateMatch}><IconCheck size={12} /> NFT minted to L2 executor</span>
              ) : (
                <span className={styles.stateUnknown}>Not detected</span>
              )}
            </span>
          </div>
        )}
        {duration && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>Total Time</span>
            <span className={styles.resultValue}>{duration}</span>
          </div>
        )}
        {state.error && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>Error</span>
            <span className={`${styles.resultValue} ${styles.resultError}`}>{state.error}</span>
          </div>
        )}
        {(state.claimL1Block ?? state.l1BlockNumber) !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>Inspect</span>
            <span className={styles.resultValue}>
              <a href={`#/visualizer?block=${state.claimL1Block ?? state.l1BlockNumber}`} className={styles.vizLink}>
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

/* ── Results Panel for L2→L1 ── */
function L2toL1Results({ state }: { state: ReverseFlashLoanState }) {
  const isAlreadyClaimed = state.alreadyClaimed && state.phase === "idle";
  if (!isAlreadyClaimed && state.phase !== "complete" && state.phase !== "failed") return null;
  if (!isAlreadyClaimed && !state.txHash) return null;

  const duration = state.startTime && state.endTime ? formatDuration(state.endTime - state.startTime) : null;

  return (
    <div className={`${styles.resultsCard} ${styles.resultsCardReverse}`}>
      <div className={styles.resultsTitle}>
        {isAlreadyClaimed ? "Previous Claim Details (L2→L1)" : state.phase === "complete" ? "Execution Results" : "Execution Failed"}
      </div>
      <div className={styles.resultsGrid}>
        {state.txHash && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>L2 Transaction</span>
            <span className={styles.resultValue}>
              <TxLink hash={state.txHash} chain="l2" short={false} />
            </span>
          </div>
        )}
        {state.l2BlockNumber !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>L2 Block</span>
            <span className={styles.resultValue}>
              <ExplorerLink value={state.l2BlockNumber.toString()} type="block" chain="l2" label={`#${state.l2BlockNumber}`} />
              {state.l2GasUsed ? ` · ${state.l2GasUsed} gas` : ""}
            </span>
          </div>
        )}
        {state.claimL1Block !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>L1 Delivery Block</span>
            <span className={styles.resultValue}>
              <ExplorerLink value={state.claimL1Block.toString()} type="block" chain="l1" label={`#${state.claimL1Block}`} />
            </span>
          </div>
        )}
        {state.claimL1TxHash && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>L1 Delivery Tx</span>
            <span className={styles.resultValue}>
              <TxLink hash={state.claimL1TxHash} chain="l1" short={false} />
            </span>
          </div>
        )}
        {(state.wrappedTokenBalanceBefore !== null || state.wrappedTokenBalanceAfter !== null) && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>Wrapped Token (L2)</span>
            <span className={styles.resultValue}>
              {state.wrappedTokenBalanceBefore !== null ? (
                <>
                  <span className={styles.balanceBefore}>{state.wrappedTokenBalanceBefore}</span>
                  <span className={styles.balanceArrow}>{" → "}</span>
                  <span className={styles.balanceAfter}>{state.wrappedTokenBalanceAfter ?? "—"}</span>
                </>
              ) : (
                <span>{state.wrappedTokenBalanceAfter ?? "—"}</span>
              )}
            </span>
          </div>
        )}
        <StateRootRow builderRoot={state.builderStateRoot} fullnodeRoot={state.fullnodeStateRoot} match={state.stateRootsMatch} />
        {state.nftMinted !== null && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>NFT Claimed (L1)</span>
            <span className={styles.resultValue}>
              {state.nftMinted ? (
                <span className={styles.stateMatch}><IconCheck size={12} /> NFT minted on L1</span>
              ) : (
                <span className={styles.stateUnknown}>Not detected</span>
              )}
            </span>
          </div>
        )}
        {duration && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>Total Time</span>
            <span className={styles.resultValue}>{duration}</span>
          </div>
        )}
        {state.error && (
          <div className={styles.resultRow}>
            <span className={styles.resultLabel}>Error</span>
            <span className={`${styles.resultValue} ${styles.resultError}`}>{state.error}</span>
          </div>
        )}
      </div>
    </div>
  );
}

/* ── Shared state root row ── */
function StateRootRow({
  builderRoot,
  fullnodeRoot,
  match,
}: {
  builderRoot: string | null;
  fullnodeRoot: string | null;
  match: boolean | null;
}) {
  if (builderRoot === null) return null;
  return (
    <>
      <div className={styles.resultRow}>
        <span className={styles.resultLabel}>State Roots</span>
        <span className={styles.resultValue}>
          {fullnodeRoot === null ? (
            <span className={styles.stateMatch}><IconCheck size={12} /> Composer verified <span className={styles.balanceNote}>(fullnode unreachable)</span></span>
          ) : match === true ? (
            <span className={styles.stateMatch}><IconCheck size={12} /> Composer = Fullnode1</span>
          ) : (
            <span className={styles.stateMismatch}><IconX size={12} /> Mismatch</span>
          )}
        </span>
      </div>
      <div className={styles.resultRow}>
        <span className={styles.resultLabel}>Composer Root</span>
        <span className={styles.resultValue}><span className={styles.monoPill}>{shortHash(builderRoot)}</span></span>
      </div>
      {fullnodeRoot && (
        <div className={styles.resultRow}>
          <span className={styles.resultLabel}>Fullnode1 Root</span>
          <span className={styles.resultValue}><span className={styles.monoPill}>{shortHash(fullnodeRoot)}</span></span>
        </div>
      )}
    </>
  );
}

/* ── How it works — L1→L2 swim diagram ── */
type SwimCol = "l1" | "proxy" | "builder" | "l2";

interface SwimStep { col: SwimCol; label: string; desc: string; }
interface SwimArrow { from: SwimCol; to: SwimCol; label: string; }
type SwimEntry = { type: "step"; step: SwimStep } | { type: "arrow"; arrow: SwimArrow };

const L1_TO_L2_SWIM: SwimEntry[] = [
  { type: "step", step: { col: "l1", label: "execute()", desc: "User calls FlashLoanBridgeExecutor" } },
  { type: "arrow", arrow: { from: "l1", to: "proxy", label: "tx sent to L1 proxy" } },
  { type: "step", step: { col: "proxy", label: "Simulate", desc: "Discovers CALL_A, CALL_B, CALL_C" } },
  { type: "arrow", arrow: { from: "proxy", to: "builder", label: "entries + raw tx" } },
  { type: "step", step: { col: "builder", label: "Build L2 block", desc: "Includes protocol txs" } },
  { type: "arrow", arrow: { from: "builder", to: "l2", label: "block with protocol txs" } },
  { type: "step", step: { col: "l2", label: "loadExecutionTable", desc: "3 continuation entries loaded" } },
  { type: "step", step: { col: "l2", label: "executeIncomingCrossChainCall", desc: "Triggers full continuation chain" } },
  { type: "step", step: { col: "l2", label: "receiveTokens → claimAndBridgeBack", desc: "NFT claimed + bridge return to L1" } },
  { type: "arrow", arrow: { from: "l2", to: "l1", label: "postBatch + forward tx" } },
  { type: "step", step: { col: "l1", label: "FlashPool repaid", desc: "onFlashLoan callback — atomic" } },
  { type: "step", step: { col: "l1", label: "Complete", desc: "Pool unchanged. Roots converge." } },
];

const L2_TO_L1_SWIM: SwimEntry[] = [
  { type: "step", step: { col: "l2", label: "execute()", desc: "User calls ReverseExecutorL2" } },
  { type: "arrow", arrow: { from: "l2", to: "proxy", label: "tx sent to L2 proxy" } },
  { type: "step", step: { col: "proxy", label: "Detect L2→L1 Call", desc: "Traces tx for executeCrossChainCall" } },
  { type: "arrow", arrow: { from: "proxy", to: "builder", label: "L2→L1 entries + forward" } },
  { type: "step", step: { col: "builder", label: "Build L2 block", desc: "Include cross-chain protocol tx" } },
  { type: "arrow", arrow: { from: "builder", to: "l1", label: "postBatch + executeL2TX" } },
  { type: "step", step: { col: "l1", label: "executeL2TX", desc: "L1 receives L2→L1 trigger" } },
  { type: "step", step: { col: "l1", label: "NFT Claimed on L1", desc: "ReverseNFT minted to executor" } },
  { type: "step", step: { col: "l1", label: "Complete", desc: "Cross-chain delivery verified." } },
];

const COL_INDEX: Record<SwimCol, number> = { l1: 0, proxy: 1, builder: 2, l2: 3 };
const COL_COLORS: Record<SwimCol, string> = {
  l1: "rgba(99, 102, 241, 0.5)",
  proxy: "rgba(34, 211, 238, 0.5)",
  builder: "rgba(251, 191, 36, 0.5)",
  l2: "rgba(52, 211, 153, 0.5)",
};

function SwimDiagram({ entries, techNote }: { entries: SwimEntry[]; techNote: string }) {
  return (
    <div className={styles.swimDiagram}>
      <div className={styles.swimHeaders}>
        <div className={styles.swimHeader} data-col="l1">L1</div>
        <div className={styles.swimHeader} data-col="proxy">Proxy</div>
        <div className={styles.swimHeader} data-col="builder">Composer</div>
        <div className={styles.swimHeader} data-col="l2">L2</div>
      </div>
      <div className={styles.swimBody}>
        <div className={styles.swimLaneLine} style={{ left: "12.5%" }} data-col="l1" />
        <div className={styles.swimLaneLine} style={{ left: "37.5%" }} data-col="proxy" />
        <div className={styles.swimLaneLine} style={{ left: "62.5%" }} data-col="builder" />
        <div className={styles.swimLaneLine} style={{ left: "87.5%" }} data-col="l2" />
        <div className={styles.swimSteps}>
          {entries.map((entry, i) => (
            <div key={i} className={styles.swimRow} style={{ animationDelay: `${i * 0.05}s` }}>
              {entry.type === "step" ? (
                <div className={styles.swimRowGrid}>
                  {(["l1", "proxy", "builder", "l2"] as SwimCol[]).map((col) => (
                    <div key={col} className={styles.swimCell}>
                      {col === entry.step.col && (
                        <div className={styles.swimCard} data-col={col}>
                          <div className={styles.swimCardLabel}>{entry.step.label}</div>
                          <div className={styles.swimCardDesc}>{entry.step.desc}</div>
                        </div>
                      )}
                    </div>
                  ))}
                </div>
              ) : (
                <div className={styles.swimArrowRow}>
                  {(() => {
                    const fromIdx = COL_INDEX[entry.arrow.from];
                    const toIdx = COL_INDEX[entry.arrow.to];
                    const leftIdx = Math.min(fromIdx, toIdx);
                    const dir = toIdx > fromIdx ? "right" : "left";
                    const midPct = ((fromIdx + toIdx) / 2) * 25 + 12.5;
                    return (
                      <>
                        <div
                          className={styles.swimArrowLine}
                          style={{
                            marginLeft: `${(leftIdx * 25) + 12.5}%`,
                            width: `${Math.abs(toIdx - fromIdx) * 25}%`,
                            background: `linear-gradient(${dir === "right" ? "90deg" : "270deg"}, ${COL_COLORS[entry.arrow.from]}, ${COL_COLORS[entry.arrow.to]})`,
                          }}
                          data-dir={dir}
                          data-to={entry.arrow.to}
                          data-from={entry.arrow.from}
                        />
                        <span
                          className={styles.swimArrowLabel}
                          style={{ position: "absolute", left: `${midPct}%`, transform: "translateX(-50%)" }}
                        >
                          {entry.arrow.label}
                        </span>
                      </>
                    );
                  })()}
                </div>
              )}
            </div>
          ))}
        </div>
      </div>
      <div className={styles.techNote}>
        <strong>Key insight:</strong> {techNote}
      </div>
    </div>
  );
}

function HowItWorksExpander({ direction }: { direction: FlashLoanDirection }) {
  const [open, setOpen] = useState(false);
  const isL1toL2 = direction === "l1-to-l2";
  const entries = isL1toL2 ? L1_TO_L2_SWIM : L2_TO_L1_SWIM;
  const techNote = isL1toL2
    ? "The L1 proxy simulates the tx on the L1 node to discover cross-chain calls before anything is submitted. It sends entries to the composer, which processes them on L2 before forwarding the original tx to L1. L2 executes the continuation chain before L1 even confirms."
    : "The L2 composer traces the tx to detect cross-chain proxy calls (executeCrossChainCall). It queues entries BEFORE forwarding the user tx (hold-then-forward). After the L2 block is built, the builder posts a batch to L1 and fires the executeL2TX trigger, which executes the cross-chain call on L1.";

  return (
    <div className={`${styles.expandable} ${open ? styles.expandableOpen : ""}`}>
      <button
        className={styles.expandableHeader}
        onClick={() => setOpen(!open)}
        aria-expanded={open}
      >
        <span className={styles.expandableIcon}>
          <IconLightning size={16} />
        </span>
        <span className={styles.expandableTitle}>How it works</span>
        <span className={styles.expandableChevron} data-open={open ? "true" : "false"}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
            <polyline points="6 9 12 15 18 9" />
          </svg>
        </span>
      </button>
      {open && (
        <div className={styles.expandableBody}>
          <SwimDiagram entries={entries} techNote={techNote} />
        </div>
      )}
    </div>
  );
}

/* ── Deploy Section ── */
function DeploySection({ deployState, onDeploy }: { deployState: DeployState; onDeploy: () => void }) {
  const { phase, deployStep, error, executorL2, proxyL1, executorL1 } = deployState;
  const isDeploying = phase === "deploying-l2" || phase === "deploying-proxy" || phase === "deploying-l1";

  if (phase === "checking") return null;

  if (phase === "ready") {
    return (
      <div className={styles.prereqCard}>
        <div className={styles.prereqHeader}>
          <span className={styles.prereqHeaderLeft}>
            <span className={styles.prereqDot} data-status="ok" />
            <span className={styles.prereqTitle}>Your Executors</span>
          </span>
          <span className={styles.statusBadge} data-status="ok">
            <IconCheck size={10} /> Deployed
          </span>
        </div>
        <div className={styles.lanes}>
          <div className={styles.lane} data-net="L1">
            <div className={styles.laneHeader}>
              <span className={styles.laneDot} data-net="L1" />
              <span className={styles.laneLabel}>L1 Ethereum</span>
            </div>
            <div className={styles.laneChips}>
              {executorL1 && executorL1 !== "0x" + "0".repeat(40) && (
                <div className={styles.chip} data-net="L1">
                  <span className={styles.chipDot} data-ok="true" />
                  <span className={styles.chipName}>
                    <ExplorerLink value={executorL1} type="address" chain="l1" label="Your L1 Executor" />
                  </span>
                </div>
              )}
              {proxyL1 && proxyL1 !== "0x" + "0".repeat(40) && (
                <div className={styles.chip} data-net="L1">
                  <span className={styles.chipDot} data-ok="true" />
                  <span className={styles.chipName}>
                    <ExplorerLink value={proxyL1} type="address" chain="l1" label="Your L1 Proxy" />
                  </span>
                </div>
              )}
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
              {executorL2 && executorL2 !== "0x" + "0".repeat(40) && (
                <div className={styles.chip} data-net="L2">
                  <span className={styles.chipDot} data-ok="true" />
                  <span className={styles.chipName}>
                    <ExplorerLink value={executorL2} type="address" chain="l2" label="Your L2 Executor" />
                  </span>
                </div>
              )}
            </div>
          </div>
        </div>
      </div>
    );
  }

  const deploySteps = [
    "Deploy L2 FlashLoanBridgeExecutor",
    "Create L1 CrossChainProxy",
    "Deploy L1 FlashLoanBridgeExecutor",
  ];

  return (
    <div className={styles.prereqCard}>
      <div className={styles.prereqHeader}>
        <span className={styles.prereqHeaderLeft}>
          <span className={styles.prereqDot} data-status={isDeploying ? "ok" : "error"} />
          <span className={styles.prereqTitle}>{isDeploying ? "Deploying..." : "Deploy Your Executors"}</span>
        </span>
        {!isDeploying && (
          <span className={styles.statusBadge} data-status="error">
            <IconX size={10} /> Not deployed
          </span>
        )}
      </div>

      {!isDeploying && (
        <div className={styles.prereqWarning}>
          These are personal executors you deploy yourself — separate from the shared protocol contracts above.
          They let you run the flash loan with your own wallet address.
        </div>
      )}

      <div className={styles.stepTracker}>
        {deploySteps.map((label, i) => {
          const status: StepStatus =
            isDeploying && deployStep === i ? "active"
            : isDeploying && deployStep > i ? "done"
            : "waiting";
          return (
            <div key={i} className={styles.stepRow} data-status={status}>
              <div className={styles.stepLeft}>
                <div className={styles.stepCircle} data-status={status}>
                  {status === "done" ? <IconCheck size={12} /> : status === "active" ? <Spinner /> : <span className={styles.stepNumber}>{i + 1}</span>}
                </div>
                {i < deploySteps.length - 1 && <div className={styles.stepLine} data-status={status} />}
              </div>
              <div className={styles.stepBody}>
                <div className={styles.stepLabel}>{label}</div>
              </div>
            </div>
          );
        })}
      </div>

      {error && (
        <div className={styles.prereqWarning} style={{ color: "var(--red)" }}>
          {error}
        </div>
      )}

      {!isDeploying && (
        <button
          className="btn btn-solid btn-accent btn-block"
          onClick={onDeploy}
          style={{ marginTop: 4 }}
        >
          <IconLightning size={14} />
          Deploy My Executors
        </button>
      )}
    </div>
  );
}

/* ── Main props ── */
interface Props {
  state: FlashLoanState;
  reverseState: ReverseFlashLoanState;
  deployState: DeployState;
  onDeploy: () => void;
  onExecute: () => void;
  onExecuteReverse: () => void;
  onReset: () => void;
  onResetReverse: () => void;
  walletConnected: boolean;
  walletAddress: string | null;
}

/* ── Main FlashLoanPanel component ── */
export function FlashLoanPanel({
  state,
  reverseState,
  deployState,
  onDeploy,
  onExecute,
  onExecuteReverse,
  onReset,
  onResetReverse,
  walletConnected: _walletConnected,
  walletAddress: _walletAddress,
}: Props) {
  const [direction, setDirection] = useState<FlashLoanDirection>("l1-to-l2");

  const isL1toL2 = direction === "l1-to-l2";
  const activeState = isL1toL2 ? state : reverseState;

  const busy = activeState.phase !== "idle" && activeState.phase !== "complete" && activeState.phase !== "failed";
  const complete = activeState.phase === "complete";
  const failed = activeState.phase === "failed";
  const alreadyClaimed = activeState.alreadyClaimed;
  const contractsDeployed = activeState.contractsDeployed;

  function handleSwap() {
    setDirection(isL1toL2 ? "l2-to-l1" : "l1-to-l2");
  }

  function handleExecute() {
    if (isL1toL2) onExecute();
    else onExecuteReverse();
  }

  function handleReset() {
    if (isL1toL2) onReset();
    else onResetReverse();
  }

  const steps = isL1toL2 ? buildL1toL2Steps(state) : buildL2toL1Steps(reverseState);
  const showSteps = activeState.phase !== "idle" || alreadyClaimed;
  const showNft = activeState.nftMinted && (complete || alreadyClaimed);

  const directionLabel = isL1toL2 ? "L1 → L2" : "L2 → L1";
  const buttonLabel = isL1toL2 ? "Execute Flash Loan" : "Execute Reverse Flash Loan";
  const busyLabel = isL1toL2 ? "Executing L1 → L2 Flash Loan..." : "Executing L2 → L1 Flash Loan...";

  // For button colors: L1→L2 uses accent (indigo/purple), L2→L1 uses cyan/green
  const btnClass = isL1toL2 ? "btn btn-solid btn-accent btn-block" : "btn btn-solid btn-green btn-block";

  return (
    <div className={styles.root}>
      {/* Hero section */}
      <div className={styles.hero}>
        <div className={styles.heroGlow} />
        <div className={styles.heroBadge}>
          <IconLightning size={12} />
          Cross-Chain Flash Loan
        </div>
        <h2 className={styles.heroTitle}>Atomic Cross-Chain Flash Loans</h2>
        <p className={styles.heroSubtitle}>
          Borrow tokens, bridge them across chains, claim an NFT, and return the loan —
          all in a single atomic transaction. Select a direction and execute.
        </p>

        {/* Stats row */}
        <div className={styles.heroStats}>
          <div className={styles.heroStat}>
            <span className={styles.heroStatValue}>10,000</span>
            <span className={styles.heroStatLabel}>Tokens Borrowed</span>
          </div>
          <div className={styles.heroStatDivider} />
          <div className={styles.heroStat}>
            <span className={styles.heroStatValue}>2</span>
            <span className={styles.heroStatLabel}>Directions</span>
          </div>
          <div className={styles.heroStatDivider} />
          <div className={styles.heroStat}>
            <span className={styles.heroStatValue}>1</span>
            <span className={styles.heroStatLabel}>Transaction</span>
          </div>
          <div className={styles.heroStatDivider} />
          <div className={styles.heroStat}>
            <span className={styles.heroStatValue}>Atomic</span>
            <span className={styles.heroStatLabel}>Guarantee</span>
          </div>
        </div>
      </div>

      {/* Direction toggle + flow diagram */}
      <div className={styles.directionSection}>
        <DirectionToggle direction={direction} onSwap={handleSwap} />
        <FlowDiagram direction={direction} phase={activeState.phase} />
      </div>

      {/* Contracts prerequisite check */}
      {isL1toL2 ? (
        <L1toL2Prerequisites state={state} />
      ) : (
        <L2toL1Prerequisites state={reverseState} />
      )}

      {/* User-deployed executors (L1→L2 only) */}
      {isL1toL2 && (
        <DeploySection deployState={deployState} onDeploy={onDeploy} />
      )}

      {/* Already claimed notice */}
      {alreadyClaimed && (activeState.phase === "idle") && (
        <div className={styles.claimedBanner}>
          <span className={styles.claimedIcon}><IconCheck size={14} /></span>
          <div>
            <div className={styles.claimedTitle}>
              {isL1toL2 ? "NFT already claimed on L2" : "NFT already claimed on L1"}
            </div>
            <div className={styles.claimedDesc}>
              You already own a FlashLoaners NFT. See results below for details.
            </div>
          </div>
        </div>
      )}

      {/* Execute button */}
      <button
        className={btnClass}
        onClick={handleExecute}
        disabled={busy || !contractsDeployed}
        style={{ fontSize: 14, padding: "14px 20px", fontWeight: 700 }}
      >
        {busy ? (
          <><span className="btn-spinner" /> {busyLabel}</>
        ) : (
          <><IconLightning size={15} /> {buttonLabel} ({directionLabel})</>
        )}
      </button>

      {/* Step tracker (only shown when active or complete) */}
      {showSteps && (
        <div className={styles.trackerCard}>
          <div className={styles.trackerTitle}>Execution Progress</div>
          <StepTracker steps={steps} />
          {(complete || failed) && (
            <button className="btn btn-sm btn-ghost" onClick={handleReset} style={{ marginTop: 8 }}>
              Reset
            </button>
          )}
        </div>
      )}

      {/* NFT celebration card */}
      {showNft && (
        isL1toL2 ? (
          <NftCardL2 nftAddress={state.nftAddress} executorL2={state.executorL2} />
        ) : (
          <NftCardL1 nftAddress={reverseState.reverseNftL1} executorL2={reverseState.reverseExecutorL2} />
        )
      )}

      {/* Results */}
      {isL1toL2 ? (
        <L1toL2Results state={state} />
      ) : (
        <L2toL1Results state={reverseState} />
      )}

      {/* How it works expandable */}
      <HowItWorksExpander direction={direction} />
    </div>
  );
}
