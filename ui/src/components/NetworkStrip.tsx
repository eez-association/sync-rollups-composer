import { L1_CHAIN, L2_CHAIN } from "../config";
import styles from "./NetworkStrip.module.css";

interface Props {
  currentChainId: string | null;
  isConnected: boolean;
  onSwitchL1: () => void;
  onSwitchL2: () => void;
}

export function NetworkStrip({
  currentChainId,
  isConnected,
  onSwitchL1,
  onSwitchL2,
}: Props) {
  const l1Active = isConnected && currentChainId === L1_CHAIN.chainId;
  const l2Active = isConnected && currentChainId === L2_CHAIN.chainId;

  return (
    <div className={styles.strip}>
      <button
        className={`${styles.chip} ${l1Active ? styles.active : ""}`}
        onClick={onSwitchL1}
        title="Switch wallet to L1"
      >
        <span className={styles.dot} />
        L1 &middot; {L1_CHAIN.chainName}
      </button>
      <button
        className={`${styles.chip} ${l2Active ? styles.active : ""}`}
        onClick={onSwitchL2}
        title="Switch wallet to L2"
      >
        <span className={styles.dot} />
        L2 &middot; {L2_CHAIN.chainName}
      </button>
    </div>
  );
}
