/**
 * ExecutionFlow — vertical chain visualization matching forge script format.
 * Shows consumed -> next -> END flow for cross-chain execution.
 * Full addresses with explorer links, full data, no truncation.
 */

import type { FlowStep } from "../../lib/blockLogDecoder";
import type { Action } from "../../types/chain";
import { actionTypeName } from "../../lib/actionFormatter";
import { ExplorerLink } from "../ExplorerLink";
import styles from "./BlockExplorer.module.css";

const KNOWN_SELECTORS: Record<string, string> = {
  // User contracts
  "0xd09de08a": "increment()",
  "0x06661abd": "counter()",
  "0x5a6a9e05": "targetCounter()",
  "0x2bf21647": "incrementProxy()",
  // L2Context
  "0xe9d68d7b": "setContext()",
  // CrossChainManagerL2
  "0x96609ad5": "loadExecutionTable()",
  "0x0f64c845": "executeIncomingCrossChainCall()",
  "0x9af53259": "executeCrossChainCall()",
  "0x92f6fe4a": "newScope()",
  // CrossChainProxy
  "0x532f0839": "executeOnBehalf()",
};

function selectorName(data: string): string | null {
  if (!data || data.length < 10) return null;
  return KNOWN_SELECTORS[data.slice(0, 10).toLowerCase()] ?? null;
}

const ZERO_ADDR = "0x" + "0".repeat(40);

/** Renders an action as JSX with explorer links on addresses */
function ActionDisplay({ action }: { action: Action }) {
  const type = actionTypeName(action.actionType);
  const fn = selectorName(action.data) ?? (action.data.length >= 10 ? action.data.slice(0, 10) : "()");

  if (action.actionType === 0) {
    return (
      <span className={styles.flowAction}>
        {type}(r{action.rollupId.toString()},{" "}
        <ExplorerLink value={action.destination} chain="l2" short={false} className={styles.inlineLink} />
        .{fn}
        {action.sourceAddress !== ZERO_ADDR && (
          <>, from <ExplorerLink value={action.sourceAddress} chain="l1" short={false} className={styles.inlineLink} /></>
        )}
        )
      </span>
    );
  }
  if (action.actionType === 1) {
    const ok = action.failed ? "fail" : "ok";
    return (
      <span className={styles.flowAction}>
        {type}(r{action.rollupId.toString()}, {ok}{action.data !== "0x" ? `, data=${action.data}` : ""})
      </span>
    );
  }
  if (action.actionType === 2) {
    return (
      <span className={styles.flowAction}>
        {type}(r{action.rollupId.toString()})
      </span>
    );
  }
  return (
    <span className={styles.flowAction}>
      {type}(r{action.rollupId.toString()},{" "}
      <ExplorerLink value={action.destination} chain="l2" short={false} className={styles.inlineLink} />)
    </span>
  );
}

/** Renders state deltas inline: [r1:0x...->0x...] */
function DeltasDisplay({ step }: { step: FlowStep }) {
  if (!step.matchedEntry) return null;
  const deltas = step.matchedEntry.stateDeltas;
  if (deltas.length === 0) return null;

  return (
    <span className={styles.flowDeltas}>
      {" "}
      {deltas.map((d, i) => (
        <span key={i}>
          [r{d.rollupId.toString()}:{d.currentState}{"\u2192"}{d.newState}]
          {i < deltas.length - 1 ? " " : ""}
        </span>
      ))}
    </span>
  );
}

interface Props {
  flowSteps: FlowStep[];
}

export function ExecutionFlow({ flowSteps }: Props) {
  if (flowSteps.length === 0) return null;

  return (
    <div className={styles.flowChain}>
      {flowSteps.map((step, i) => {
        const hasNext = step.matchedEntry != null;
        const nextIsReal = hasNext && (
          step.matchedEntry!.nextAction.actionType !== 0 ||
          step.matchedEntry!.nextAction.destination !== ZERO_ADDR
        );

        return (
          <div key={i} className={styles.flowNode}>
            <div className={styles.flowConnector}>|</div>
            <div className={styles.flowStep}>
              <span className={styles.flowPrefix}>+-- </span>
              <span className={styles.flowConsumedLabel}>[consumed] </span>
              <ActionDisplay action={step.consumed} />
              <DeltasDisplay step={step} />
            </div>
            {nextIsReal && (
              <div className={styles.flowNext}>
                <div className={styles.flowConnector}>|     </div>
                <span className={styles.flowPrefix}>{"next -> "}</span>
                <ActionDisplay action={step.matchedEntry!.nextAction} />
              </div>
            )}
          </div>
        );
      })}
      <div className={styles.flowEnd}>
        <div className={styles.flowConnector}>|</div>
        <span className={styles.flowPrefix}>+-- </span>
        <span className={styles.flowEndLabel}>END</span>
      </div>
    </div>
  );
}
