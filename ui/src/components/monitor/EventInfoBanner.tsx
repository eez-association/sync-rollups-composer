import React from "react";
import { useMonitorStore } from "../../store";
import styles from "./EventInfoBanner.module.css";

export const EventInfoBanner: React.FC = () => {
  const events = useMonitorStore((s) => s.events);
  const selectedEventId = useMonitorStore((s) => s.selectedEventId);

  const isLive = selectedEventId === null;
  const selectedIdx = isLive
    ? events.length - 1
    : events.findIndex((e) => e.id === selectedEventId);

  if (events.length === 0) return null;

  const event = events[selectedIdx];
  if (!event) return null;

  const chainColor = event.chain === "l1" ? "#3b82f6" : "#a855f7";

  return (
    <div className={styles.banner}>
      <h2 className={styles.step}>
        Step {selectedIdx + 1} of {events.length}
        {isLive && <span className={styles.liveBadge}>LIVE</span>}
      </h2>
      <div className={styles.eventLine}>
        <span className={styles.chainLabel} style={{ color: chainColor }}>
          [{event.chain.toUpperCase()}]
        </span>{" "}
        {event.eventName}
      </div>
      <div className={styles.meta}>
        Block {event.blockNumber.toString()} · Tx {event.transactionHash.slice(0, 10)}...
        {" · "}Use arrow keys to navigate
      </div>
    </div>
  );
};
