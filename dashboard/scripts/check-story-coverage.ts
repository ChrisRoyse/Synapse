import { readFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import { primitiveCoverage, viewStateCoverage, visualStoryCases } from "../tests/storybook-cases";

const root = process.cwd();

function read(relativePath: string) {
  return readFileSync(path.join(root, relativePath), "utf8");
}

function walk(dir: string): string[] {
  return readdirSync(dir).flatMap((entry) => {
    const full = path.join(dir, entry);
    if (statSync(full).isDirectory()) return walk(full);
    return full;
  });
}

const primitiveSource = read("src/primitives/index.tsx");
const exportedPrimitives = [...primitiveSource.matchAll(/export function ([A-Z][A-Za-z0-9]+)/g)].map((match) => match[1]);
const packageJson = read("package.json");
const storyText = walk(path.join(root, "src"))
  .filter((file) => /\.stories\.tsx?$/.test(file))
  .map((file) => readFileSync(file, "utf8"))
  .join("\n");

const requiredUiPrimitives = ["Button", "Switch", "Tabs", "Tooltip", "StatusBadge", "FlagBadge"];
const missingPrimitiveCoverage = [...exportedPrimitives, ...requiredUiPrimitives].filter(
  (name) => !primitiveCoverage.includes(name as (typeof primitiveCoverage)[number])
);
const missingStoryMentions = primitiveCoverage.filter((name) => !storyText.includes(name));
const missingViewStates = viewStateCoverage.filter((state) => !storyText.includes(`coverage:view:${state}`));
const forbidden = [];
if (/chromatic/i.test(packageJson)) forbidden.push("Chromatic/SaaS dependency or script is forbidden");
if (!packageJson.includes("test:visual")) forbidden.push("missing test:visual script");
if (!packageJson.includes("test:a11y")) forbidden.push("missing test:a11y script");

const failures = [
  ...missingPrimitiveCoverage.map((name) => `primitive export lacks coverage entry: ${name}`),
  ...missingStoryMentions.map((name) => `primitive coverage entry has no story mention: ${name}`),
  ...missingViewStates.map((state) => `view state lacks story coverage marker: ${state}`),
  ...forbidden
];

if (failures.length) {
  throw new Error(`story coverage check failed:\n${failures.map((failure) => `- ${failure}`).join("\n")}`);
}

console.log(
  `story coverage check ok: primitives=${primitiveCoverage.length} view_states=${viewStateCoverage.length} visual_cases=${visualStoryCases.length}`
);
