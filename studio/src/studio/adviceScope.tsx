import "./advice-page.css";
import { useEffect, useState } from "react";
import { call } from "../memory/api";
import { useChoice, useOption } from "../memory/hashOptions";
import type { Selection } from "../memory/types";

// What every total in Advice is counted over, shared by its Served and Learned tabs: one window (`days` in the link:
// 7, 30, 90 or all; 30 when it names none) and one set of notes (the team's and the reader's own, or for the org's
// admins one person's own in the reader's place: `person` in the link). Moving between the tabs keeps both.

/** The windows, as the link names them (`#/advice?days=7`; 30 days when it names none). */
export const RANGES = [
  { id: "7", days: 7, label: "7 days" },
  { id: "30", days: 30, label: "30 days" },
  { id: "90", days: 90, label: "90 days" },
  { id: "all", days: null, label: "All" },
] as const;
const RANGE_IDS = RANGES.map((r) => r.id);
type RangeId = (typeof RANGES)[number]["id"];

/** The link's options Advice's tabs carry from one to the other. */
export const SCOPE_OPTIONS = ["days", "person"] as const;

/**
 * The window and whose own notes are counted, as the link says. `person` is honoured for the org's admins only (the
 * server refuses anyone else naming another person); empty, the reader's own.
 */
export function useAdviceScope(admin: boolean) {
  const [range, setRange] = useChoice("days", RANGE_IDS, "30");
  const [picked, setPerson] = useOption("person");
  const days = (RANGES.find((r) => r.id === range) ?? RANGES[1]).days;
  return { range, setRange, days, person: admin ? picked : "", setPerson };
}

/** The window's segmented control. */
export function RangeSeg({
  range,
  setRange,
}: {
  range: RangeId;
  setRange: (r: RangeId) => void;
}) {
  return (
    <div className="ap-seg" role="group" aria-label="Time range">
      {RANGES.map((r) => (
        <button
          key={r.id}
          aria-pressed={range === r.id}
          className={range === r.id ? "on" : ""}
          onClick={() => setRange(r.id)}
        >
          {r.label}
        </button>
      ))}
    </div>
  );
}

/**
 * What a tab counts, in words: whose notes, and over which runs or sessions. `days` is the window the server says it
 * applied (null: all of them); `what` names what the window cuts ("sessions", "runs and sessions").
 */
export function scopeText(person: string, days: number | null, what: string): string {
  const whose = person ? `the team's notes and ${person}'s` : "the team's notes and yours";
  const span = days ? `${what} in the last ${days} days` : `every one of their ${what}`;
  return `Counting ${whose}, ${span}`;
}

/**
 * For the org's admins: whose own notes Advice counts beside the team's, one chip per person who has notes of their
 * own (as `/lessons` knows them). Nothing for anyone else, or where no one else has notes of their own.
 */
export function PersonPicker({ sel, admin }: { sel: Selection; admin: boolean }) {
  const { person, setPerson } = useAdviceScope(admin);
  const [got, setGot] = useState<{
    key: string;
    me: string;
    everyone: boolean;
    people: string[];
  } | null>(null);
  const key = JSON.stringify([sel.org, sel.team, sel.branch]);
  useEffect(() => {
    if (!admin) return;
    let live = true;
    call<{ reader: { user: string; everyone: boolean }; people: string[] }>(sel, "lessons", {
      count: true,
    }).then(
      (v) =>
        live &&
        setGot({ key, me: v.reader.user, everyone: v.reader.everyone, people: v.people ?? [] }),
      () => live && setGot({ key, me: "", everyone: false, people: [] }),
    );
    return () => {
      live = false;
    };
  }, [key, admin]);
  const cur = got?.key === key ? got : null;
  if (!admin || !cur?.everyone) return null;
  const same = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();
  const others = cur.people.filter((p) => !same(p, cur.me));
  if (!others.length && !person) return null;
  return (
    <nav className="ap-chips ap-person" aria-label="Whose own notes">
      <span className="ap-muted ap-small">Beside the team&apos;s:</span>
      <button className={!person ? "on" : ""} aria-pressed={!person} onClick={() => setPerson(null)}>
        Yours
      </button>
      {others.map((p) => (
        <button
          key={p}
          className={same(person, p) ? "on" : ""}
          aria-pressed={same(person, p)}
          onClick={() => setPerson(p)}
        >
          {p}
        </button>
      ))}
    </nav>
  );
}
