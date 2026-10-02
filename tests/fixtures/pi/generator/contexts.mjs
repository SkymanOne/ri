// Writes pi's model context for each committed session fixture to ../contexts.
// Run after generate.mjs: `node contexts.mjs`. Deterministic for given fixtures.
import { mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import * as pi from "@earendil-works/pi-coding-agent";

const fixtures = join(dirname(fileURLToPath(import.meta.url)), "..");
const out = join(fixtures, "contexts");
mkdirSync(out, { recursive: true });
for (const file of readdirSync(join(fixtures, "sessions")).sort()) {
	const entries = pi.parseSessionEntries(readFileSync(join(fixtures, "sessions", file), "utf8"));
	pi.migrateSessionEntries(entries);
	const sessionEntries = entries.filter((entry) => entry.type !== "session");
	const context = pi.buildSessionContext(sessionEntries);
	writeFileSync(join(out, file.replace(/\.jsonl$/, ".json")), `${JSON.stringify(context, null, 2)}\n`);
}
console.log(`Wrote contexts for ${readdirSync(out).length} sessions`);
