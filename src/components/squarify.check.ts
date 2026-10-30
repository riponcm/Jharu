// Self-check for squarify(). No test runner in this project, so run it with
// Node (22.6+ strips the types itself):
//   node src/components/squarify.check.ts
import { squarify } from "./squarify.ts";

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`squarify check failed: ${msg}`);
}

const EPS = 1e-6;

function run(name: string, values: number[], w: number, h: number) {
  const placed = squarify(values.map((v, i) => ({ value: v, item: i })), { x: 0, y: 0, w, h });
  const positive = values.filter((v) => v > 0 && Number.isFinite(v));
  check(placed.length === positive.length, `${name}: every positive item placed`);

  const total = positive.reduce((a, b) => a + b, 0);
  let areaSum = 0;
  for (const p of placed) {
    check(p.w > 0 && p.h > 0, `${name}: positive size`);
    check(p.x >= -EPS && p.y >= -EPS, `${name}: inside top-left`);
    check(p.x + p.w <= w + EPS && p.y + p.h <= h + EPS, `${name}: inside bottom-right`);
    check(
      Math.abs((p.w * p.h) / (w * h) - p.value / total) < 1e-9,
      `${name}: area proportional to value`
    );
    areaSum += p.w * p.h;
  }
  check(Math.abs(areaSum - w * h) < 1e-6 * w * h, `${name}: tiles the whole box`);

  for (let i = 0; i < placed.length; i++) {
    for (let j = i + 1; j < placed.length; j++) {
      const a = placed[i];
      const b = placed[j];
      const overlapW = Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x);
      const overlapH = Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y);
      check(!(overlapW > 1e-6 && overlapH > 1e-6), `${name}: ${i} and ${j} do not overlap`);
    }
  }
  return placed;
}

// The worked example from the original paper.
const paper = run("paper", [6, 6, 4, 3, 2, 2, 1], 600, 400);
check(paper[0].value === 6, "largest comes first");

run("single", [42], 1000, 380);
run("equal", [5, 5, 5, 5], 800, 400);
run("skewed", [1e12, 5e9, 3e9, 1e9, 1e6, 1e3], 1000, 380);
run("tall box", [3, 2, 2, 1], 100, 900);
run("ignores junk", [10, 0, -5, NaN, 4, Infinity], 500, 300);
check(squarify([], { x: 0, y: 0, w: 100, h: 100 }).length === 0, "empty input");
check(squarify([{ value: 1, item: 0 }], { x: 0, y: 0, w: 0, h: 100 }).length === 0, "empty box");

// Squarified layouts keep blocks readable: for a spread of similar sizes the
// worst aspect ratio stays small.
const many = run("many", Array.from({ length: 30 }, (_, i) => 100 - i * 2), 1000, 380);
const worstRatio = Math.max(...many.map((p) => Math.max(p.w / p.h, p.h / p.w)));
check(worstRatio < 4, `many: worst aspect ratio ${worstRatio.toFixed(2)} stays under 4`);

console.log("squarify: all checks passed");
