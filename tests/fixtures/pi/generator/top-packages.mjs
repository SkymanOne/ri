// Lists the most-downloaded npm packages with the `pi-package` keyword, the
// keyword that puts a package in pi's package gallery.
//
//   node top-packages.mjs 500 > ../packages/top500.json
//
// npm's search, ranked by popularity, returns its first few thousand matches
// with their downloads over the last month. They are sorted by those
// downloads, and each entry pins the version that was latest.

const count = Number(process.argv[2] ?? 500);

async function json(url) {
	for (let attempt = 0; ; attempt++) {
		const response = await fetch(url, { signal: AbortSignal.timeout(30_000) }).catch((error) => ({ ok: false, status: String(error) }));
		if (response.ok) return response.json();
		if (attempt === 5) throw new Error(`${url}: ${response.status}`);
		process.stderr.write(`retrying ${url}: ${response.status}\n`);
		await new Promise((resolve) => setTimeout(resolve, 2000 * 2 ** attempt));
	}
}

// The search serves at most a few thousand results and then repeats them, so
// paging stops once a page brings nothing new.
const found = new Map();
for (let from = 0; ; from += 250) {
	const page = await json(
		`https://registry.npmjs.org/-/v1/search?text=keywords:pi-package&size=250&from=${from}&popularity=1.0&quality=0.0&maintenance=0.0`,
	);
	const before = found.size;
	for (const { package: pkg, downloads } of page?.objects ?? []) {
		found.set(pkg.name, { name: pkg.name, version: pkg.version, monthlyDownloads: downloads?.monthly ?? 0 });
	}
	process.stderr.write(`search: ${found.size} of ${page?.total}\n`);
	if (found.size === before) break;
}

const top = [...found.values()]
	.sort((a, b) => b.monthlyDownloads - a.monthlyDownloads || a.name.localeCompare(b.name))
	.slice(0, count);
process.stdout.write(`${JSON.stringify(top, null, "\t")}\n`);
