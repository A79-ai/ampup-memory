import "./advice-page.css";
import { useChoice, useOption } from "../memory/hashOptions";

// What every total in Advice is counted over, shared by its Advice and Serving tabs: one window (`days` in the link:
// 7, 30, 90 or all; 30 when it names none) and one set of notes (the team's and the reader's own, or for the org's
// admins one person's own in the reader's place: `person` in the link), shown all at once or only the reader's own or
// the team's (`show`: the All / Mine / Team pill above both tabs). Moving between the tabs keeps all three. The
// platform's operator may also count every person's own notes at once on Served (`person=*`); Learned then counts the
// reader's own.

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
export const SCOPE_OPTIONS = ["days", "person", "show"] as const;

/** `person` naming every person's own notes at once (`person=*` in the link): the platform operator's view. */
export const EVERYONE = "*";

/**
 * The window and whose own notes are counted, as the link says. `person` is honoured for the org's admins only (the
 * server refuses anyone else naming another person), and everyone's (`*`) for the platform's operator only
 * (`everyone`; the server refuses anyone else); empty, the reader's own.
 */
export function useAdviceScope(admin: boolean, everyone = false) {
  const [range, setRange] = useChoice("days", RANGE_IDS, "30");
  const [picked, setPerson] = useOption("person");
  const days = (RANGES.find((r) => r.id === range) ?? RANGES[1]).days;
  const person = picked === EVERYONE ? (everyone ? EVERYONE : "") : admin ? picked : "";
  return { range, setRange, days, person, setPerson };
}

/** Whose own notes Served counts beside the team's, for the platform's operator: theirs, or every person's. */
export function WhoseSeg({ person, setPerson }: { person: string; setPerson: (p: string | null) => void }) {
  return (
    <div className="ap-seg" role="group" aria-label="Whose own notes">
      <button aria-pressed={!person} className={!person ? "on" : ""} onClick={() => setPerson(null)}>
        Yours
      </button>
      <button
        aria-pressed={person === EVERYONE}
        className={person === EVERYONE ? "on" : ""}
        onClick={() => setPerson(EVERYONE)}
        title="Every person's own notes in this org, each with whose it is (the operator's view)"
      >
        Everyone&apos;s
      </button>
    </div>
  );
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
  const whose =
    person === EVERYONE
      ? "the team's notes and every person's own"
      : person
        ? `the team's notes and ${person}'s`
        : "the team's notes and yours";
  const span = days ? `${what} in the last ${days} days` : `every one of their ${what}`;
  return `Counting ${whose}, ${span}`;
}
