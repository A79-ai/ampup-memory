import { useEffect, useRef } from "react";

/**
 * Calls `tick` every `every` ms while `on`, gently on a server that is struggling:
 *
 * - never while the last call is still out: the next one is timed from when the last answered;
 * - not while the page is hidden (another tab, a minimised window), and once at once when it shows again;
 * - after a failure, less often: the wait doubles with each failure in a row, up to `max`, and is back to `every`
 *   once a call answers.
 *
 * It starts again when `on` or `deps` change.
 */
export function usePoll(
  on: boolean,
  every: number,
  tick: () => Promise<unknown>,
  deps: readonly unknown[] = [],
  max = 60_000,
) {
  // The latest `tick`, so a re-render's doesn't restart the timer.
  const latest = useRef(tick);
  latest.current = tick;
  useEffect(() => {
    if (!on) return;
    let live = true;
    let out = false;
    let fails = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const hidden = () => document.hidden;
    const next = () => {
      clearTimeout(timer);
      timer = setTimeout(run, Math.min(every * 2 ** fails, max));
    };
    const run = async () => {
      timer = undefined;
      // Hidden: nothing until it shows again (`shown`).
      if (!live || out || hidden()) return;
      out = true;
      try {
        await latest.current();
        fails = 0;
      } catch {
        fails++;
      } finally {
        out = false;
      }
      if (live && !hidden()) next();
    };
    const shown = () => {
      if (live && !hidden() && !out) {
        clearTimeout(timer);
        void run();
      }
    };
    document.addEventListener("visibilitychange", shown);
    next();
    return () => {
      live = false;
      clearTimeout(timer);
      document.removeEventListener("visibilitychange", shown);
    };
  }, [on, every, max, ...deps]);
}
