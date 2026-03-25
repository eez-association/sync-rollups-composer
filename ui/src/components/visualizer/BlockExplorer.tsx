/**
 * BlockExplorer — dual-lane L1 + L2 block explorer.
 * L1 events on the left, decoded L2 blocks on the right.
 * Supports both L1 and L2 entry: L2 mode resolves the parent L1 block
 * from the setContext() calldata and shows the same dual-lane view.
 */

import { useState, useEffect, useCallback, useRef } from "react";
import { config } from "../../config";
import { fetchBlockLogs, findLatestEventBlock, findLatestPostedL2Block, findL1BlockForL2Block } from "../../lib/blockLogDecoder";
import type { DecodedBlock } from "../../lib/blockLogDecoder";
import { ExplorerLink } from "../ExplorerLink";
import { TxCard } from "./TxCard";
import { L2BlockCard } from "./L2BlockCard";
import { BlockFlowDiagram } from "./BlockFlowDiagram";
import styles from "./BlockExplorer.module.css";

type Chain = "l1" | "l2";

interface BlockExplorerProps {
  initialBlock?: number | null;
}

export function BlockExplorer({ initialBlock }: BlockExplorerProps) {
  const [chain, setChain] = useState<Chain>("l1");
  const [blockInput, setBlockInput] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [decodedBlock, setDecodedBlock] = useState<DecodedBlock | null>(null);
  // In L2 mode, track which L2 block number the user entered (for navigation)
  const [l2EntryBlock, setL2EntryBlock] = useState<number | null>(null);

  // Navigation uses the chain-specific block number
  const currentBlockNumber = chain === "l1"
    ? decodedBlock?.blockNumber ?? null
    : l2EntryBlock;

  // ─── Core L1 fetch (shared by both modes) ───
  const fetchL1 = useCallback(async (num: number) => {
    const result = await fetchBlockLogs(num);
    setDecodedBlock(result);
    return result;
  }, []);

  // ─── Fetch by L1 block number ───
  const fetchByL1 = useCallback(async (num: number) => {
    setLoading(true);
    setError(null);
    try {
      await fetchL1(num);
      setBlockInput(num.toString());
      setL2EntryBlock(null);
    } catch (e) {
      setError((e as Error).message);
      setDecodedBlock(null);
    } finally {
      setLoading(false);
    }
  }, [fetchL1]);

  // ─── Fetch by L2 block number: find L1 block that posted it → fetch L1 ───
  const fetchByL2 = useCallback(async (num: number) => {
    setLoading(true);
    setError(null);
    try {
      const l1BlockNum = await findL1BlockForL2Block(num);
      if (l1BlockNum == null) {
        setError(`Could not find L1 block containing L2 block ${num}`);
        setDecodedBlock(null);
        setL2EntryBlock(null);
        setLoading(false);
        return;
      }
      await fetchL1(l1BlockNum);
      setL2EntryBlock(num);
      setBlockInput(num.toString());
    } catch (e) {
      setError((e as Error).message);
      setDecodedBlock(null);
      setL2EntryBlock(null);
    } finally {
      setLoading(false);
    }
  }, [fetchL1]);

  // ─── Unified fetch ───
  const fetchBlock = useCallback(async (num: number) => {
    if (chain === "l2") {
      await fetchByL2(num);
    } else {
      await fetchByL1(num);
    }
  }, [chain, fetchByL1, fetchByL2]);

  const handleFetch = useCallback(() => {
    const num = parseInt(blockInput, 10);
    if (isNaN(num) || num < 0) {
      setError("Enter a valid block number");
      return;
    }
    fetchBlock(num);
  }, [blockInput, fetchBlock]);

  // ─── Fetch latest (L1 or L2) ───
  const fetchLatestL1 = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const latest = await findLatestEventBlock();
      if (latest === null) {
        setError("No Rollups events found in recent blocks");
        return;
      }
      await fetchL1(latest);
      setBlockInput(latest.toString());
      setL2EntryBlock(null);
    } catch (e) {
      setError((e as Error).message);
      setDecodedBlock(null);
    } finally {
      setLoading(false);
    }
  }, [fetchL1]);

  const fetchLatestL2 = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await findLatestPostedL2Block();
      if (result === null) {
        setError("No posted L2 blocks found");
        return;
      }
      await fetchL1(result.l1Block);
      setL2EntryBlock(result.l2Block);
      setBlockInput(result.l2Block.toString());
    } catch (e) {
      setError((e as Error).message);
      setDecodedBlock(null);
    } finally {
      setLoading(false);
    }
  }, [fetchL1]);

  const handleLatest = useCallback(async () => {
    if (chain === "l2") {
      await fetchLatestL2();
    } else {
      await fetchLatestL1();
    }
  }, [chain, fetchLatestL1, fetchLatestL2]);

  const handlePrev = useCallback(() => {
    if (currentBlockNumber != null && currentBlockNumber > 0) {
      fetchBlock(currentBlockNumber - 1);
    }
  }, [currentBlockNumber, fetchBlock]);

  const handleNext = useCallback(() => {
    if (currentBlockNumber != null) {
      fetchBlock(currentBlockNumber + 1);
    }
  }, [currentBlockNumber, fetchBlock]);

  const handleKeyDown = useCallback((e: React.KeyboardEvent) => {
    if (e.key === "Enter") handleFetch();
  }, [handleFetch]);

  // ─── Chain switch: auto-fetch latest on the new chain ───
  // Use ref to track pending chain switch so the effect can trigger the fetch
  const pendingChainSwitch = useRef<Chain | null>(null);

  const handleChainSwitch = useCallback((newChain: Chain) => {
    if (newChain === chain) return;
    setChain(newChain);
    setDecodedBlock(null);
    setL2EntryBlock(null);
    setBlockInput("");
    setError(null);
    pendingChainSwitch.current = newChain;
  }, [chain]);

  // After chain state updates, fetch latest for the new chain
  useEffect(() => {
    if (pendingChainSwitch.current == null) return;
    const target = pendingChainSwitch.current;
    pendingChainSwitch.current = null;
    if (target === "l2") {
      fetchLatestL2();
    } else {
      fetchLatestL1();
    }
  }, [chain]); // eslint-disable-line react-hooks/exhaustive-deps

  // Keyboard navigation: left/right arrows for prev/next block
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) return;
      if (e.key === "ArrowLeft") handlePrev();
      if (e.key === "ArrowRight") handleNext();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [handlePrev, handleNext]);

  // Auto-fetch on mount: specific block from URL, or latest
  useEffect(() => {
    if (!config.rollupsAddress) return;
    if (initialBlock != null && !isNaN(initialBlock) && initialBlock >= 0) {
      fetchByL1(initialBlock);
    } else {
      fetchLatestL1();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Guard: rollupsAddress not configured
  if (!config.rollupsAddress) {
    return (
      <div className={styles.emptyState}>
        <div className={styles.emptyIcon}>
          <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
            <circle cx="12" cy="12" r="10" /><line x1="12" y1="8" x2="12" y2="12" /><line x1="12" y1="16" x2="12.01" y2="16" />
          </svg>
        </div>
        <div className={styles.emptyTitle}>Rollups address not configured</div>
        <div className={styles.emptySub}>
          Set the <code>rollups</code> URL parameter or ensure <code>/shared/rollup.env</code> is loaded.
        </div>
      </div>
    );
  }

  const l2BlockCount = decodedBlock?.l2Blocks?.length ?? 0;
  const hasL2Blocks = l2BlockCount > 0;

  return (
    <div>
      {/* Query Bar */}
      <div className={styles.queryBar}>
        {/* Chain toggle */}
        <div className={styles.chainToggle}>
          <button
            className={`${styles.chainBtn} ${chain === "l1" ? styles.chainBtnActive : ""}`}
            onClick={() => handleChainSwitch("l1")}
          >
            L1
          </button>
          <button
            className={`${styles.chainBtn} ${chain === "l2" ? styles.chainBtnActiveL2 : ""}`}
            onClick={() => handleChainSwitch("l2")}
          >
            L2
          </button>
        </div>

        {/* Navigation group: arrows + input */}
        <div className={styles.navGroup}>
          <button className={styles.navArrow} onClick={handlePrev} disabled={currentBlockNumber == null || currentBlockNumber <= 0 || loading} title="Previous block (Left Arrow)">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
              <polyline points="15 18 9 12 15 6" />
            </svg>
          </button>
          <input
            type="text"
            className={styles.blockInput}
            value={blockInput}
            onChange={(e) => setBlockInput(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder={chain === "l1" ? "L1 Block #" : "L2 Block #"}
          />
          <button className={styles.navArrow} onClick={handleNext} disabled={currentBlockNumber == null || loading} title="Next block (Right Arrow)">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
              <polyline points="9 18 15 12 9 6" />
            </svg>
          </button>
        </div>

        {/* Action group: fetch + latest */}
        <div className={styles.actionGroup}>
          <button className={`${styles.actionBtn} ${styles.actionBtnPrimary}`} onClick={handleFetch} disabled={loading || !blockInput.trim()}>
            {loading ? <span className={styles.actionSpinner} /> : "Fetch"}
          </button>
          <button className={styles.actionBtn} onClick={handleLatest} disabled={loading}>
            Latest
          </button>
        </div>
      </div>

      {/* Error */}
      {error && (
        <div className={styles.errorBanner}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" style={{ flexShrink: 0 }}>
            <circle cx="12" cy="12" r="10" /><line x1="15" y1="9" x2="9" y2="15" /><line x1="9" y1="9" x2="15" y2="15" />
          </svg>
          {error}
        </div>
      )}

      {/* Loading */}
      {loading && (
        <div className={styles.loadingState}>
          <span className={styles.spinnerLg} />
          <span>Fetching block data{blockInput ? ` for ${chain === "l2" ? "L2" : "L1"} block ${blockInput}` : ""}...</span>
        </div>
      )}

      {/* Block content — same dual-lane view for both L1 and L2 entry */}
      {!loading && decodedBlock && (
        <>
          {/* Block header — L1 info | timestamp divider | L2 info */}
          <div className={styles.blockHeader}>
            {/* L1 section — right-aligned toward center */}
            <div className={styles.blockHeaderSectionL1}>
              <span className={`${styles.blockStat} ${styles.l1StatHighlight}`}>
                <span className={styles.l1BadgeInline}>L1</span>
                <ExplorerLink
                  value={decodedBlock.blockNumber.toString()}
                  type="block"
                  chain="l1"
                  short={false}
                  label={`#${decodedBlock.blockNumber.toLocaleString()}`}
                  className={styles.l1BlockLink}
                />
              </span>
              <span className={`${styles.blockStat} ${styles.l1StatHighlight}`}>{decodedBlock.totalLogs} logs</span>
              <span className={`${styles.blockStat} ${styles.l1StatHighlight}`}>
                {decodedBlock.txs.length} tx{decodedBlock.txs.length !== 1 ? "s" : ""}
              </span>
              {decodedBlock.allBatchEntries.length > 0 && (
                <span className={`${styles.blockStat} ${styles.l1StatHighlight}`}>{decodedBlock.allBatchEntries.length} entries</span>
              )}
            </div>

            {/* Timestamp divider — locked to geometric center */}
            <div className={styles.blockHeaderDivider}>
              <div className={styles.blockHeaderDividerLine} />
              <span className={styles.blockHeaderDividerText}>
                {decodedBlock.timestamp != null
                  ? new Date(decodedBlock.timestamp * 1000).toLocaleString()
                  : "\u2014"}
              </span>
              <div className={styles.blockHeaderDividerLine} />
            </div>

            {/* L2 section — left-aligned away from center */}
            <div className={styles.blockHeaderSectionL2}>
              {l2BlockCount > 0 ? (
                <>
                  {decodedBlock.l2Blocks.map((b) => (
                    <span key={b.number} className={`${styles.blockStat} ${styles.l2StatHighlight}`}>
                      <span className={styles.l2BadgeInline}>L2</span>
                      <ExplorerLink
                        value={b.number.toString()}
                        type="block"
                        chain="l2"
                        short={false}
                        label={`#${b.number.toLocaleString()}`}
                        className={styles.l2BlockLink}
                      />
                    </span>
                  ))}
                  <span className={`${styles.blockStat} ${styles.l2StatHighlight}`}>
                    {decodedBlock.l2Blocks.reduce((s, b) => s + (b.txCount >= 0 ? b.txCount : 0), 0)} txs
                  </span>
                  <span className={`${styles.blockStat} ${styles.l2StatHighlight}`}>
                    {decodedBlock.l2Blocks.reduce((s, b) => s + b.transactions.filter(t => t.isProtocol).length, 0)} protocol
                  </span>
                  <span className={`${styles.blockStat} ${styles.l2StatHighlight}`}>
                    {decodedBlock.l2Blocks.reduce((s, b) => s + b.transactions.filter(t => !t.isProtocol).length, 0)} user
                  </span>
                  {(() => {
                    const totalGas = decodedBlock.l2Blocks.reduce((s, b) => s + b.gasUsed, 0);
                    if (totalGas <= 0) return null;
                    const fmt = totalGas >= 1_000_000 ? (totalGas / 1_000_000).toFixed(1) + "M" : totalGas >= 1_000 ? (totalGas / 1_000).toFixed(0) + "K" : totalGas.toString();
                    return <span className={`${styles.blockStat} ${styles.l2StatHighlight}`}>{fmt} gas</span>;
                  })()}
                </>
              ) : (
                <span className={styles.blockStat}>no L2 blocks</span>
              )}
            </div>
          </div>

          {/* Flow diagram */}
          <BlockFlowDiagram block={decodedBlock} />

          {/* Keyboard hint */}
          <div className={styles.keyboardHint}>
            {"\u2190"}/{"\u2192"} navigate {chain === "l2" ? "L2" : "L1"} blocks
          </div>

          {/* Empty block */}
          {decodedBlock.txs.length === 0 && !hasL2Blocks && (
            <div className={styles.emptyState}>
              <div className={styles.emptyIcon}>
                <svg width="44" height="44" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1" strokeLinecap="round" strokeLinejoin="round">
                  <rect x="3" y="3" width="18" height="18" rx="2" /><line x1="9" y1="9" x2="15" y2="15" /><line x1="15" y1="9" x2="9" y2="15" />
                </svg>
              </div>
              <div className={styles.emptyTitle}>
                {chain === "l2"
                  ? `No Rollups events in L1 block for L2 block ${l2EntryBlock?.toLocaleString() ?? blockInput}`
                  : `No Rollups events in block ${decodedBlock.blockNumber.toLocaleString()}`}
              </div>
              <div className={styles.emptySub}>
                Try clicking "Latest" to find the most recent block with events, or use {"\u2190"}/{"\u2192"} to browse nearby blocks.
              </div>
            </div>
          )}

          {/* Dual-lane layout */}
          {(decodedBlock.txs.length > 0 || hasL2Blocks) && (
            <div className={styles.dualLane}>
              {/* L1 Lane */}
              <div className={styles.l1Lane}>
                <div className={styles.laneHeader}>
                  <span className={`${styles.laneBadge} ${styles.l1Badge}`}>L1</span>
                  <span className={styles.laneTitle}>
                    {decodedBlock.txs.length} transaction{decodedBlock.txs.length !== 1 ? "s" : ""}
                  </span>
                </div>
                {decodedBlock.txs.map((tx, i) => (
                  <TxCard key={tx.txHash} tx={tx} index={i} />
                ))}
              </div>

              {/* Connector strip */}
              <div className={styles.laneConnector}>
                <div className={styles.connectorLine} />
                {hasL2Blocks && (
                  <span className={styles.connectorLabel}>postBatch</span>
                )}
              </div>

              {/* L2 Lane */}
              <div className={styles.l2Lane}>
                <div className={styles.laneHeader}>
                  <span className={`${styles.laneBadge} ${styles.l2LaneBadge}`}>L2</span>
                  <span className={styles.laneTitle}>
                    {hasL2Blocks
                      ? `${l2BlockCount} block${l2BlockCount !== 1 ? "s" : ""} submitted`
                      : "No L2 blocks submitted"}
                  </span>
                </div>
                {hasL2Blocks ? (
                  decodedBlock.l2Blocks.map((block) => (
                    <L2BlockCard key={block.number} block={block} />
                  ))
                ) : (
                  <div className={styles.l2Empty}>
                    {decodedBlock.txs.some((tx) => tx.batchEntries.length > 0)
                      ? "Cross-chain entries only \u2014 no block data in callData"
                      : "No L2 blocks submitted in this L1 block"}
                  </div>
                )}
              </div>
            </div>
          )}
        </>
      )}

      {/* Initial empty state */}
      {!loading && !decodedBlock && !error && (
        <div className={styles.emptyState}>
          <div className={styles.emptyIcon}>
            <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1" strokeLinecap="round" strokeLinejoin="round">
              <rect x="3" y="3" width="7" height="7" rx="1" /><rect x="14" y="3" width="7" height="7" rx="1" />
              <rect x="14" y="14" width="7" height="7" rx="1" /><rect x="3" y="14" width="7" height="7" rx="1" />
            </svg>
          </div>
          <div className={styles.emptyTitle}>Block Explorer</div>
          <div className={styles.emptySub}>
            {chain === "l1"
              ? "Enter a block number or click \"Latest\" to decode Rollups.sol events."
              : "Enter an L2 block number or click \"Latest\" to view the corresponding L1+L2 block data."}
          </div>
        </div>
      )}
    </div>
  );
}
