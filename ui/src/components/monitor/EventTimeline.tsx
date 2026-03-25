import React, { useRef, useEffect, useCallback, useMemo, useState } from "react";
import { useMonitorStore } from "../../store";
import { EventCard } from "./EventCard";
import { BundleList } from "./BundleList";
import { findCorrelatedPairs } from "../../lib/crossChainCorrelation";
import type { TransactionBundle } from "../../types/visualization";
import styles from "./EventTimeline.module.css";

type ViewMode = "events" | "bundles";

type Props = {
  onSelectBundle?: (bundle: TransactionBundle) => void;
};

export const EventTimeline: React.FC<Props> = ({ onSelectBundle }) => {
  const events = useMonitorStore((s) => s.events);
  const selectedEventId = useMonitorStore((s) => s.selectedEventId);
  const setSelectedEventId = useMonitorStore((s) => s.setSelectedEventId);
  const scrollRef = useRef<HTMLDivElement>(null);
  const [viewMode, setViewMode] = useState<ViewMode>("events");
  const [selectedBundleId, setSelectedBundleId] = useState<string | null>(null);

  const correlationMap = useMemo(() => {
    const map = new Map<string, "l1" | "l2">();
    const pairs = findCorrelatedPairs(events);
    for (const pair of pairs) {
      map.set(pair.l1Event.id, "l2");
      map.set(pair.l2Event.id, "l1");
    }
    return map;
  }, [events]);

  const isLive = selectedEventId === null;
  const selectedIdx = isLive
    ? events.length - 1
    : events.findIndex((e) => e.id === selectedEventId);

  useEffect(() => {
    if (isLive && scrollRef.current && viewMode === "events") {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [events.length, isLive, viewMode]);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (viewMode !== "events") return;
      if (events.length === 0) return;
      if (e.key === "ArrowDown" || e.key === "ArrowRight") {
        e.preventDefault();
        if (selectedEventId === null) return;
        const idx = events.findIndex((ev) => ev.id === selectedEventId);
        if (idx >= 0 && idx < events.length - 1) {
          setSelectedEventId(events[idx + 1]!.id);
        } else {
          setSelectedEventId(null);
        }
      } else if (e.key === "ArrowUp" || e.key === "ArrowLeft") {
        e.preventDefault();
        if (selectedEventId === null) {
          setSelectedEventId(events[events.length - 1]!.id);
        } else {
          const idx = events.findIndex((ev) => ev.id === selectedEventId);
          if (idx > 0) {
            setSelectedEventId(events[idx - 1]!.id);
          }
        }
      } else if (e.key === "Escape") {
        e.preventDefault();
        setSelectedEventId(null);
      }
    },
    [events, selectedEventId, setSelectedEventId, viewMode],
  );

  useEffect(() => {
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [handleKeyDown]);

  const handleSelectBundle = useCallback(
    (bundle: TransactionBundle) => {
      setSelectedBundleId(bundle.id);
      onSelectBundle?.(bundle);
    },
    [onSelectBundle],
  );

  return (
    <div className={styles.root}>
      <div className={styles.header}>
        <div className={styles.toggle}>
          <button
            onClick={() => setViewMode("events")}
            className={`${styles.toggleBtn} ${viewMode === "events" ? styles.toggleActive : styles.toggleInactive}`}
          >
            Events ({events.length})
          </button>
          <button
            onClick={() => setViewMode("bundles")}
            className={`${styles.toggleBtn} ${viewMode === "bundles" ? styles.toggleActive : styles.toggleInactive}`}
          >
            Bundles
          </button>
        </div>

        <div className={styles.controls}>
          {viewMode === "events" && !isLive && (
            <button
              onClick={() => setSelectedEventId(null)}
              className={styles.latestBtn}
            >
              Latest
            </button>
          )}
          {viewMode === "events" && isLive && (
            <span className={styles.liveBadge}>LIVE</span>
          )}
        </div>
      </div>

      <div
        ref={scrollRef}
        className={styles.content}
        style={{ padding: viewMode === "events" ? 6 : 0 }}
      >
        {viewMode === "events" ? (
          events.length === 0 ? (
            <div className={styles.emptyMsg}>Waiting for events...</div>
          ) : (
            events.map((event, i) => (
              <EventCard
                key={event.id}
                event={event}
                stepNumber={i + 1}
                selected={
                  isLive
                    ? event === events[events.length - 1]
                    : event.id === selectedEventId
                }
                isPlayed={i < selectedIdx}
                onClick={() => setSelectedEventId(event.id)}
                correlatedChain={correlationMap.get(event.id)}
              />
            ))
          )
        ) : (
          <BundleList
            onSelectBundle={handleSelectBundle}
            selectedBundleId={selectedBundleId}
          />
        )}
      </div>
    </div>
  );
};
