export interface Day {
  date: string;
  count: number;
  level: number;
}

export interface DifficultyCounts {
  all: number;
  easy: number;
  medium: number;
  hard: number;
}

const active = (d: Day) => d.count > 0 || d.level > 0;

const DAY_MS = 86_400_000;
/** Days since 1970 for a calendar date, independent of time zones. */
const dayNumber = (date: string) =>
  Math.round(
    Date.UTC(+date.slice(0, 4), +date.slice(5, 7) - 1, +date.slice(8, 10)) /
      DAY_MS,
  );
const fromNumber = (n: number) =>
  new Date(n * DAY_MS).toISOString().slice(0, 10);

export const shade = (level: number) => {
  if (level <= 0) {
    return "var(--color-fill-2)";
  } else if (level > 0) {
    return `color-mix(in srgb, var(--color-orange) ${
      [0, 32, 54, 76, 100][Math.min(4, Math.round(level))]
    }%, transparent)`;
  }
};

export function total(days: Day[]): number {
  return days.reduce((sum, d) => sum + d.count, 0);
}

export function todayCount(days: Day[], today: string): number {
  return days.find((d) => d.date === today)?.count ?? 0;
}

export function streaks(
  days: Day[],
  today: string,
): { current: number; longest: number } {
  const on = new Set(days.filter(active).map((d) => dayNumber(d.date)));
  let longest = 0;
  for (const n of on) {
    if (on.has(n - 1)) continue;
    let len = 1;
    while (on.has(n + len)) len++;
    longest = Math.max(longest, len);
  }
  let n = dayNumber(today);
  if (!on.has(n)) n--;
  let current = 0;
  while (on.has(n - current)) current++;
  return { current, longest };
}

export function weekColumns(
  days: Day[],
  today: string,
  weeks: number,
): (Day | null)[][] {
  const byDate = new Map(days.map((d) => [d.date, d]));
  const end = dayNumber(today);
  // 1970-01-01 was a Thursday: weekday 0 = Sunday.
  const sunday = end - ((end + 4) % 7);
  const first = sunday - (weeks - 1) * 7;
  return Array.from({ length: weeks }, (_, w) =>
    Array.from({ length: 7 }, (_, d) => {
      const n = first + w * 7 + d;
      if (n > end) return null;
      const date = fromNumber(n);
      return byDate.get(date) ?? { date, count: 0, level: 0 };
    }),
  );
}
